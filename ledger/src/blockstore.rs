//! The `blockstore` module provides functions for parallel verification of the
//! Proof of History ledger as well as iterative read, append write, and random
//! access read to a persistent file-based ledger.

#[cfg(feature = "dev-context-only-utils")]
use trees::{Tree, TreeWalk};
use {
    crate::{
        ancestor_iterator::AncestorIterator,
        blockstore::column::{TypedColumn, columns as cf},
        blockstore_db::{
            DBPinnedT, IteratorDirection, IteratorMode, LedgerColumn, Rocks, WriteBatch,
        },
        blockstore_meta::*,
        blockstore_options::{
            BLOCKSTORE_DIRECTORY_ROCKS_LEVEL, BlockstoreOptions, LedgerColumnOptions,
        },
        next_slots_iterator::NextSlotsIterator,
        shred::{
            self, DATA_SHREDS_PER_FEC_BLOCK, ErasureSetId, Payload, ProcessShredsStats,
            ReedSolomonCache, Shred, ShredFlags, ShredId, ShredType, Shredder,
            filter::ShredRecoveryContext,
            merkle_tree::{MerkleTree, SIZE_OF_MERKLE_PROOF_ENTRY, get_proof_size},
        },
        slot_stats::{ShredSource, SlotsStats},
        transaction_address_lookup_table_scanner::scan_transaction,
    },
    agave_snapshots::unpack_genesis_archive,
    agave_votor_messages::{
        migration::MigrationStatus, unverified_vote_message::UnverifiedCertificate,
    },
    assert_matches::{assert_matches, debug_assert_matches},
    crossbeam_channel::{Receiver, Sender, TrySendError, bounded},
    dashmap::DashSet,
    itertools::Itertools,
    log::*,
    lru::LruCache,
    parking_lot::{FairMutex, FairMutexGuard},
    rand::Rng,
    rayon::iter::{IntoParallelIterator, ParallelIterator},
    rocksdb::LiveFile,
    solana_account::ReadableAccount,
    solana_address_lookup_table_interface::state::AddressLookupTable,
    solana_clock::{Slot, UnixTimestamp},
    solana_entry::{
        block_component::{
            BlockComponent, VersionedBlockHeader, VersionedBlockMarker, VersionedUpdateParent,
        },
        entry::{Entry, create_ticks},
    },
    solana_genesis_config::{DEFAULT_GENESIS_ARCHIVE, DEFAULT_GENESIS_FILE, GenesisConfig},
    solana_hash::{HASH_BYTES, Hash},
    solana_keypair::Keypair,
    solana_measure::{measure::Measure, measure_us},
    solana_metrics::datapoint_error,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, leader_schedule_utils::leader_slot_index},
    solana_sha256_hasher::hashv,
    solana_signature::Signature,
    solana_signer::Signer,
    solana_time_utils::timestamp,
    solana_transaction::{
        TransactionVerificationMode,
        versioned::{VersionedTransaction, sanitized::SanitizedVersionedTransaction},
    },
    solana_transaction_status::{
        ConfirmedTransactionStatusWithSignature, ConfirmedTransactionWithStatusMeta, EntrySummary,
        RewardsAndNumPartitions, TransactionStatusMeta, TransactionWithStatusMeta,
        VersionedConfirmedBlock, VersionedConfirmedBlockWithEntries,
        VersionedTransactionWithStatusMeta,
    },
    std::{
        borrow::Cow,
        cell::RefCell,
        cmp,
        collections::{
            BTreeMap, HashMap, HashSet, VecDeque, btree_map::Entry as BTreeMapEntry,
            hash_map::Entry as HashMapEntry,
        },
        convert::TryInto,
        fmt::Write,
        fs::{self, File},
        io::Error as IoError,
        num::NonZeroUsize,
        ops::Range,
        path::{Path, PathBuf},
        rc::Rc,
        sync::{
            Arc, Mutex, MutexGuard, OnceLock, RwLock,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
    },
    tar,
    tempfile::{Builder, TempDir},
    thiserror::Error,
    wincode::config::DefaultConfig,
};

pub mod blockstore_purge;
pub mod column;
pub mod error;
pub use {
    crate::{
        blockstore::error::{BlockstoreError, Result},
        blockstore_db::{default_num_compaction_threads, default_num_flush_threads},
        blockstore_meta::{OptimisticSlotMetaVersioned, SlotMeta},
        blockstore_metrics::{BlockstoreInsertionMetrics, BlockstoreSwitchBankMetrics},
    },
    blockstore_purge::PurgeType,
    rocksdb::properties as RocksProperties,
};

pub const MAX_REPLAY_WAKE_UP_SIGNALS: usize = 1;
pub const MAX_COMPLETED_SLOTS_IN_CHANNEL: usize = 100_000;
/// Maximum queued UpdateParent notifications from blockstore insertion to replay.
///
/// Replay also recovers by reading SlotMeta, so dropping this bounded signal is
/// a latency event rather than the only source of truth. Keep this small enough
/// that Tower validators do not pay a large idle-memory cost for the channel.
pub const MAX_UPDATE_PARENT_SIGNALS: usize = 4_096;
// Update parent events are expected to be rare, so a small cache to avoid
// hitting blockstore should suffice here. The "DoS" attack to cause cache churn
// and misses is also expensive because it would involve malicious leader giving
// up their future leader slots.
const UPDATE_PARENT_SHRED_PARENT_CACHE_CAPACITY: NonZeroUsize = NonZeroUsize::new(16).unwrap();

pub type CompletedSlotsSender = Sender<Vec<Slot>>;
pub type CompletedSlotsReceiver = Receiver<Vec<Slot>>;

pub type UpdateParentSender = Sender<UpdateParentSignal>;
pub type UpdateParentReceiver = Receiver<UpdateParentSignal>;
type UpdateParentShredParentKey = (BlockLocation, Slot, u32);
type UpdateParentShredParentCache =
    LruCache<UpdateParentShredParentKey, /* parent used for shred filtering */ Slot>;

#[derive(Debug, Clone)]
pub struct UpdateParentSignal {
    /// Slot whose parent metadata was updated by an UpdateParent marker.
    ///
    /// This is only a wakeup for replay. Consumers must re-read SlotMeta/dead
    /// state from blockstore so stale queued signals cannot override newer
    /// parent metadata or a durable dead-slot decision.
    pub slot: Slot,
}

// Contiguous, sorted and non-empty ranges of shred indices:
//     completed_ranges[i].start < completed_ranges[i].end
//     completed_ranges[i].end  == completed_ranges[i + 1].start
// The ranges represent data shred indices that can reconstruct a block component.
// In particular, the data shred at index
//     completed_ranges[i].end - 1
// has DATA_COMPLETE_SHRED flag.
type CompletedRanges = Vec<Range<u32>>;

#[derive(Default)]
pub struct SignatureInfosForAddress {
    pub infos: Vec<ConfirmedTransactionStatusWithSignature>,
    pub found_before: bool,
    pub found_until: bool,
}

#[derive(Error, Debug)]
enum InsertDataShredError {
    #[error("Data shred already exists in Blockstore")]
    Exists,
    #[error("Invalid data shred")]
    InvalidShred,
    #[error(transparent)]
    BlockstoreError(#[from] BlockstoreError),
}

#[derive(Error, Debug)]
enum InsertCodingShredError {
    #[error("Coding shred already exists in Blockstore")]
    Exists,
    #[error("Invalid coding shred")]
    InvalidShred,
    #[error("Invalid coding shred erasure config")]
    InvalidErasureConfig,
    #[error(transparent)]
    BlockstoreError(#[from] BlockstoreError),
}

#[derive(Eq, PartialEq, Debug, Clone)]
pub enum PossibleDuplicateShred {
    Exists(Shred), // Blockstore has another shred in its spot
    // The index of this shred conflicts with `slot_meta.last_index`
    LastIndexConflict(
        Shred,          // original
        shred::Payload, // conflict
    ),
    // The coding shred has a conflict in the erasure_meta
    ErasureConflict(
        Shred,          // original
        shred::Payload, // conflict
    ),
    // Merkle root conflict in the same fec set
    MerkleRootConflict(
        Shred,          // original
        shred::Payload, // conflict
    ),
    // Chained merkle root conflict with previous fec set
    // A part of SIMD-0340
    FixedFECChainedMerkleRootConflict(Slot),
}

impl PossibleDuplicateShred {
    pub fn slot(&self) -> Slot {
        match self {
            Self::Exists(shred) => shred.slot(),
            Self::LastIndexConflict(shred, _) => shred.slot(),
            Self::ErasureConflict(shred, _) => shred.slot(),
            Self::MerkleRootConflict(shred, _) => shred.slot(),
            Self::FixedFECChainedMerkleRootConflict(slot) => *slot,
        }
    }
}

enum WorkingEntry<T> {
    Dirty(T), // Value has been modified with respect to the blockstore column
    Clean(T), // Value matches what is currently in the blockstore column
}

impl<T> WorkingEntry<T> {
    fn should_write(&self) -> bool {
        matches!(self, Self::Dirty(_))
    }
}

impl<T> AsRef<T> for WorkingEntry<T> {
    fn as_ref(&self) -> &T {
        match self {
            Self::Dirty(value) => value,
            Self::Clean(value) => value,
        }
    }
}

pub struct InsertResults {
    completed_data_set_infos: Vec<CompletedDataSetInfo>,
    duplicate_shreds: Vec<PossibleDuplicateShred>,
}

#[allow(clippy::large_enum_variant)]
pub enum ConfirmedBlockComponent {
    EntryBatch(Vec<EntrySummary>),
    BlockMarker(VersionedBlockMarker),
}

pub struct VersionedConfirmedBlockWithComponents {
    pub block: VersionedConfirmedBlock,
    pub components: Vec<ConfirmedBlockComponent>,
}

/// A "complete data set" is a range of [`Shred`]s that combined in sequence carry a single
/// serialized [`Vec<Entry>`].
///
/// Services such as the `WindowService` for a TVU, and `ReplayStage` for a TPU, piece together
/// these sets by inserting shreds via direct or indirect calls to
/// [`Blockstore::insert_shreds_handle_duplicate()`].
///
/// `solana_core::completed_data_sets_service::CompletedDataSetsService` is the main receiver of
/// `CompletedDataSetInfo`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedDataSetInfo {
    /// [`Slot`] to which the [`Shred`]s in this set belong.
    pub slot: Slot,
    /// Data [`Shred`]s' indices in this set.
    pub indices: Range<u32>,
}

pub struct BlockstoreSignals {
    pub blockstore: Blockstore,
    pub ledger_signal_receiver: Receiver<bool>,
    pub completed_slots_receiver: CompletedSlotsReceiver,
    pub update_parent_receiver: UpdateParentReceiver,
}

struct SwitchBlockLock(FairMutex<()>);

struct SwitchBlockGuard<'a> {
    _guard: FairMutexGuard<'a, ()>,
}

impl SwitchBlockLock {
    fn lock(&self) -> SwitchBlockGuard<'_> {
        SwitchBlockGuard {
            _guard: self.0.lock(),
        }
    }
}

// ledger window
pub struct Blockstore {
    ledger_path: PathBuf,
    db: Arc<Rocks>,

    // Shred insertion column families
    data_shred_cf: LedgerColumn<cf::ShredData>,
    code_shred_cf: LedgerColumn<cf::ShredCode>,
    meta_cf: LedgerColumn<cf::SlotMeta>,
    index_cf: LedgerColumn<cf::Index>,
    erasure_meta_cf: LedgerColumn<cf::ErasureMeta>,
    merkle_root_meta_cf: LedgerColumn<cf::MerkleRootMeta>,
    double_merkle_meta_cf: LedgerColumn<cf::DoubleMerkleMeta>,
    orphans_cf: LedgerColumn<cf::Orphans>,
    duplicate_slots_cf: LedgerColumn<cf::DuplicateSlots>,

    // Shred insertion column families for handling Alpenglow alternate blocks
    alt_data_shred_cf: LedgerColumn<cf::AlternateShredData>,
    alt_meta_cf: LedgerColumn<cf::AlternateSlotMeta>,
    alt_index_cf: LedgerColumn<cf::AlternateIndex>,
    alt_merkle_root_meta_cf: LedgerColumn<cf::AlternateMerkleRootMeta>,

    // Block status column families
    bank_hash_cf: LedgerColumn<cf::BankHash>,
    optimistic_slots_cf: LedgerColumn<cf::OptimisticSlots>,
    roots_cf: LedgerColumn<cf::Root>,
    dead_slots_cf: LedgerColumn<cf::DeadSlots>,

    // Block and transaction metadata column families (for RPC)
    block_height_cf: LedgerColumn<cf::BlockHeight>,
    blocktime_cf: LedgerColumn<cf::Blocktime>,
    rewards_cf: LedgerColumn<cf::Rewards>,
    transaction_status_cf: LedgerColumn<cf::TransactionStatus>,
    transaction_memos_cf: LedgerColumn<cf::TransactionMemos>,
    address_signatures_cf: LedgerColumn<cf::AddressSignatures>,
    perf_samples_cf: LedgerColumn<cf::PerfSamples>,

    max_root: AtomicU64,
    insert_shreds_lock: Mutex<()>,
    switch_block_lock: SwitchBlockLock,
    new_shreds_signals: Mutex<Vec<Sender<bool>>>,
    completed_slots_senders: Mutex<Vec<CompletedSlotsSender>>,
    update_parent_signals: Mutex<Vec<UpdateParentSender>>,
    /// Stable shred-header parent for recent UpdateParent slots.
    ///
    /// After UpdateParent, `SlotMeta.parent_slot` contains the effective replay
    /// parent, but all shreds in the slot must still use the original shred
    /// parent. This tiny cache avoids a blockstore lookup for each later shred
    /// in small insertion batches.
    update_parent_shred_parent_cache: Mutex<UpdateParentShredParentCache>,
    certificate_sender: OnceLock<Sender<(Slot, UnverifiedCertificate)>>,
    pub lowest_cleanup_slot: RwLock<Slot>,
    // A sender that feeds into the BlockstoreCleanupService request channel
    // to enable manual Blockstore purge requests to be issued
    pub(crate) manual_purge_request_sender: Mutex<Option<Sender<Slot>>>,
    pub slots_stats: SlotsStats,
}

pub struct IndexMetaWorkingSetEntry {
    index: Index,
    // true only if at least one shred for this Index was inserted since the time this
    // struct was created
    did_insert_occur: bool,
}

/// The in-memory data structure for updating entries in the column family
/// [`cf::SlotMeta`].
pub struct SlotMetaWorkingSetEntry {
    /// The dirty version of the `SlotMeta` which might not be persisted
    /// to the blockstore yet.
    new_slot_meta: Rc<RefCell<SlotMeta>>,
    /// The latest version of the `SlotMeta` that was persisted in the
    /// blockstore.  If None, it means the current slot is new to the
    /// blockstore.
    old_slot_meta: Option<SlotMeta>,
    /// True only if at least one shred for this SlotMeta was inserted since
    /// this struct was created.
    did_insert_occur: bool,
}

struct ShredInsertionTracker<'a> {
    // Map which contains data shreds that have just been inserted. They will
    // later be written to `cf::ShredData` or `cf::AlternateShredData`
    just_inserted_shreds: HashMap<(BlockLocation, ShredId), Cow<'a, Shred>>,
    // In-memory map that maintains the dirty copy of the erasure meta.  It will
    // later be written to `cf::ErasureMeta`
    erasure_metas: BTreeMap<ErasureSetId, WorkingEntry<ErasureMeta>>,
    // In-memory map that maintains the dirty copy of the merkle root meta. It
    // will later be written to `cf::MerkleRootMeta` or `cf::AlternateMerkleRootMeta`
    merkle_root_metas: HashMap<(BlockLocation, ErasureSetId), WorkingEntry<MerkleRootMeta>>,
    // In-memory map that maintains the dirty copy of the index meta.  It will
    // later be written to `cf::SlotMeta` or `cf::AlternateSlotMeta`
    slot_meta_working_set: HashMap<(BlockLocation, Slot), SlotMetaWorkingSetEntry>,
    // In-memory map that maintains the dirty copy of the index meta.  It will
    // later be written to `cf::Index` or `cf::AlternateIndex`
    index_working_set: HashMap<(BlockLocation, Slot), IndexMetaWorkingSetEntry>,
    duplicate_shreds: Vec<PossibleDuplicateShred>,
    // Collection of the current blockstore writes which will be committed
    // atomically.
    write_batch: WriteBatch,
    // Time spent on loading or creating the index meta entry from the db
    index_meta_time_us: u64,
    // Collection of recently completed data sets (data portion of erasure batch)
    newly_completed_data_sets: Vec<CompletedDataSetInfo>,
}

impl ShredInsertionTracker<'_> {
    fn new(shred_num: usize, write_batch: WriteBatch) -> Self {
        Self {
            just_inserted_shreds: HashMap::with_capacity(shred_num),
            erasure_metas: BTreeMap::new(),
            merkle_root_metas: HashMap::new(),
            slot_meta_working_set: HashMap::new(),
            index_working_set: HashMap::new(),
            duplicate_shreds: vec![],
            write_batch,
            index_meta_time_us: 0,
            newly_completed_data_sets: vec![],
        }
    }
}

impl SlotMetaWorkingSetEntry {
    /// Construct a new SlotMetaWorkingSetEntry with the specified `new_slot_meta`
    /// and `old_slot_meta`.  `did_insert_occur` is set to false.
    fn new(new_slot_meta: Rc<RefCell<SlotMeta>>, old_slot_meta: Option<SlotMeta>) -> Self {
        Self {
            new_slot_meta,
            old_slot_meta,
            did_insert_occur: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ParentInfo {
    /// Parent slot to expose through SlotMeta and replay.
    pub(crate) parent_slot: Slot,
    /// Parent block id paired with `parent_slot`.
    pub(crate) parent_block_id: Hash,
    /// Zero for the block header parent; non-zero when populated by UpdateParent.
    pub(crate) replay_fec_set_index: u32,
}

impl ParentInfo {
    /// Read the currently persisted parent metadata, if enough information has
    /// been written to distinguish it from an old/default SlotMeta.
    fn from_slot_meta(slot_meta: &SlotMeta) -> Option<Self> {
        let parent_slot = slot_meta.parent_slot?;
        let parent_info = ParentInfo {
            parent_slot,
            parent_block_id: slot_meta.parent_block_id,
            replay_fec_set_index: slot_meta.replay_fec_set_index,
        };
        (parent_info.has_update_parent() || parent_info.parent_block_id != Hash::default())
            .then_some(parent_info)
    }

    /// Try to parse a `ParentInfo` from a block header in the first shred
    /// (index 0) of a slot.
    fn maybe_parse_block_header(current_shred: &Shred) -> Option<Self> {
        // Block headers only occur at index 0
        if current_shred.index() != 0 {
            return None;
        }

        let shred_bytes = current_shred.payload();
        let payload = shred::layout::get_data(shred_bytes).ok()?;

        if !BlockComponent::infer_is_block_marker(payload).unwrap_or(false) {
            return None;
        }

        let component: BlockComponent = wincode::deserialize(payload).ok()?;
        let VersionedBlockMarker::V1(marker) = component.as_marker()?;
        let VersionedBlockHeader::V1(header) = marker.as_block_header()?;

        Some(ParentInfo {
            parent_slot: header.parent_slot,
            parent_block_id: header.parent_block_id,
            replay_fec_set_index: 0,
        })
    }

    /// Validate compatibility between two `ParentInfo` values.
    ///
    /// Returns `Ok(true)` if the new value should replace the previous one,
    /// `Ok(false)` if it should not, or `Err` if they are incompatible.
    fn should_write_parent_info(slot: Slot, new: &ParentInfo, prev: &ParentInfo) -> Result<bool> {
        let (update_parent_info, block_header_parent_info, should_write) = match (
            new.populated_from_block_header(),
            prev.populated_from_block_header(),
        ) {
            // New is BlockHeader, prev is UpdateParent
            // UpdateParent takes precedence, so don't overwrite
            (true, false) => (prev, new, false),
            // New is UpdateParent, prev is BlockHeader
            // Validate and allow UpdateParent to replace BlockHeader
            (false, true) => (new, prev, true),
            // Both are UpdateParent - ensure they match
            (false, false) if new == prev => return Ok(false),
            (false, false) => return Err(BlockstoreError::MultipleUpdateParents(slot)),
            // Both are block headers - ensure they match
            (true, true) if new == prev => return Ok(false),
            (true, true) => return Err(BlockstoreError::BlockComponentMismatch(slot)),
        };

        // Validate that the UpdateParent is compatible with the BlockHeader
        if update_parent_info.block() == block_header_parent_info.block() {
            return Err(BlockstoreError::UpdateParentMatchesBlockHeader(slot));
        }

        if update_parent_info.parent_slot > block_header_parent_info.parent_slot {
            return Err(BlockstoreError::UpdateParentSlotGreaterThanBlockHeader(
                slot,
            ));
        }

        Ok(should_write)
    }

    /// True when this metadata came from an `UpdateParent` marker.
    fn has_update_parent(&self) -> bool {
        self.replay_fec_set_index > 0
    }

    fn validate_update_parent_slot(&self, slot: Slot) -> Result<()> {
        if self.has_update_parent() && leader_slot_index(slot) != 0 {
            return Err(BlockstoreError::UpdateParentNotFirstInLeaderWindow(slot));
        }

        Ok(())
    }

    /// True when this metadata came from the slot's block header.
    fn populated_from_block_header(&self) -> bool {
        self.replay_fec_set_index == 0
    }

    /// Parent slot and block id as a single comparable value.
    fn block(&self) -> (Slot, Hash) {
        (self.parent_slot, self.parent_block_id)
    }

    /// Validate this parent against both ledger ancestry and the shred header.
    ///
    /// UpdateParent is allowed to choose the shred-header parent slot or an
    /// older parent slot; block headers must exactly match the shred header.
    /// The shred header has no parent block id, so same-slot UpdateParent
    /// switches remain legal for duplicate parent blocks.
    fn validate_shred_parent(&self, slot: Slot, shred_parent_slot: Slot, root: Slot) -> Result<()> {
        if !verify_shred_slots(slot, self.parent_slot, root) {
            return Err(BlockstoreError::InvalidParentInfo {
                slot,
                parent_slot: self.parent_slot,
                root,
            });
        }

        if self.populated_from_block_header() && self.parent_slot != shred_parent_slot {
            return Err(BlockstoreError::BlockHeaderParentMismatch {
                slot,
                block_header_parent_slot: self.parent_slot,
                shred_parent_slot,
            });
        }

        if self.has_update_parent() && self.parent_slot > shred_parent_slot {
            return Err(BlockstoreError::UpdateParentSlotGreaterThanShredParent {
                slot,
                update_parent_slot: self.parent_slot,
                shred_parent_slot,
            });
        }

        Ok(())
    }
}

pub fn banking_trace_path(path: &Path) -> PathBuf {
    path.join("banking_trace")
}

impl Blockstore {
    pub fn ledger_path(&self) -> &PathBuf {
        &self.ledger_path
    }

    pub fn banking_trace_path(&self) -> PathBuf {
        banking_trace_path(&self.ledger_path)
    }

    /// Wires the channel used to hand certificates recovered from blockstore to the BLS
    /// sigverifier.
    ///
    /// The accompanying slot is the carrier slot whose entries contained the certificate, while
    /// the slot in the certificate is the block being certified. The carrier slot is needed to
    /// check whether block markers were active and to attribute an invalid certificate to the
    /// leader that included it.
    pub fn set_certificate_sender(&self, sender: Sender<(Slot, UnverifiedCertificate)>) {
        self.certificate_sender
            .set(sender)
            .expect("certificate sender already set");
    }

    /// Opens a Ledger in directory, provides "infinite" window of shreds
    pub fn open(ledger_path: &Path) -> Result<Blockstore> {
        Self::do_open(ledger_path, BlockstoreOptions::default())
    }

    pub fn open_with_options(ledger_path: &Path, options: BlockstoreOptions) -> Result<Blockstore> {
        Self::do_open(ledger_path, options)
    }

    fn do_open(ledger_path: &Path, options: BlockstoreOptions) -> Result<Blockstore> {
        fs::create_dir_all(ledger_path)?;
        let blockstore_path = ledger_path.join(BLOCKSTORE_DIRECTORY_ROCKS_LEVEL);

        // Open the database
        let mut measure = Measure::start("blockstore open");
        info!("Opening blockstore at {blockstore_path:?}");
        let db = Arc::new(Rocks::open(blockstore_path, options)?);

        let data_shred_cf = db.column();
        let code_shred_cf = db.column();
        let meta_cf = db.column();
        let index_cf = db.column();
        let erasure_meta_cf = db.column();
        let merkle_root_meta_cf = db.column();
        let double_merkle_meta_cf = db.column();
        let orphans_cf = db.column();
        let duplicate_slots_cf = db.column();

        let alt_data_shred_cf = db.column();
        let alt_meta_cf = db.column();
        let alt_index_cf = db.column();
        let alt_merkle_root_meta_cf = db.column();

        let bank_hash_cf = db.column();
        let optimistic_slots_cf = db.column();
        let roots_cf = db.column();
        let dead_slots_cf = db.column();

        let block_height_cf = db.column();
        let blocktime_cf = db.column();
        let rewards_cf = db.column();
        let transaction_status_cf = db.column();
        let transaction_memos_cf = db.column();
        let address_signatures_cf = db.column();
        let perf_samples_cf = db.column();

        // Get max root or 0 if it doesn't exist
        let max_root = roots_cf
            .iter(IteratorMode::End)?
            .next()
            .map(|(slot, _)| slot)
            .unwrap_or(0);
        let max_root = AtomicU64::new(max_root);

        measure.stop();
        info!("Opening blockstore done; {measure}");
        let blockstore = Blockstore {
            ledger_path: ledger_path.to_path_buf(),
            db,
            address_signatures_cf,
            bank_hash_cf,
            block_height_cf,
            blocktime_cf,
            code_shred_cf,
            data_shred_cf,
            dead_slots_cf,
            orphans_cf,
            duplicate_slots_cf,
            erasure_meta_cf,
            index_cf,
            merkle_root_meta_cf,
            double_merkle_meta_cf,
            meta_cf,
            optimistic_slots_cf,
            perf_samples_cf,
            rewards_cf,
            roots_cf,
            transaction_memos_cf,
            transaction_status_cf,
            alt_meta_cf,
            alt_index_cf,
            alt_data_shred_cf,
            alt_merkle_root_meta_cf,

            new_shreds_signals: Mutex::default(),
            completed_slots_senders: Mutex::default(),
            update_parent_signals: Mutex::default(),
            update_parent_shred_parent_cache: Mutex::new(LruCache::new(
                UPDATE_PARENT_SHRED_PARENT_CACHE_CAPACITY,
            )),
            certificate_sender: OnceLock::new(),
            insert_shreds_lock: Mutex::<()>::default(),
            switch_block_lock: SwitchBlockLock(FairMutex::new(())),
            max_root,
            lowest_cleanup_slot: RwLock::<Slot>::default(),
            manual_purge_request_sender: Mutex::default(),
            slots_stats: SlotsStats::default(),
        };

        Ok(blockstore)
    }

    pub fn open_with_signal(
        ledger_path: &Path,
        options: BlockstoreOptions,
    ) -> Result<BlockstoreSignals> {
        let blockstore = Self::open_with_options(ledger_path, options)?;
        let (ledger_signal_sender, ledger_signal_receiver) = bounded(MAX_REPLAY_WAKE_UP_SIGNALS);
        let (completed_slots_sender, completed_slots_receiver) =
            bounded(MAX_COMPLETED_SLOTS_IN_CHANNEL);
        let (update_parent_sender, update_parent_receiver) = bounded(MAX_UPDATE_PARENT_SIGNALS);

        blockstore.add_new_shred_signal(ledger_signal_sender);
        blockstore.add_completed_slots_signal(completed_slots_sender);
        blockstore.add_update_parent_signal(update_parent_sender);

        Ok(BlockstoreSignals {
            blockstore,
            ledger_signal_receiver,
            completed_slots_receiver,
            update_parent_receiver,
        })
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn add_tree(
        &self,
        forks: Tree<Slot>,
        is_orphan: bool,
        is_slot_complete: bool,
        num_ticks: u64,
        starting_hash: Hash,
    ) {
        let mut walk = TreeWalk::from(forks);
        let mut blockhashes = HashMap::new();
        let mut merkle_roots: HashMap<Slot, Hash> = HashMap::new();
        let reed_solomon_cache = shred::ReedSolomonCache::default();
        while let Some(visit) = walk.get() {
            let slot = *visit.node().data();
            if self.meta(slot).unwrap().is_some() && self.orphan(slot).unwrap().is_none() {
                // If slot exists in blockstore and is not an orphan, then skip it
                walk.forward();
                continue;
            }
            let parent = walk.get_parent().map(|n| *n.data());
            if parent.is_some() || !is_orphan {
                // parent won't exist for first node in a tree where
                // `is_orphan == true`
                let parent_hash = parent
                    .and_then(|parent| blockhashes.get(&parent))
                    .unwrap_or(&starting_hash);
                let parent_slot = parent.unwrap_or(slot);
                let mut entries = create_ticks(
                    num_ticks * (std::cmp::max(1, slot - parent_slot)),
                    0,
                    *parent_hash,
                );
                blockhashes.insert(slot, entries.last().unwrap().hash);
                if !is_slot_complete {
                    entries.pop().unwrap();
                }
                let chained_merkle_root = parent
                    .and_then(|p| merkle_roots.get(&p).copied())
                    .or_else(|| self.get_last_shred_merkle_root(parent_slot).ok().flatten())
                    .unwrap_or_else(|| Hash::new_from_array(rand::rng().random()));
                let shreds: Vec<Shred> = Shredder::new(slot, parent_slot, 0, 0)
                    .unwrap()
                    .make_merkle_shreds_from_entries(
                        &Keypair::new(),
                        &entries,
                        is_slot_complete,
                        chained_merkle_root,
                        0,
                        0,
                        &reed_solomon_cache,
                        &mut ProcessShredsStats::default(),
                    )
                    .filter(Shred::is_data)
                    .collect();
                if let Some(last_shred) = shreds.last() {
                    merkle_roots.insert(slot, last_shred.merkle_root().unwrap());
                }
                self.insert_shreds(shreds, false).unwrap();
            }
            walk.forward();
        }
    }

    /// Deletes the blockstore at the specified path.
    ///
    /// Note that if the `ledger_path` has multiple rocksdb instances, this
    /// function will destroy all.
    pub fn destroy(ledger_path: &Path) -> Result<()> {
        // Database::destroy() fails if the root directory doesn't exist
        fs::create_dir_all(ledger_path)?;
        Rocks::destroy(&Path::new(ledger_path).join(BLOCKSTORE_DIRECTORY_ROCKS_LEVEL))
    }

    /// Checks all available block versions, if we have a *complete* block for
    /// `block_id`, returns the location where it is stored
    fn get_block_location(&self, slot: Slot, block_id: Hash) -> Result<Option<BlockLocation>> {
        for location in [
            BlockLocation::Original,
            BlockLocation::Alternate { block_id },
        ] {
            if self.get_double_merkle_root(slot, location)? == Some(block_id) {
                return Ok(Some(location));
            }
        }
        Ok(None)
    }

    /// Returns the SlotMeta for a complete block identified by `block_id`, along with
    /// its current location.
    pub fn get_slot_meta_for_block_id(
        &self,
        slot: Slot,
        block_id: Hash,
    ) -> Result<Option<(SlotMeta, BlockLocation)>> {
        let _lock = self.switch_block_lock.lock();
        let Some(location) = self.get_block_location(slot, block_id)? else {
            return Ok(None);
        };
        let Some(slot_meta) = self.meta_from_location(slot, location)? else {
            return Ok(None);
        };
        Ok(Some((slot_meta, location)))
    }

    /// Returns the SlotMeta of the specified slot.
    pub fn meta(&self, slot: Slot) -> Result<Option<SlotMeta>> {
        self.meta_cf.get(slot)
    }

    /// Returns the [`SlotMetaRepair`] of the specified slot.
    pub fn meta_repair(&self, slot: Slot) -> Result<Option<SlotMetaRepair>> {
        self.meta_cf
            .get_slice(slot)?
            .as_deref()
            .map(|bytes| Ok(wincode::deserialize::<SlotMetaRepair>(bytes)?))
            .transpose()
    }

    /// Returns the SlotMeta of the specified slot from the specified location
    fn meta_from_location(&self, slot: Slot, location: BlockLocation) -> Result<Option<SlotMeta>> {
        match location {
            BlockLocation::Original => self.meta_cf.get(slot),
            BlockLocation::Alternate { block_id } => self.alt_meta_cf.get((slot, block_id)),
        }
    }

    /// Puts the SlotMeta of the specified slot in the column for the specified location
    fn put_meta_in_batch(
        &self,
        write_batch: &mut WriteBatch,
        slot: Slot,
        location: BlockLocation,
        meta: &SlotMeta,
    ) -> Result<()> {
        match location {
            BlockLocation::Original => self.meta_cf.put_in_batch(write_batch, slot, meta),
            BlockLocation::Alternate { block_id } => {
                self.alt_meta_cf
                    .put_in_batch(write_batch, (slot, block_id), meta)
            }
        }
    }

    /// Inserts a shred index into the alternate index for testing purposes.
    /// This simulates receiving a data shred for an alternate block.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn insert_shred_index_for_alternate_block(
        &self,
        slot: Slot,
        block_id: Hash,
        shred_index: u32,
    ) -> Result<()> {
        use crate::blockstore_meta::Index;
        let mut index = self
            .alt_index_cf
            .get((slot, block_id))?
            .unwrap_or_else(|| Index::new(slot));
        index.data_mut().insert(shred_index as u64);
        self.alt_index_cf.put((slot, block_id), &index)
    }

    /// Test helper: directly set the double merkle root for a slot/location.
    /// This simulates Turbine completing for a slot with a specific block_id.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn set_double_merkle_root(
        &self,
        slot: Slot,
        block_location: BlockLocation,
        double_merkle_root: Hash,
    ) -> Result<()> {
        use crate::blockstore_meta::DoubleMerkleMeta;
        let meta = DoubleMerkleMeta {
            double_merkle_root,
            fec_set_count: 1,   // Minimal valid value
            proofs: Vec::new(), // Empty proofs for testing
        };
        self.double_merkle_meta_cf
            .put((slot, block_location), &meta)
    }

    /// Returns true if the specified slot is full.
    pub fn is_full(&self, slot: Slot) -> bool {
        if let Ok(Some(meta)) = self.meta_cf.get(slot) {
            return meta.is_full();
        }
        false
    }

    fn erasure_meta(&self, erasure_set: ErasureSetId) -> Result<Option<ErasureMeta>> {
        let (slot, fec_set_index) = erasure_set.store_key();
        self.erasure_meta_cf.get((slot, u64::from(fec_set_index)))
    }

    #[cfg(test)]
    fn put_erasure_meta(
        &self,
        erasure_set: ErasureSetId,
        erasure_meta: &ErasureMeta,
    ) -> Result<()> {
        let (slot, fec_set_index) = erasure_set.store_key();
        self.erasure_meta_cf.put_bytes(
            (slot, u64::from(fec_set_index)),
            &wincode::serialize(erasure_meta).unwrap(),
        )
    }

    /// Attempts to find the previous consecutive erasure set for `erasure_set`
    /// using the fact that FEC sets are fixed in size at DATA_SHREDS_PER_FEC_BLOCK
    /// data shreds.
    ///
    /// Only for use in the `Original` column
    fn previous_fec_set_shred_id(
        &self,
        erasure_set: &ErasureSetId,
        merkle_root_metas: &HashMap<(BlockLocation, ErasureSetId), WorkingEntry<MerkleRootMeta>>,
    ) -> Option<ShredId> {
        let prev_erasure_set = erasure_set.previous_fec_set()?;
        let prev_merkle_root_meta = merkle_root_metas
            .get(&(BlockLocation::Original, prev_erasure_set))
            .map(WorkingEntry::as_ref)
            .map(Cow::Borrowed)
            .or_else(|| {
                self.merkle_root_meta(prev_erasure_set)
                    .unwrap()
                    .map(Cow::Owned)
            })?;
        Some(ShredId::new(
            erasure_set.slot(),
            prev_merkle_root_meta.first_received_shred_index(),
            prev_merkle_root_meta.first_received_shred_type(),
        ))
    }

    fn merkle_root_meta(&self, erasure_set: ErasureSetId) -> Result<Option<MerkleRootMeta>> {
        self.merkle_root_meta_cf.get(erasure_set.store_key())
    }

    fn merkle_root_meta_from_location(
        &self,
        erasure_set: ErasureSetId,
        location: BlockLocation,
    ) -> Result<Option<MerkleRootMeta>> {
        match location {
            BlockLocation::Original => self.merkle_root_meta_cf.get(erasure_set.store_key()),
            BlockLocation::Alternate { block_id } => {
                let (slot, fec_set_index) = erasure_set.store_key();
                self.alt_merkle_root_meta_cf
                    .get((slot, block_id, fec_set_index))
            }
        }
    }

    /// Puts the MerkleRootMeta of the specified erasure set in the column for the specified location
    fn put_merkle_root_meta_in_batch(
        &self,
        write_batch: &mut WriteBatch,
        erasure_set: ErasureSetId,
        location: BlockLocation,
        merkle_root_meta: &MerkleRootMeta,
    ) -> Result<()> {
        let (slot, fec_set_index) = erasure_set.store_key();
        match location {
            BlockLocation::Original => self.merkle_root_meta_cf.put_in_batch(
                write_batch,
                (slot, fec_set_index),
                merkle_root_meta,
            ),
            BlockLocation::Alternate { block_id } => self.alt_merkle_root_meta_cf.put_in_batch(
                write_batch,
                (slot, block_id, fec_set_index),
                merkle_root_meta,
            ),
        }
    }

    /// For serving parent fec-set-count block id repair responses.
    ///
    /// Returns relevant metadata for the complete block identified by `block_id`.
    ///
    /// If the repair proofs are missing, these are also populated.
    pub fn get_parent_repair_metadata(
        &self,
        slot: Slot,
        block_id: Hash,
    ) -> Result<Option<(DoubleMerkleMeta, SlotMeta)>> {
        let lock = self.switch_block_lock.lock();
        let Some((double_merkle_meta, location)) =
            self.get_double_merkle_meta_with_proofs_locked(slot, block_id, &lock)?
        else {
            return Ok(None);
        };
        let Some(slot_meta) = self.meta_from_location(slot, location)? else {
            return Ok(None);
        };
        Ok(Some((double_merkle_meta, slot_meta)))
    }

    /// For serving fec-set-root block id repair responses.
    ///
    /// Returns relevant metadata for the complete block identified by `block_id`.
    ///
    /// If the repair proofs are missing, these are also populated.
    pub fn get_fec_set_root_repair_metadata(
        &self,
        slot: Slot,
        block_id: Hash,
        fec_set_index: u32,
    ) -> Result<Option<(DoubleMerkleMeta, MerkleRootMeta)>> {
        let lock = self.switch_block_lock.lock();
        let Some((double_merkle_meta, location)) =
            self.get_double_merkle_meta_with_proofs_locked(slot, block_id, &lock)?
        else {
            return Ok(None);
        };
        let erasure_set = ErasureSetId::new(slot, fec_set_index);
        let Some(merkle_root_meta) = self.merkle_root_meta_from_location(erasure_set, location)?
        else {
            return Ok(None);
        };
        Ok(Some((double_merkle_meta, merkle_root_meta)))
    }

    /// Gets the double merkle meta for a complete block identified by `block_id`.
    /// If the meta is present but the proofs have not yet been populated, generate
    /// and store them and return the updated double merkle meta.
    fn get_double_merkle_meta_with_proofs_locked(
        &self,
        slot: Slot,
        block_id: Hash,
        _switch_block_lock: &SwitchBlockGuard<'_>,
    ) -> Result<Option<(DoubleMerkleMeta, BlockLocation)>> {
        // Find which column this block resides in (if any)
        let Some(location) = self.get_block_location(slot, block_id)? else {
            return Ok(None);
        };

        // Block was full above, get_block_location returned Some, but may have
        // been purged by BlockstoreCleanupService in between
        let Some(mut double_merkle_meta) = self.double_merkle_meta_cf.get((slot, location))? else {
            return Ok(None);
        };

        if double_merkle_meta.proofs.is_empty() {
            self.populate_double_merkle_meta_proofs(slot, location, &mut double_merkle_meta)?;
            self.double_merkle_meta_cf
                .put((slot, location), &double_merkle_meta)?;
        }

        if double_merkle_meta.double_merkle_root == block_id {
            Ok(Some((double_merkle_meta, location)))
        } else {
            Ok(None)
        }
    }

    /// Gets the double merkle root for the block in the given location.
    ///
    /// Returns `None` if the block is not full, or if the slot was completed
    /// while block markers were disabled. That can happen for legacy Tower
    /// slots in a process that later enters Alpenglow migration.
    pub fn get_double_merkle_root(
        &self,
        slot: Slot,
        location: BlockLocation,
    ) -> Result<Option<Hash>> {
        let Some(double_merkle_meta_bytes) =
            self.double_merkle_meta_cf.get_slice((slot, location))?
        else {
            return Ok(None);
        };

        let dmr: Hash = wincode::deserialize(&double_merkle_meta_bytes[0..HASH_BYTES])?;
        Ok(Some(dmr))
    }

    /// Returns the parent metadata carried by SlotMeta for the specified slot and location.
    #[cfg(test)]
    pub(crate) fn get_parent_info(
        &self,
        slot: Slot,
        location: BlockLocation,
    ) -> Result<Option<ParentInfo>> {
        let slot_meta = self.meta_from_location(slot, location)?;
        Ok(slot_meta.and_then(|slot_meta| ParentInfo::from_slot_meta(&slot_meta)))
    }

    /// Try to parse a `ParentInfo` from an `UpdateParent` block component at
    /// an FEC set boundary.
    ///
    /// Two cases trigger parsing:
    /// - (a) Current shred has `data_complete()` — look at the 0th shred of the
    ///   NEXT FEC set for an `UpdateParent`.
    /// - (b) Current shred is at index 0 of its FEC set (and index > 0) — check
    ///   if the previous shred had `DATA_COMPLETE_SHRED`, and if so parse the
    ///   current shred.
    fn maybe_parse_update_parent(
        &self,
        current_shred: &Shred,
        slot: Slot,
        location: BlockLocation,
        just_inserted_shreds: &HashMap<(BlockLocation, ShredId), Cow<'_, Shred>>,
    ) -> Option<ParentInfo> {
        let current_index = current_shred.index();
        let fec_set_index = current_shred.fec_set_index();
        let data_complete = current_shred.data_complete();

        let (shred_bytes, target_fec_set_index) = if data_complete {
            // Case (a): Current shred has DATA_COMPLETE=true (end of FEC set)
            // Check the 0th shred in the NEXT FEC set for UpdateParent
            let next_fec_set_index = fec_set_index + DATA_SHREDS_PER_FEC_BLOCK as u32;
            let shred_id = ShredId::new(slot, next_fec_set_index, ShredType::Data);
            let shred_bytes =
                self.get_shred_from_just_inserted_or_db(just_inserted_shreds, shred_id, location);
            (shred_bytes, next_fec_set_index)
        } else if current_index.is_multiple_of(DATA_SHREDS_PER_FEC_BLOCK as u32)
            && current_index > 0
        {
            // Case (b): Current shred is the 0th shred of the FEC set
            // Check if the PREVIOUS shred had DATA_COMPLETE=true
            let prev_shred_index = current_index - 1;
            let shred_id = ShredId::new(slot, prev_shred_index, ShredType::Data);

            let prev_payload =
                self.get_shred_from_just_inserted_or_db(just_inserted_shreds, shred_id, location);

            let shred_bytes = prev_payload
                .and_then(|prev| shred::layout::get_flags(&prev).ok())
                .and_then(|flags| {
                    flags
                        .contains(ShredFlags::DATA_COMPLETE_SHRED)
                        .then_some(Cow::Borrowed(current_shred.payload()))
                });

            (shred_bytes, fec_set_index)
        } else {
            return None;
        };

        let shred_bytes = shred_bytes?;
        let payload = shred::layout::get_data(&shred_bytes).ok()?;

        if !BlockComponent::infer_is_block_marker(payload).unwrap_or(false) {
            return None;
        }

        let component: BlockComponent = wincode::deserialize(payload).ok()?;
        let VersionedBlockMarker::V1(marker) = component.as_marker()?;
        let VersionedUpdateParent::V1(update_parent) = marker.as_update_parent()?;

        Some(ParentInfo {
            parent_slot: update_parent.new_parent_slot,
            parent_block_id: update_parent.new_parent_block_id,
            replay_fec_set_index: target_fec_set_index,
        })
    }

    /// Orchestrates parsing and validation of `ParentInfo` during shred
    /// insertion.
    ///
    /// Called at FEC set boundaries. Parses block headers and `UpdateParent`
    /// markers, validates them against parent metadata in `SlotMeta`, and
    /// marks the slot dead on conflict.
    fn maybe_update_parent_info(
        &self,
        shred: &Shred,
        shred_parent_slot: Slot,
        location: BlockLocation,
        slot_meta: &mut SlotMeta,
        just_inserted_shreds: &HashMap<(BlockLocation, ShredId), Cow<'_, Shred>>,
        write_batch: &mut WriteBatch,
    ) -> Result<()> {
        let slot = shred.slot();
        let previous_parent_info = ParentInfo::from_slot_meta(slot_meta);

        let new_parent_info = if shred.index() == 0 {
            ParentInfo::maybe_parse_block_header(shred)
        } else {
            // Keep checking FEC boundaries even after an UpdateParent has been
            // recorded, so multiple UpdateParents are detected independent of
            // shred arrival order.
            self.maybe_parse_update_parent(shred, slot, location, just_inserted_shreds)
        };

        let Some(new_parent_info) = new_parent_info else {
            return Ok(());
        };

        if let Err(err) = new_parent_info.validate_update_parent_slot(slot) {
            self.mark_invalid_parent_info_dead(write_batch, new_parent_info, slot, location, &err)?;
            return Err(err);
        }

        let max_root = self.max_root();
        if let Err(err) = new_parent_info.validate_shred_parent(slot, shred_parent_slot, max_root) {
            self.mark_invalid_parent_info_dead(write_batch, new_parent_info, slot, location, &err)?;
            return Err(err);
        }

        // Validate new ParentInfo against previous (if both exist)
        if let Some(prev) = previous_parent_info.as_ref() {
            match ParentInfo::should_write_parent_info(slot, &new_parent_info, prev) {
                Ok(true) => {}
                Ok(false) => return Ok(()),
                Err(e) => {
                    self.mark_invalid_parent_info_dead(
                        write_batch,
                        new_parent_info,
                        slot,
                        location,
                        &e,
                    )?;
                    return Err(e);
                }
            }
        }

        // Update parent metadata fields in SlotMeta.
        slot_meta.update_from_parent_info(new_parent_info);

        Ok(())
    }

    fn mark_invalid_parent_info_dead(
        &self,
        write_batch: &mut WriteBatch,
        parent_info: ParentInfo,
        slot: Slot,
        location: BlockLocation,
        err: &BlockstoreError,
    ) -> Result<()> {
        // Parent markers are consensus-critical. If a marker is malformed or
        // incompatible with already-persisted metadata, keep the slot from
        // being replayed rather than letting validators derive different
        // parents from different shred arrival orders.
        datapoint_error!(
            "blockstore_error",
            (
                "error",
                format!(
                    "Invalid parent info in slot {slot} location {location}: {:?}. Marking slot \
                     as dead",
                    parent_info,
                ),
                String
            )
        );
        if !matches!(location, BlockLocation::Original) {
            error!(
                "Shreds being inserted to the alternate column {location:?} for slot {slot} have \
                 invalid or mismatching ParentInfo: {err}. Note that these shreds have reached a \
                 confirmation threshold, and were fetched by block id repair which does pre \
                 verification via merkle proofs before ingest. For a mismatch to occur something \
                 has gone disastrously wrong and we might not be able to recover. Marking the \
                 slot as dead.",
            );
        }
        self.dead_slots_cf.put_in_batch(write_batch, slot, &true)
    }

    /// Check whether the specified slot is an orphan slot which does not
    /// have a parent slot.
    ///
    /// Returns true if the specified slot does not have a parent slot.
    /// For other return values, it means either the slot is not in the
    /// blockstore or the slot isn't an orphan slot.
    pub fn orphan(&self, slot: Slot) -> Result<Option<bool>> {
        self.orphans_cf.get(slot)
    }

    pub fn slot_meta_iterator(
        &self,
        slot: Slot,
    ) -> Result<impl Iterator<Item = (Slot, SlotMeta)> + '_> {
        let meta_iter = self
            .meta_cf
            .iter(IteratorMode::From(slot, IteratorDirection::Forward))?;
        Ok(meta_iter.map(|(slot, slot_meta_bytes)| {
            (
                slot,
                cf::SlotMeta::deserialize(&slot_meta_bytes).unwrap_or_else(|e| {
                    panic!("Could not deserialize SlotMeta for slot {slot}: {e:?}")
                }),
            )
        }))
    }

    pub fn live_slots_iterator(&self, root: Slot) -> impl Iterator<Item = (Slot, SlotMeta)> + '_ {
        let root_forks = NextSlotsIterator::new(root, self);

        let orphans_iter = self.orphans_iterator(root + 1).unwrap();
        root_forks.chain(orphans_iter.flat_map(move |orphan| NextSlotsIterator::new(orphan, self)))
    }

    pub fn live_files_metadata(&self) -> Result<Vec<LiveFile>> {
        self.db.live_files_metadata()
    }

    #[cfg(feature = "dev-context-only-utils")]
    #[allow(clippy::type_complexity)]
    pub fn iterator_cf(
        &self,
        cf_name: &str,
    ) -> Result<impl Iterator<Item = (Box<[u8]>, Box<[u8]>)> + '_ + use<'_>> {
        let cf = self.db.cf_handle(cf_name);
        let iterator = self.db.iterator_cf(cf, rocksdb::IteratorMode::Start);
        Ok(iterator.map(|pair| pair.unwrap()))
    }

    #[allow(clippy::type_complexity)]
    pub fn slot_data_iterator(
        &self,
        slot: Slot,
        index: u64,
    ) -> Result<impl Iterator<Item = ((u64, u64), Box<[u8]>)> + '_> {
        let slot_iterator = self.data_shred_cf.iter(IteratorMode::From(
            (slot, index),
            IteratorDirection::Forward,
        ))?;
        Ok(slot_iterator.take_while(move |((shred_slot, _), _)| *shred_slot == slot))
    }

    #[allow(clippy::type_complexity)]
    pub fn slot_coding_iterator(
        &self,
        slot: Slot,
        index: u64,
    ) -> Result<impl Iterator<Item = ((u64, u64), Box<[u8]>)> + '_> {
        let slot_iterator = self.code_shred_cf.iter(IteratorMode::From(
            (slot, index),
            IteratorDirection::Forward,
        ))?;
        Ok(slot_iterator.take_while(move |((shred_slot, _), _)| *shred_slot == slot))
    }

    fn prepare_rooted_slot_iterator(
        &self,
        slot: Slot,
        direction: IteratorDirection,
    ) -> Result<impl Iterator<Item = Slot> + '_> {
        let slot_iterator = self.roots_cf.iter(IteratorMode::From(slot, direction))?;
        Ok(slot_iterator.map(move |(rooted_slot, _)| rooted_slot))
    }

    pub fn rooted_slot_iterator(&self, slot: Slot) -> Result<impl Iterator<Item = Slot> + '_> {
        self.prepare_rooted_slot_iterator(slot, IteratorDirection::Forward)
    }

    pub fn reversed_rooted_slot_iterator(
        &self,
        slot: Slot,
    ) -> Result<impl Iterator<Item = Slot> + '_> {
        self.prepare_rooted_slot_iterator(slot, IteratorDirection::Reverse)
    }

    pub fn reversed_optimistic_slots_iterator(
        &self,
    ) -> Result<impl Iterator<Item = (Slot, Hash, UnixTimestamp)> + '_> {
        let iter = self.optimistic_slots_cf.iter(IteratorMode::End)?;
        Ok(iter.map(|(slot, bytes)| {
            let meta = cf::OptimisticSlots::deserialize(&bytes).unwrap();
            (slot, meta.hash(), meta.timestamp())
        }))
    }

    /// Determines if we can iterate from `starting_slot` to >= `ending_slot` by full slots
    /// `starting_slot` is excluded from the `is_full()` check
    pub fn slot_range_connected(&self, starting_slot: Slot, ending_slot: Slot) -> bool {
        if starting_slot == ending_slot {
            return true;
        }

        let mut next_slots: VecDeque<_> = match self.meta(starting_slot) {
            Ok(Some(starting_slot_meta)) => starting_slot_meta.next_slots.into(),
            _ => return false,
        };
        while let Some(slot) = next_slots.pop_front() {
            if let Ok(Some(slot_meta)) = self.meta(slot)
                && slot_meta.is_full()
            {
                match slot.cmp(&ending_slot) {
                    cmp::Ordering::Less => next_slots.extend(slot_meta.next_slots),
                    _ => return true,
                }
            }
        }

        false
    }

    /// Return the available data shreds for recovery.
    /// Note: that we do not do recovery on the Alternate shred columns
    fn get_recovery_data_shreds<'a>(
        &'a self,
        index: &'a Index,
        erasure_meta: &'a ErasureMeta,
        prev_inserted_shreds: &'a HashMap<(BlockLocation, ShredId), Cow<'_, Shred>>,
    ) -> impl Iterator<Item = Shred> + 'a {
        let slot = index.slot;
        erasure_meta.data_shreds_indices().filter_map(move |i| {
            let key = ShredId::new(slot, u32::try_from(i).unwrap(), ShredType::Data);
            if let Some(shred) = prev_inserted_shreds.get(&(BlockLocation::Original, key)) {
                return Some(shred.as_ref().clone());
            }
            if !index.data().contains(i) {
                return None;
            }
            match self.data_shred_cf.get_bytes((slot, i)).unwrap() {
                None => {
                    error!(
                        "Unable to read the data shred with slot {slot}, index {i} for shred \
                         recovery. The shred is marked present in the slot's data shred index, \
                         but the shred could not be found in the data shred column."
                    );
                    None
                }
                Some(data) => Shred::new_from_serialized_shred(data).ok(),
            }
        })
    }

    /// Return the available coding shreds for recovery.
    /// Note: that we do not do recovery on the Alternate shred columns
    fn get_recovery_coding_shreds<'a>(
        &'a self,
        index: &'a Index,
        erasure_meta: &'a ErasureMeta,
        prev_inserted_shreds: &'a HashMap<(BlockLocation, ShredId), Cow<'_, Shred>>,
    ) -> impl Iterator<Item = Shred> + 'a {
        let slot = index.slot;
        erasure_meta.coding_shreds_indices().filter_map(move |i| {
            let key = ShredId::new(slot, u32::try_from(i).unwrap(), ShredType::Code);
            if let Some(shred) = prev_inserted_shreds.get(&(BlockLocation::Original, key)) {
                return Some(shred.as_ref().clone());
            }
            if !index.coding().contains(i) {
                return None;
            }
            match self.code_shred_cf.get_bytes((slot, i)).unwrap() {
                None => {
                    error!(
                        "Unable to read the coding shred with slot {slot}, index {i} for shred \
                         recovery. The shred is marked present in the slot's coding shred index, \
                         but the shred could not be found in the coding shred column."
                    );
                    None
                }
                Some(code) => Shred::new_from_serialized_shred(code).ok(),
            }
        })
    }

    /// Performs shred recovery for `erasure_meta`, applying shred sanitization and filters
    /// to the recovered shreds.
    /// Note: that we do not do recovery on the Alternate shred columns
    fn recover_shreds(
        &self,
        index: &Index,
        erasure_meta: &ErasureMeta,
        prev_inserted_shreds: &HashMap<(BlockLocation, ShredId), Cow<'_, Shred>>,
        shred_recovery_ctx: &mut ShredRecoveryContext,
        recovered_shreds: &mut Vec<Payload>,
        recovered_data_shreds: &mut Vec<Shred>,
    ) -> std::result::Result<(), shred::Error> {
        // Find shreds for this erasure set and try recovery
        let data = self.get_recovery_data_shreds(index, erasure_meta, prev_inserted_shreds);
        let code = self.get_recovery_coding_shreds(index, erasure_meta, prev_inserted_shreds);
        shred_recovery_ctx.recover(data.chain(code), recovered_shreds, recovered_data_shreds)
    }

    /// Collects and reports [`BlockstoreRocksDbColumnFamilyMetrics`] for the
    /// all the column families.
    ///
    /// [`BlockstoreRocksDbColumnFamilyMetrics`]: crate::blockstore_metrics::BlockstoreRocksDbColumnFamilyMetrics
    pub fn submit_rocksdb_cf_metrics_for_all_cfs(&self) {
        self.data_shred_cf.submit_rocksdb_cf_metrics();
        self.code_shred_cf.submit_rocksdb_cf_metrics();
        self.meta_cf.submit_rocksdb_cf_metrics();
        self.index_cf.submit_rocksdb_cf_metrics();
        self.erasure_meta_cf.submit_rocksdb_cf_metrics();
        self.merkle_root_meta_cf.submit_rocksdb_cf_metrics();
        self.orphans_cf.submit_rocksdb_cf_metrics();
        self.duplicate_slots_cf.submit_rocksdb_cf_metrics();

        self.alt_data_shred_cf.submit_rocksdb_cf_metrics();
        self.alt_meta_cf.submit_rocksdb_cf_metrics();
        self.alt_index_cf.submit_rocksdb_cf_metrics();
        self.alt_merkle_root_meta_cf.submit_rocksdb_cf_metrics();

        self.bank_hash_cf.submit_rocksdb_cf_metrics();
        self.optimistic_slots_cf.submit_rocksdb_cf_metrics();
        self.roots_cf.submit_rocksdb_cf_metrics();
        self.dead_slots_cf.submit_rocksdb_cf_metrics();

        self.block_height_cf.submit_rocksdb_cf_metrics();
        self.blocktime_cf.submit_rocksdb_cf_metrics();
        self.rewards_cf.submit_rocksdb_cf_metrics();
        self.transaction_status_cf.submit_rocksdb_cf_metrics();
        self.transaction_memos_cf.submit_rocksdb_cf_metrics();
        self.address_signatures_cf.submit_rocksdb_cf_metrics();
        self.perf_samples_cf.submit_rocksdb_cf_metrics();
    }

    /// If the block is not full, mark the slot as dead
    fn mark_slot_dead_if_not_full(
        &self,
        slot: Slot,
        location: BlockLocation,
        shred_insertion_tracker: &mut ShredInsertionTracker,
    ) {
        let mark_slot_dead = shred_insertion_tracker
            .slot_meta_working_set
            .get(&(location, slot))
            .map(|meta| !meta.new_slot_meta.borrow().is_full())
            .or_else(|| {
                self.meta_from_location(slot, location)
                    .ok()
                    .flatten()
                    .map(|meta| !meta.is_full())
            })
            .unwrap_or(true);

        if mark_slot_dead {
            // If the slot is already full there is no reason to mark as dead
            self.dead_slots_cf.put_bytes_in_batch(
                &mut shred_insertion_tracker.write_batch,
                slot,
                &[true as u8],
            );
        }
    }

    /// Attempts to insert shreds into blockstore and updates relevant metrics
    /// based on the results, split out by shred source (turbine vs. repair).
    fn attempt_shred_insertion<'a>(
        &self,
        shreds: impl IntoIterator<
            Item = (Cow<'a, Shred>, /*is_repaired:*/ bool, BlockLocation),
            IntoIter: ExactSizeIterator,
        >,
        is_trusted: bool,
        shred_insertion_tracker: &mut ShredInsertionTracker<'a>,
        metrics: &mut BlockstoreInsertionMetrics,
    ) {
        let shreds = shreds.into_iter();
        metrics.num_shreds += shreds.len();
        let mut start = Measure::start("Shred insertion");
        for (shred, is_repaired, location) in shreds {
            let slot = shred.slot();
            let shred_source = if is_repaired {
                ShredSource::Repaired
            } else {
                ShredSource::Turbine
            };
            match shred.shred_type() {
                ShredType::Data => {
                    match self.check_insert_data_shred(
                        shred,
                        location,
                        shred_insertion_tracker,
                        is_trusted,
                        shred_source,
                    ) {
                        Err(InsertDataShredError::Exists) => {
                            if is_repaired {
                                metrics.num_repaired_data_shreds_exists += 1;
                            } else {
                                metrics.num_turbine_data_shreds_exists += 1;
                            }
                        }
                        Err(InsertDataShredError::InvalidShred) => {
                            self.mark_slot_dead_if_not_full(
                                slot,
                                location,
                                shred_insertion_tracker,
                            );
                            metrics.num_data_shreds_invalid += 1
                        }
                        Err(InsertDataShredError::BlockstoreError(err)) => {
                            metrics.num_data_shreds_blockstore_error += 1;
                            error!("blockstore error: {err}");
                        }
                        Ok(()) => {
                            if is_repaired {
                                metrics.num_repair += 1;
                            }
                            metrics.num_inserted += 1;
                        }
                    };
                }
                ShredType::Code => {
                    // Block id based repair (the source for populating the Alternate column) cannot receive
                    // coding shreds. Thus if we receive a coding shred, it must be for `BlockLocation::Original`
                    debug_assert_matches!(location, BlockLocation::Original);
                    match self.check_insert_coding_shred(
                        shred,
                        shred_insertion_tracker,
                        is_trusted,
                        shred_source,
                    ) {
                        Err(InsertCodingShredError::Exists) => {
                            metrics.num_coding_shreds_exists += 1;
                        }
                        Err(InsertCodingShredError::InvalidShred) => {
                            self.mark_slot_dead_if_not_full(
                                slot,
                                location,
                                shred_insertion_tracker,
                            );
                            metrics.num_coding_shreds_invalid += 1;
                        }
                        Err(InsertCodingShredError::InvalidErasureConfig) => {
                            self.mark_slot_dead_if_not_full(
                                slot,
                                location,
                                shred_insertion_tracker,
                            );
                            metrics.num_coding_shreds_invalid_erasure_config += 1;
                        }
                        Err(InsertCodingShredError::BlockstoreError(err)) => {
                            metrics.num_coding_shreds_blockstore_error += 1;
                            error!("blockstore error during coding shred insertion: {err}");
                        }
                        Ok(()) => {
                            metrics.num_coding_shreds_inserted += 1;
                            metrics.num_inserted += 1;
                        }
                    }
                }
            };
        }
        start.stop();

        metrics.insert_shreds_elapsed_us += start.as_us();
    }

    /// Attempt shred recovery for erasure metas
    /// Recovery rules:
    /// 1. Only try recovery around indexes for which new data or coding shreds are received
    /// 2. For new data shreds, check if an erasure set exists. If not, don't try recovery
    /// 3. Before trying recovery, check if enough number of shreds have been received
    ///    3a. Enough number of shreds = (#data + #coding shreds) > erasure.num_data
    /// 4. Only perform rceovery in the Original column
    ///
    /// Returns (recovered_shreds, recovered_data_shreds)
    fn try_shred_recovery(
        &self,
        erasure_metas: &BTreeMap<ErasureSetId, WorkingEntry<ErasureMeta>>,
        index_working_set: &HashMap<(BlockLocation, u64), IndexMetaWorkingSetEntry>,
        prev_inserted_shreds: &HashMap<(BlockLocation, ShredId), Cow<'_, Shred>>,
        shred_recovery_ctx: &mut ShredRecoveryContext,
    ) -> (
        /* recovered_shreds */ Vec<Payload>,
        /* recovered_data_shreds */ Vec<Shred>,
    ) {
        let erasure_metas_to_recover = erasure_metas
            .iter()
            .filter_map(|(erasure_set, working_erasure_meta)| {
                let erasure_meta = working_erasure_meta.as_ref();
                let slot = erasure_set.slot();
                let index_meta_entry = index_working_set.get(&(BlockLocation::Original, slot))?;
                let index = &index_meta_entry.index;
                erasure_meta
                    .should_recover_shreds(index)
                    .then_some((index, erasure_meta))
            })
            .collect_vec();

        let max_recovered_shreds = erasure_metas_to_recover.len() * DATA_SHREDS_PER_FEC_BLOCK;
        let mut recovered_shreds = Vec::with_capacity(max_recovered_shreds);
        let mut recovered_data_shreds = Vec::with_capacity(max_recovered_shreds);
        for (index, erasure_meta) in erasure_metas_to_recover {
            let _ = self
                .recover_shreds(
                    index,
                    erasure_meta,
                    prev_inserted_shreds,
                    shred_recovery_ctx,
                    &mut recovered_shreds,
                    &mut recovered_data_shreds,
                )
                .inspect_err(|e| {
                    info!(
                        "Unable to perform shred recovery for slot {} fec set {}: {e:?}",
                        index.slot,
                        erasure_meta.fec_set_index()
                    )
                });
        }
        (recovered_shreds, recovered_data_shreds)
    }

    /// Attempts shred recovery and does the following for recovered data
    /// shreds:
    /// 1. Verify signatures
    /// 2. Insert into blockstore
    /// 3. Send for retransmit.
    ///
    /// Note: We only perform recovery for the Original shred column
    fn handle_shred_recovery(
        &self,
        shred_recovery_context: &mut ShredRecoveryContext,
        shred_insertion_tracker: &mut ShredInsertionTracker,
        is_trusted: bool,
        metrics: &mut BlockstoreInsertionMetrics,
    ) {
        let mut start = Measure::start("Shred recovery");
        let (recovered_shreds, recovered_data_shreds) = self.try_shred_recovery(
            &shred_insertion_tracker.erasure_metas,
            &shred_insertion_tracker.index_working_set,
            &shred_insertion_tracker.just_inserted_shreds,
            shred_recovery_context,
        );
        shred_recovery_context.try_retransmit_shreds(recovered_shreds);

        metrics.num_recovered += recovered_data_shreds.len();
        for shred in recovered_data_shreds {
            let slot = shred.slot();
            *match self.check_insert_data_shred(
                Cow::Owned(shred),
                BlockLocation::Original,
                shred_insertion_tracker,
                is_trusted,
                ShredSource::Recovered,
            ) {
                Err(InsertDataShredError::Exists) => &mut metrics.num_recovered_exists,
                Err(InsertDataShredError::InvalidShred) => {
                    self.mark_slot_dead_if_not_full(
                        slot,
                        BlockLocation::Original,
                        shred_insertion_tracker,
                    );
                    &mut metrics.num_recovered_failed_invalid
                }
                Err(InsertDataShredError::BlockstoreError(err)) => {
                    error!("blockstore error: {err}");
                    &mut metrics.num_recovered_blockstore_error
                }
                Ok(()) => &mut metrics.num_recovered_inserted,
            } += 1;
        }
        start.stop();
        metrics.shred_recovery_elapsed_us += start.as_us();
    }

    /// Check the chained merkle root consistency between newly inserted FEC sets.
    /// Note: This check is not performed on Alternate columns as the shreds are already pre verified.
    fn check_chained_merkle_root_consistency(
        &self,
        shred_insertion_tracker: &mut ShredInsertionTracker,
    ) {
        for ((location, erasure_set), working_merkle_root_meta) in
            shred_insertion_tracker.merkle_root_metas.iter()
        {
            if !working_merkle_root_meta.should_write() {
                // Not a new merkle root meta
                continue;
            }
            let (slot, _) = erasure_set.store_key();

            // Note: this check is not performed on Alternate columns as they are already validated
            // via a separate merkle proof on repair ingest
            if !matches!(location, BlockLocation::Original) {
                continue;
            }

            let merkle_root_meta = working_merkle_root_meta.as_ref();
            let shred_id = ShredId::new(
                slot,
                merkle_root_meta.first_received_shred_index(),
                merkle_root_meta.first_received_shred_type(),
            );
            let shred = shred_insertion_tracker
                .just_inserted_shreds
                .get(&(*location, shred_id))
                .expect("Merkle root meta was just created, initial shred must exist");

            // Forward check for SIMD-0340
            if let Some(next_erasure_set) = erasure_set.next_fec_set()
                && !self.check_forward_chained_merkle_root_consistency(
                    shred,
                    next_erasure_set,
                    &shred_insertion_tracker.just_inserted_shreds,
                    &shred_insertion_tracker.merkle_root_metas,
                )
            {
                shred_insertion_tracker.duplicate_shreds.push(
                    PossibleDuplicateShred::FixedFECChainedMerkleRootConflict(slot),
                );
                continue;
            }

            // Backwards check for SIMD-0340
            if let Some(prev_shred_id) = self
                .previous_fec_set_shred_id(erasure_set, &shred_insertion_tracker.merkle_root_metas)
                && !self.check_backwards_chained_merkle_root_consistency(
                    shred,
                    prev_shred_id,
                    &shred_insertion_tracker.just_inserted_shreds,
                )
            {
                shred_insertion_tracker.duplicate_shreds.push(
                    PossibleDuplicateShred::FixedFECChainedMerkleRootConflict(slot),
                );
                continue;
            }
        }
    }

    /// Computes and adds DoubleMerkleMeta to the write_batch for newly completed
    /// slots after block markers have been enabled.
    fn compute_double_merkle_meta_for_newly_completed_slots(
        &self,
        shred_insertion_tracker: &mut ShredInsertionTracker,
    ) -> Result<()> {
        for (&(location, slot), slot_meta_entry) in
            shred_insertion_tracker.slot_meta_working_set.iter()
        {
            let meta = RefCell::borrow(&*slot_meta_entry.new_slot_meta);
            if !is_newly_completed_slot(&meta, &slot_meta_entry.old_slot_meta) {
                continue;
            }

            self.build_double_merkle_meta_in_batch(
                slot,
                location,
                meta.last_index.expect("Slot is full"),
                &shred_insertion_tracker.slot_meta_working_set,
                &shred_insertion_tracker.merkle_root_metas,
                &mut shred_insertion_tracker.write_batch,
            )?;
        }
        Ok(())
    }

    /// Builds DoubleMerkleMeta for a completed slot and adds it to the write batch.
    /// Expects the block to be full
    fn build_double_merkle_meta_in_batch(
        &self,
        slot: Slot,
        location: BlockLocation,
        last_index: u64,
        slot_metas: &HashMap<(BlockLocation, Slot), SlotMetaWorkingSetEntry>,
        merkle_root_metas: &HashMap<(BlockLocation, ErasureSetId), WorkingEntry<MerkleRootMeta>>,
        write_batch: &mut WriteBatch,
    ) -> Result<()> {
        let fec_set_count = last_index as u32 / (DATA_SHREDS_PER_FEC_BLOCK as u32) + 1;
        let merkle_tree = self.build_double_merkle_tree(
            slot,
            location,
            fec_set_count,
            Some(slot_metas),
            Some(merkle_root_metas),
        )?;

        let double_merkle_root = *merkle_tree.root();
        // We don't build the proofs here as they are only needed to serve repair requests.
        // These will be built later when serving block-id repair requests.
        let proofs = Vec::new();

        // Create and add DoubleMerkleMeta to write batch
        let double_merkle_meta = DoubleMerkleMeta {
            double_merkle_root,
            fec_set_count,
            proofs,
        };

        self.double_merkle_meta_cf
            .put_in_batch(write_batch, (slot, location), &double_merkle_meta)
    }

    /// Builds the double merkle tree for a completed block (expects the block to be full)
    fn build_double_merkle_tree(
        &self,
        slot: Slot,
        location: BlockLocation,
        fec_set_count: u32,
        slot_metas: Option<&HashMap<(BlockLocation, Slot), SlotMetaWorkingSetEntry>>,
        merkle_root_metas: Option<
            &HashMap<(BlockLocation, ErasureSetId), WorkingEntry<MerkleRootMeta>>,
        >,
    ) -> Result<MerkleTree> {
        // Get the parent info from tracker if specified first, then fall back to db
        let Some((Some(parent_slot), parent_block_id)) =
            self.get_parent_info_from_tracker_or_db(slot, location, slot_metas)?
        else {
            // Something has gone wrong here - the block is full yet the slot meta/parent slot info was missing!
            error!(
                "slot {slot} location {location} was full, yet the slot meta is missing / \
                 incomplete"
            );
            return Err(BlockstoreError::ParentInfoUnavailable(slot, location));
        };

        // Collect merkle roots for each FEC set
        let merkle_tree_leaves = (0..fec_set_count)
            .map(|i| {
                let fec_set_index = i * DATA_SHREDS_PER_FEC_BLOCK as u32;
                let erasure_set = ErasureSetId::new(slot, fec_set_index);
                self.get_merkle_root_from_tracker_or_db(location, erasure_set, merkle_root_metas)
                    .map_err(|_| shred::Error::InvalidMerkleRoot)
                    .and_then(|mr| mr.ok_or(shred::Error::InvalidMerkleRoot))
            })
            // Add parent info as the last leaf. The `fec_set_count` is bound
            // into this leaf so that an adversary cannot convince a verifier
            // that the count is off-by-one when the number of FEC sets is
            // even (which makes the total leaf count odd and causes the last
            // leaf to be hashed with itself during tree construction).
            .chain(std::iter::once(Ok(hashv(&[
                &parent_slot.to_le_bytes(),
                parent_block_id.as_ref(),
                &fec_set_count.to_le_bytes(),
            ]))));

        MerkleTree::try_new_with_len(merkle_tree_leaves, fec_set_count as usize + 1)
            .map_err(|_| BlockstoreError::MerkleTreeConstructionFailure(slot, location))
    }

    /// Populates the proofs for the block at `slot` `location` into the `double_merkle_meta`
    fn populate_double_merkle_meta_proofs(
        &self,
        slot: Slot,
        location: BlockLocation,
        double_merkle_meta: &mut DoubleMerkleMeta,
    ) -> Result<()> {
        let merkle_tree = self.build_double_merkle_tree(
            slot,
            location,
            double_merkle_meta.fec_set_count,
            None,
            None,
        )?;
        let tree_size = double_merkle_meta.fec_set_count as usize + 1;
        let proof_len_bytes = get_proof_size(tree_size) as usize * SIZE_OF_MERKLE_PROOF_ENTRY;
        let mut proofs = Vec::with_capacity(tree_size * proof_len_bytes);

        for leaf_index in 0..tree_size {
            for proof_entry in merkle_tree.make_merkle_proof(leaf_index, tree_size) {
                let proof_entry = proof_entry
                    .map_err(|_| BlockstoreError::MerkleProofConstructionFailure(slot, location))?;
                proofs.extend_from_slice(proof_entry);
            }
        }

        double_merkle_meta.proofs = proofs;

        Ok(())
    }

    /// Gets the merkle root from the tracker if available, otherwise from the db.
    fn get_merkle_root_from_tracker_or_db(
        &self,
        location: BlockLocation,
        erasure_set: ErasureSetId,
        merkle_root_metas: Option<
            &HashMap<(BlockLocation, ErasureSetId), WorkingEntry<MerkleRootMeta>>,
        >,
    ) -> Result<Option<Hash>> {
        let err = || {
            BlockstoreError::MissingMerkleRoot(
                erasure_set.slot(),
                erasure_set.fec_set_index() as u64,
            )
        };
        // First check the tracker if available
        if let Some(working_entry) =
            merkle_root_metas.and_then(|mm| mm.get(&(location, erasure_set)))
        {
            return Ok(Some(working_entry.as_ref().merkle_root().ok_or_else(err)?));
        }

        // Fall back to the db
        self.merkle_root_meta_from_location(erasure_set, location)
            .and_then(|maybe_meta| {
                maybe_meta
                    .map(|meta| meta.merkle_root().ok_or_else(err))
                    .transpose()
            })
    }

    /// Gets the parent info for this block using the slot meta from the tracker if available, otherwise from the db
    fn get_parent_info_from_tracker_or_db(
        &self,
        slot: Slot,
        location: BlockLocation,
        slot_metas: Option<&HashMap<(BlockLocation, Slot), SlotMetaWorkingSetEntry>>,
    ) -> Result<Option<(Option<Slot>, Hash)>> {
        if let Some(working_entry) = slot_metas.and_then(|sm| sm.get(&(location, slot))) {
            // First check the tracker if available
            let entry = RefCell::borrow(&*working_entry.new_slot_meta);
            Ok(Some((entry.parent_slot, entry.parent_block_id)))
        } else if let Some(meta) = self.meta_from_location(slot, location)? {
            // Fall back to DB
            Ok(Some((meta.parent_slot, meta.parent_block_id)))
        } else {
            Ok(None)
        }
    }

    fn commit_updates_to_write_batch(
        &self,
        shred_insertion_tracker: &mut ShredInsertionTracker,
        metrics: &mut BlockstoreInsertionMetrics,
    ) -> Result<(
        /* signal slot updates */ bool,
        /* slots updated */ Vec<u64>,
        /* update parent signals */ Vec<UpdateParentSignal>,
    )> {
        let mut start = Measure::start("Commit Working Sets");
        let (should_signal, newly_completed_slots, update_parent_signals) = self
            .commit_slot_meta_working_set(
                &shred_insertion_tracker.slot_meta_working_set,
                &mut shred_insertion_tracker.write_batch,
            )?;

        for (erasure_set, working_erasure_meta) in &shred_insertion_tracker.erasure_metas {
            if !working_erasure_meta.should_write() {
                // No need to rewrite the column
                continue;
            }
            let (slot, fec_set_index) = erasure_set.store_key();
            self.erasure_meta_cf.put_in_batch(
                &mut shred_insertion_tracker.write_batch,
                (slot, u64::from(fec_set_index)),
                working_erasure_meta.as_ref(),
            )?;
        }

        for (&(location, erasure_set), working_merkle_root_meta) in
            &shred_insertion_tracker.merkle_root_metas
        {
            if !working_merkle_root_meta.should_write() {
                // No need to rewrite the column
                continue;
            }
            self.put_merkle_root_meta_in_batch(
                &mut shred_insertion_tracker.write_batch,
                erasure_set,
                location,
                working_merkle_root_meta.as_ref(),
            )?;
        }

        for (&(location, slot), index_working_set_entry) in
            shred_insertion_tracker.index_working_set.iter()
        {
            if index_working_set_entry.did_insert_occur {
                self.put_index_in_batch(
                    &mut shred_insertion_tracker.write_batch,
                    slot,
                    location,
                    &index_working_set_entry.index,
                )?;
            }
        }
        start.stop();
        metrics.commit_working_sets_elapsed_us += start.as_us();

        Ok((should_signal, newly_completed_slots, update_parent_signals))
    }

    /// The main helper function that performs the shred insertion logic
    /// and updates corresponding meta-data.
    ///
    /// This function updates the following column families:
    ///   - [`cf::DeadSlots`]: mark a slot as "dead" for all shred insertion
    ///     failures except [`InsertDataShredError::Exists`] and
    ///     [`InsertCodingShredError::Exists`].
    ///   - [`cf::ShredData`]: stores data shreds (in check_insert_data_shreds).
    ///   - [`cf::ShredCode`]: stores coding shreds (in check_insert_coding_shreds).
    ///   - [`cf::SlotMeta`]: the SlotMeta of the input `shreds` and their related
    ///     shreds are updated.  Specifically:
    ///     - `handle_chaining()` updates `cf::SlotMeta` in two ways.  First, it
    ///       updates the in-memory slot_meta_working_set, which will later be
    ///       persisted in commit_slot_meta_working_set().  Second, for the newly
    ///       chained slots (updated inside handle_chaining_for_slot()), it will
    ///       directly persist their slot-meta into `cf::SlotMeta`.
    ///     - In `commit_slot_meta_working_set()`, persists everything stored
    ///       in the in-memory structure slot_meta_working_set, which is updated
    ///       by both `check_insert_data_shred()` and `handle_chaining()`.
    ///   - [`cf::Orphans`]: add or remove the ID of a slot to `cf::Orphans`
    ///     if it becomes / is no longer an orphan slot in `handle_chaining()`.
    ///   - [`cf::ErasureMeta`]: the associated ErasureMeta of the coding and data
    ///     shreds inside `shreds` will be updated and committed to
    ///     `cf::ErasureMeta`.
    ///   - [`cf::MerkleRootMeta`]: the associated MerkleRootMeta of the coding and data
    ///     shreds inside `shreds` will be updated and committed to
    ///     `cf::MerkleRootMeta`.
    ///   - [`cf::Index`][]: stores (slot id, index to the index_working_set_entry)
    ///     pair to the `cf::Index` column family for each index_working_set_entry which insert did occur in this function call.
    ///
    /// Arguments:
    ///  - `shreds`: the shreds to be inserted, alongside their repaired flag and
    ///    insertion location.
    ///  - `is_trusted`: whether the shreds come from a trusted source. If this
    ///    is set to true, then the function will skip the shred duplication and
    ///    integrity checks.
    ///  - `shred_recovery_context`: recovery-time dependencies and policy for
    ///    erasure recovery. `None` disables recovery.
    ///  - `metrics`: the metric for reporting detailed stats
    ///
    /// On success, the function returns an Ok result with a vector of
    /// `CompletedDataSetInfo` and a vector of its corresponding index in the
    /// input `shreds` vector.
    fn do_insert_shreds<'a>(
        &self,
        shreds: impl IntoIterator<
            Item = (Cow<'a, Shred>, /*is_repaired:*/ bool, BlockLocation),
            IntoIter: ExactSizeIterator,
        >,
        is_trusted: bool,
        // When inserting own shreds during leader slots, we shouldn't try to
        // recover shreds. If shreds are not to be recovered we don't need the
        // retransmit channel either. Otherwise, if we are inserting shreds
        // from another leader, we need to try erasure recovery and retransmit
        // recovered shreds.
        shred_recovery_context: Option<&mut ShredRecoveryContext>,
        metrics: &mut BlockstoreInsertionMetrics,
    ) -> Result<InsertResults> {
        let mut total_start = Measure::start("Total elapsed");

        // Acquire the insertion lock
        let mut start = Measure::start("Blockstore lock");
        let lock = self.insert_shreds_lock.lock().unwrap();
        start.stop();
        metrics.insert_lock_elapsed_us += start.as_us();

        let result = self.do_insert_shreds_locked(
            &lock,
            shreds,
            is_trusted,
            shred_recovery_context,
            metrics,
        );

        // Roll up metrics
        total_start.stop();
        metrics.total_elapsed_us += total_start.as_us();

        result
    }

    /// Core shred insertion logic.
    fn do_insert_shreds_locked<'a>(
        &self,
        _insert_shreds_lock: &MutexGuard<'_, ()>,
        shreds: impl IntoIterator<
            Item = (Cow<'a, Shred>, /*is_repaired:*/ bool, BlockLocation),
            IntoIter: ExactSizeIterator,
        >,
        is_trusted: bool,
        shred_recovery_context: Option<&mut ShredRecoveryContext>,
        metrics: &mut BlockstoreInsertionMetrics,
    ) -> Result<InsertResults> {
        let shreds = shreds.into_iter();
        let mut shred_insertion_tracker =
            ShredInsertionTracker::new(shreds.len(), self.get_write_batch()?);

        self.attempt_shred_insertion(shreds, is_trusted, &mut shred_insertion_tracker, metrics);
        if let Some(shred_recovery_context) = shred_recovery_context {
            self.handle_shred_recovery(
                shred_recovery_context,
                &mut shred_insertion_tracker,
                is_trusted,
                metrics,
            );
        }
        // Handle chaining for the members of the slot_meta_working_set that
        // were inserted into, drop the others.
        self.handle_chaining(
            &mut shred_insertion_tracker.write_batch,
            &mut shred_insertion_tracker.slot_meta_working_set,
            metrics,
        )?;

        self.check_chained_merkle_root_consistency(&mut shred_insertion_tracker);

        // Compute DoubleMerkleMeta for any newly completed slots so it's committed atomically
        self.compute_double_merkle_meta_for_newly_completed_slots(&mut shred_insertion_tracker)?;

        let (should_signal, newly_completed_slots, update_parent_signals) =
            self.commit_updates_to_write_batch(&mut shred_insertion_tracker, metrics)?;

        // Write out the accumulated batch.
        let mut start = Measure::start("Write Batch");
        self.write_batch(shred_insertion_tracker.write_batch)?;
        start.stop();
        metrics.write_batch_elapsed_us += start.as_us();

        send_signals(
            &self.new_shreds_signals.lock().unwrap(),
            &self.completed_slots_senders.lock().unwrap(),
            &self.update_parent_signals.lock().unwrap(),
            should_signal,
            newly_completed_slots,
            update_parent_signals,
        );

        metrics.index_meta_time_us += shred_insertion_tracker.index_meta_time_us;

        Ok(InsertResults {
            completed_data_set_infos: shred_insertion_tracker.newly_completed_data_sets,
            duplicate_shreds: shred_insertion_tracker.duplicate_shreds,
        })
    }

    /// Simlar to `insert_shreds_at_location_handle_duplicate`  but always inserts
    /// shreds in the original column specified by `BlockLocation::Original`
    #[cfg(feature = "dev-context-only-utils")]
    pub fn insert_shreds_handle_duplicate<'a, F>(
        &self,
        shreds: impl IntoIterator<
            Item = (Cow<'a, Shred>, /*is_repaired:*/ bool),
            IntoIter: ExactSizeIterator,
        >,
        is_trusted: bool,
        shred_recovery_context: &mut ShredRecoveryContext,
        handle_duplicate: &F,
        metrics: &mut BlockstoreInsertionMetrics,
    ) -> Result<Vec<CompletedDataSetInfo>>
    where
        F: Fn(PossibleDuplicateShred),
    {
        self.insert_shreds_at_location_handle_duplicate(
            shreds
                .into_iter()
                .map(|(shred, is_repaired)| (shred, is_repaired, BlockLocation::Original)),
            is_trusted,
            shred_recovery_context,
            handle_duplicate,
            metrics,
        )
    }

    /// Inserts `shreds` into the column specified by  the `BlockLocation`.
    ///
    /// Additionally attempts to recover and retransmit recovered shreds (also identifying
    /// and handling duplicate shreds). Broadcast stage should instead call
    /// Blockstore::insert_shreds when inserting own shreds during leader slots.
    pub fn insert_shreds_at_location_handle_duplicate<'a, F>(
        &self,
        shreds: impl IntoIterator<
            Item = (Cow<'a, Shred>, /*is_repaired:*/ bool, BlockLocation),
            IntoIter: ExactSizeIterator,
        >,
        is_trusted: bool,
        shred_recovery_context: &mut ShredRecoveryContext,
        handle_duplicate: &F,
        metrics: &mut BlockstoreInsertionMetrics,
    ) -> Result<Vec<CompletedDataSetInfo>>
    where
        F: Fn(PossibleDuplicateShred),
    {
        let InsertResults {
            completed_data_set_infos,
            duplicate_shreds,
        } = self.do_insert_shreds(shreds, is_trusted, Some(shred_recovery_context), metrics)?;

        for shred in duplicate_shreds {
            handle_duplicate(shred);
        }

        Ok(completed_data_set_infos)
    }

    pub fn add_new_shred_signal(&self, s: Sender<bool>) {
        self.new_shreds_signals.lock().unwrap().push(s);
    }

    pub fn add_completed_slots_signal(&self, s: CompletedSlotsSender) {
        self.completed_slots_senders.lock().unwrap().push(s);
    }

    pub fn add_update_parent_signal(&self, s: UpdateParentSender) {
        self.update_parent_signals.lock().unwrap().push(s);
    }

    pub fn get_new_shred_signals_len(&self) -> usize {
        self.new_shreds_signals.lock().unwrap().len()
    }

    pub fn get_new_shred_signal(&self, index: usize) -> Option<Sender<bool>> {
        self.new_shreds_signals.lock().unwrap().get(index).cloned()
    }

    pub fn drop_signal(&self) {
        self.new_shreds_signals.lock().unwrap().clear();
        self.completed_slots_senders.lock().unwrap().clear();
        self.update_parent_signals.lock().unwrap().clear();
    }

    /// Clear `slot` from the Blockstore
    ///
    /// This function currently requires `insert_shreds_lock`, as both
    /// `clear_unconfirmed_slot()` and `insert_shreds_handle_duplicate()`
    /// try to perform read-modify-write operation on [`cf::SlotMeta`] column
    /// family.
    pub fn clear_unconfirmed_slot(&self, slot: Slot) {
        self.clear_unconfirmed_slots(slot, slot);
    }

    /// Atomically clear a range of `slot` inclusive, similar to `Blockstore::clear_unconfirmed_slot`
    /// Holds the shred lock during the entire purge.
    pub fn clear_unconfirmed_slots(&self, start: Slot, end: Slot) {
        let _lock = self.insert_shreds_lock.lock().unwrap();
        for slot in start..=end {
            // Purge the slot and insert an empty `SlotMeta` with only the `next_slots` field preserved.
            // Shreds inherently know their parent slot, and a parent's SlotMeta `next_slots` list
            // will be updated when the child is inserted (see `Blockstore::handle_chaining()`).
            // However, we are only purging and repairing the parent slot here. Since the child will not be
            // reinserted the chaining will be lost. In order for bank forks discovery to ingest the child,
            // we must retain the chain by preserving `next_slots`.
            match self.purge_slot_cleanup_chaining(slot) {
                Ok(_) => {}
                Err(BlockstoreError::SlotUnavailable) => {
                    error!("clear_unconfirmed_slot() called on slot {slot} with no SlotMeta")
                }
                Err(e) => panic!("Purge database operations failed {e}"),
            }
        }
    }

    /// Helper to copy shreds from one location to another.
    /// Reads all data shreds from `from_location` and inserts them at `to_location`.
    fn copy_shreds_locked(
        &self,
        lock: &std::sync::MutexGuard<'_, ()>,
        slot: Slot,
        from_location: BlockLocation,
        to_location: BlockLocation,
    ) -> Result<()> {
        let shreds = self.get_data_shreds_for_slot_from_location(
            slot,
            /* start_index */ 0,
            from_location,
        )?;

        let shreds = shreds.into_iter().map(|shred| {
            (Cow::Owned(shred), /*is_repaired:*/ false, to_location)
        });
        self.do_insert_shreds_locked(
            lock,
            shreds,
            true, // is_trusted
            None, // should_recover_shreds
            &mut BlockstoreInsertionMetrics::default(),
        )?;

        Ok(())
    }

    /// Switch the block in `slot` from an alternate location to the original location.
    /// This atomically:
    /// 1. Back up the original column data if it's a valid block that we don't have
    /// 2. Purges the original column data while preserving alternate columns
    /// 3. Copies shreds from the alternate location to the original location
    /// 4. Verify that the switch was successful
    ///
    /// Holds `switch_block_lock` and `insert_shreds_lock` for the entire operation.
    ///
    /// Assumes that the block at `location` is full.
    pub fn switch_block_from_alternate(
        &self,
        slot: Slot,
        from_location: BlockLocation,
    ) -> Result<()> {
        assert!(
            !matches!(from_location, BlockLocation::Original),
            "Cannot switch from Original location"
        );

        let mut metrics = BlockstoreSwitchBankMetrics::default();

        let mut total_measure = Measure::start("switch_block_from_alternate_total");

        let (_switch_block_lock, switch_lock_time_us) = measure_us!(self.switch_block_lock.lock());
        let (lock, insert_lock_time_us) = measure_us!(self.insert_shreds_lock.lock().unwrap());
        metrics.lock_elapsed_us = switch_lock_time_us.saturating_add(insert_lock_time_us);

        // 1. Backup the original block if needed
        let mut backup_measure = Measure::start("switch_block_from_alternate_backup");
        if let Some(dmr) = self.get_double_merkle_root(slot, BlockLocation::Original)? {
            let backup_location = BlockLocation::Alternate { block_id: dmr };
            if self
                .get_double_merkle_root(slot, backup_location)?
                .is_none()
            {
                self.copy_shreds_locked(&lock, slot, BlockLocation::Original, backup_location)?;
            }
        }
        backup_measure.stop();
        metrics.backup_elapsed_us = backup_measure.as_us();

        // 2. Purge the original column data, keeping alternate columns intact
        let mut purge_measure = Measure::start("switch_block_from_alternate_purge");
        self.purge_slot_cleanup_chaining_keep_alt(slot)
            .or_else(|err| {
                if matches!(err, BlockstoreError::SlotUnavailable) {
                    // There was no block in the original column, continue to copying
                    Ok(())
                } else {
                    Err(err)
                }
            })?;
        purge_measure.stop();
        metrics.purge_elapsed_us = purge_measure.as_us();

        // 3. Copy shreds from alternate location to original
        let mut copy_measure = Measure::start("switch_block_from_alternate_copy");
        let alt_meta = self
            .meta_from_location(slot, from_location)?
            .expect("Alternate slot must have SlotMeta");
        debug_assert!(alt_meta.is_full(), "Alternate slot must be full");

        self.copy_shreds_locked(&lock, slot, from_location, BlockLocation::Original)?;
        copy_measure.stop();
        metrics.copy_elapsed_us = copy_measure.as_us();

        // 4. Verify the switch was successful
        debug_assert!(
            self.meta(slot)?
                .expect("Slot must have SlotMeta after switch")
                .is_full(),
            "Slot must be full after switch"
        );
        total_measure.stop();
        metrics.total_elapsed_us = total_measure.as_us();

        metrics.report_metrics(slot, from_location);
        Ok(())
    }

    // Bypasses erasure recovery becuase it is called from broadcast stage
    // when inserting own shreds during leader slots. Stores all shreds in the original column.
    pub fn insert_cow_shreds<'a>(
        &self,
        shreds: impl IntoIterator<Item = Cow<'a, Shred>, IntoIter: ExactSizeIterator>,
        is_trusted: bool,
    ) -> Result<Vec<CompletedDataSetInfo>> {
        let shreds = shreds
            .into_iter()
            .map(|shred| (shred, /*is_repaired:*/ false, BlockLocation::Original));
        let insert_results = self.do_insert_shreds(
            shreds,
            is_trusted,
            None, // Skip recovery for locally produced shreds.
            &mut BlockstoreInsertionMetrics::default(),
        )?;
        Ok(insert_results.completed_data_set_infos)
    }

    // Test-only function.
    pub fn insert_shreds(
        &self,
        shreds: impl IntoIterator<Item = Shred, IntoIter: ExactSizeIterator>,
        is_trusted: bool,
    ) -> Result<Vec<CompletedDataSetInfo>> {
        let shreds = shreds.into_iter().map(Cow::Owned);
        self.insert_cow_shreds(shreds, is_trusted)
    }

    #[cfg(test)]
    fn insert_shred_return_duplicate(&self, shred: Shred) -> Vec<PossibleDuplicateShred> {
        let insert_results = self
            .do_insert_shreds(
                [(
                    Cow::Owned(shred),
                    /*is_repaired:*/ false,
                    BlockLocation::Original,
                )],
                false,
                None, // Skip recovery for this direct insertion path.
                &mut BlockstoreInsertionMetrics::default(),
            )
            .unwrap();
        insert_results.duplicate_shreds
    }

    /// Create an entry to the specified `write_batch` that performs coding shred
    /// insertion and associated metadata update. The function also updates
    /// in-memory copies of the associated metadata.
    ///
    /// Currently, this function must be invoked while holding
    /// `insert_shreds_lock` as it performs read-modify-write operations
    /// on multiple column families.
    ///
    /// On success, the resulting `write_batch` includes updates to
    /// [`cf::ShredCode`], while in-memory `index_working_set`, `erasure_metas`,
    /// and `merkle_root_metas` are updated so they can be committed later.
    ///
    /// On failure, this function returns [`InsertCodingShredError`]. Callers are
    /// responsible for marking the slot as dead.
    ///
    /// Arguments:
    /// - `shred`: the coding shred to insert.
    /// - `shred_insertion_tracker`: collection of shred insertion tracking data.
    /// - `is_trusted`: if false, duplicate and integrity checks are applied.
    /// - `shred_source`: the source of the shred.
    /// - `metrics`: insertion metrics to update.
    fn check_insert_coding_shred<'a>(
        &self,
        shred: Cow<'a, Shred>,
        shred_insertion_tracker: &mut ShredInsertionTracker<'a>,
        is_trusted: bool,
        shred_source: ShredSource,
    ) -> std::result::Result<(), InsertCodingShredError> {
        let slot = shred.slot();
        let shred_index = u64::from(shred.index());

        let ShredInsertionTracker {
            just_inserted_shreds,
            erasure_metas,
            merkle_root_metas,
            index_working_set,
            index_meta_time_us,
            duplicate_shreds,
            write_batch,
            ..
        } = shred_insertion_tracker;

        let index_meta_working_set_entry = self.get_index_meta_entry(
            slot,
            BlockLocation::Original,
            index_working_set,
            index_meta_time_us,
        )?;

        let index_meta = &mut index_meta_working_set_entry.index;
        let erasure_set = shred.erasure_set();

        if let HashMapEntry::Vacant(entry) =
            merkle_root_metas.entry((BlockLocation::Original, erasure_set))
            && let Some(meta) = self.merkle_root_meta(erasure_set).unwrap()
        {
            entry.insert(WorkingEntry::Clean(meta));
        }

        // This gives the index of first coding shred in this FEC block
        // So, all coding shreds in a given FEC block will have the same set index
        if !is_trusted {
            if index_meta.coding().contains(shred_index) {
                duplicate_shreds.push(PossibleDuplicateShred::Exists(shred.into_owned()));
                return Err(InsertCodingShredError::Exists);
            }

            if !Blockstore::should_insert_coding_shred(&shred, self.max_root()) {
                return Err(InsertCodingShredError::InvalidShred);
            }

            if let Some(merkle_root_meta) =
                merkle_root_metas.get(&(BlockLocation::Original, erasure_set))
            {
                // A previous shred has been inserted in this batch or in blockstore
                // Compare our current shred against the previous shred for potential
                // conflicts
                if !self.check_merkle_root_consistency(
                    just_inserted_shreds,
                    slot,
                    BlockLocation::Original,
                    merkle_root_meta.as_ref(),
                    &shred,
                    duplicate_shreds,
                ) {
                    return Err(InsertCodingShredError::InvalidShred);
                }
            }
        }

        let erasure_meta_entry = erasure_metas.entry(erasure_set).or_insert_with(|| {
            self.erasure_meta(erasure_set)
                .expect("Expect database get to succeed")
                .map(WorkingEntry::Clean)
                .unwrap_or_else(|| {
                    WorkingEntry::Dirty(ErasureMeta::from_coding_shred(&shred).unwrap())
                })
        });
        let erasure_meta = erasure_meta_entry.as_ref();

        if !erasure_meta.check_coding_shred(&shred) {
            if !self.has_duplicate_shreds_in_slot(slot) {
                if let Some(conflicting_shred) = self
                    .find_conflicting_coding_shred(&shred, slot, erasure_meta, just_inserted_shreds)
                    .map(Cow::into_owned)
                {
                    if let Err(e) = self.store_duplicate_slot(
                        slot,
                        conflicting_shred.clone(),
                        shred.payload().clone(),
                    ) {
                        warn!(
                            "Unable to store conflicting erasure meta duplicate proof for {slot} \
                             {erasure_set:?} {e}"
                        );
                    }

                    duplicate_shreds.push(PossibleDuplicateShred::ErasureConflict(
                        shred.as_ref().clone(),
                        conflicting_shred,
                    ));
                } else {
                    error!(
                        "Unable to find the conflicting coding shred that set {erasure_meta:?}. \
                         This should only happen in extreme cases where blockstore cleanup has \
                         caught up to the root. Skipping the erasure meta duplicate shred check"
                    );
                }
            }

            // ToDo: This is a potential slashing condition
            warn!("Received multiple erasure configs for the same erasure set!!!");
            warn!(
                "Slot: {}, shred index: {}, erasure_set: {:?}, is_duplicate: {}, stored config: \
                 {:#?}, new shred: {:#?}",
                slot,
                shred.index(),
                erasure_set,
                self.has_duplicate_shreds_in_slot(slot),
                erasure_meta.config(),
                shred,
            );
            return Err(InsertCodingShredError::InvalidErasureConfig);
        }

        self.slots_stats.record_shred(
            shred.slot(),
            BlockLocation::Original,
            shred.fec_set_index(),
            shred_source,
            None,
        );

        // insert coding shred into rocks
        self.insert_coding_shred(index_meta, &shred, write_batch)?;

        index_meta_working_set_entry.did_insert_occur = true;

        merkle_root_metas
            .entry((BlockLocation::Original, erasure_set))
            .or_insert(WorkingEntry::Dirty(MerkleRootMeta::from_shred(&shred)));

        if let HashMapEntry::Vacant(entry) =
            just_inserted_shreds.entry((BlockLocation::Original, shred.id()))
        {
            entry.insert(shred);
        }

        Ok(())
    }

    fn find_conflicting_coding_shred<'a>(
        &'a self,
        shred: &Shred,
        slot: Slot,
        erasure_meta: &ErasureMeta,
        just_received_shreds: &'a HashMap<(BlockLocation, ShredId), Cow<'_, Shred>>,
    ) -> Option<Cow<'a, shred::Payload>> {
        // Search for the shred which set the initial erasure config, either inserted,
        // or in the current batch in just_received_shreds.
        let index = erasure_meta.first_received_coding_shred_index()?;
        let shred_id = ShredId::new(slot, index, ShredType::Code);
        let maybe_shred = self.get_shred_from_just_inserted_or_db(
            just_received_shreds,
            shred_id,
            BlockLocation::Original,
        );

        if index != 0 || maybe_shred.is_some() {
            return maybe_shred;
        }

        // If we are using a blockstore created from an earlier version than 1.18.12,
        // `index` will be 0 as it was not yet populated, revert to a scan until  we no longer support
        // those blockstore versions.
        for coding_index in erasure_meta.coding_shreds_indices() {
            let maybe_shred = self.get_coding_shred(slot, coding_index);
            if let Ok(Some(shred_data)) = maybe_shred {
                let potential_shred = Shred::new_from_serialized_shred(shred_data).unwrap();
                if shred.erasure_mismatch(&potential_shred).unwrap() {
                    return Some(Cow::Owned(potential_shred.into_payload()));
                }
            } else if let Some(potential_shred) = {
                let key = ShredId::new(slot, u32::try_from(coding_index).unwrap(), ShredType::Code);
                just_received_shreds.get(&(BlockLocation::Original, key))
            } && shred.erasure_mismatch(potential_shred).unwrap()
            {
                return Some(Cow::Borrowed(potential_shred.payload()));
            }
        }
        None
    }

    /// Create an entry to the specified `write_batch` that performs shred
    /// insertion and associated metadata update.  The function also updates
    /// its in-memory copy of the associated metadata.
    ///
    /// Currently, this function must be invoked while holding
    /// `insert_shreds_lock` as it performs read-modify-write operations
    /// on multiple column families.
    ///
    /// The resulting `write_batch` may include updates to [`cf::ShredData`].
    /// Note that it will also update the in-memory copy
    /// of `erasure_metas`, `merkle_root_metas`, and `index_working_set`, which will
    /// later be used to update other column families such as [`cf::ErasureMeta`] and
    /// [`cf::Index`].
    ///
    /// On failure, this function returns [`InsertDataShredError`]. Callers are
    /// responsible for marking the slot as dead.
    ///
    /// Arguments:
    /// - `shred`: the shred to be inserted
    /// - `location`: the location to insert into
    /// - `shred_insertion_tracker`: collection of shred insertion tracking
    ///   data.
    /// - `is_trusted`: if false, this function will check whether the
    ///   input shred is duplicate.
    /// - `shred_source`: the source of the shred.
    fn check_insert_data_shred<'a>(
        &self,
        shred: Cow<'a, Shred>,
        location: BlockLocation,
        shred_insertion_tracker: &mut ShredInsertionTracker<'a>,
        is_trusted: bool,
        shred_source: ShredSource,
    ) -> std::result::Result<(), InsertDataShredError> {
        let slot = shred.slot();
        let shred_index = u64::from(shred.index());

        let ShredInsertionTracker {
            index_working_set,
            slot_meta_working_set,
            just_inserted_shreds,
            merkle_root_metas,
            duplicate_shreds,
            index_meta_time_us,
            erasure_metas,
            write_batch,
            newly_completed_data_sets,
        } = shred_insertion_tracker;

        let index_meta_working_set_entry =
            self.get_index_meta_entry(slot, location, index_working_set, index_meta_time_us)?;
        let index_meta = &mut index_meta_working_set_entry.index;
        let shred_parent_slot = shred
            .parent()
            .map_err(|_| InsertDataShredError::InvalidShred)?;
        let slot_meta_entry =
            self.get_slot_meta_entry(slot_meta_working_set, slot, location, shred_parent_slot)?;

        let slot_meta = &mut slot_meta_entry.new_slot_meta.borrow_mut();
        let erasure_set = shred.erasure_set();
        if let HashMapEntry::Vacant(entry) = merkle_root_metas.entry((location, erasure_set))
            && let Some(meta) = self
                .merkle_root_meta_from_location(erasure_set, location)
                .unwrap()
        {
            entry.insert(WorkingEntry::Clean(meta));
        }

        if !is_trusted {
            if Self::is_data_shred_present(&shred, slot_meta, index_meta.data()) {
                duplicate_shreds.push(PossibleDuplicateShred::Exists(shred.into_owned()));
                return Err(InsertDataShredError::Exists);
            }

            if shred.last_in_slot() && shred_index < slot_meta.received && !slot_meta.is_full() {
                // We got a last shred < slot_meta.received, which signals there's an alternative,
                // shorter version of the slot. Because also `!slot_meta.is_full()`, then this
                // means, for the current version of the slot, we might never get all the
                // shreds < the current last index, never replay this slot, and make no
                // progress (for instance if a leader sends an additional detached "last index"
                // shred with a very high index, but none of the intermediate shreds). Ideally, we would
                // just purge all shreds > the new last index slot, but because replay may have already
                // replayed entries past the newly detected "last" shred, the caller marks the slot
                // as dead and replay can dump and repair the correct version.
                warn!(
                    "Received *last* shred index {} less than previous shred index {}, and slot \
                     {} is not full",
                    shred_index, slot_meta.received, slot
                );
            }

            if !self.should_insert_data_shred(
                &shred,
                location,
                slot_meta,
                just_inserted_shreds,
                self.max_root(),
                shred_source,
                duplicate_shreds,
            ) {
                return Err(InsertDataShredError::InvalidShred);
            }

            if let Some(merkle_root_meta) = merkle_root_metas.get(&(location, erasure_set)) {
                // A previous shred has been inserted in this batch or in blockstore
                // Compare our current shred against the previous shred for potential
                // conflicts
                if !self.check_merkle_root_consistency(
                    just_inserted_shreds,
                    slot,
                    location,
                    merkle_root_meta.as_ref(),
                    &shred,
                    duplicate_shreds,
                ) {
                    // This indicates there is an alternate version of this block.
                    // Similar to the last index case above, we might never get all the
                    // shreds for our current version, never replay this slot, and make no
                    // progress. We cannot determine if we have the version that will eventually
                    // be complete, so the caller marks the slot as dead and replay can dump
                    // and repair the correct version.
                    return Err(InsertDataShredError::InvalidShred);
                }
            }
        }

        // Validate parent meta before persisting the shred. If the shred
        // contains conflicting parent information the slot is marked dead and
        // the shred is not inserted.
        if shred
            .index()
            .is_multiple_of(DATA_SHREDS_PER_FEC_BLOCK as u32)
            || shred.data_complete()
        {
            self.maybe_update_parent_info(
                &shred,
                shred_parent_slot,
                location,
                slot_meta,
                just_inserted_shreds,
                write_batch,
            )?;
        }

        let completed_data_sets = self.insert_data_shred(
            slot_meta,
            index_meta.data_mut(),
            &shred,
            location,
            write_batch,
            shred_source,
        );

        if matches!(location, BlockLocation::Original) {
            // We don't currently notify RPC when we complete data sets in alternate columns. This can be extended in the future
            // if necessary.
            newly_completed_data_sets.extend(completed_data_sets);
        }
        merkle_root_metas
            .entry((location, erasure_set))
            .or_insert(WorkingEntry::Dirty(MerkleRootMeta::from_shred(&shred)));
        just_inserted_shreds.insert((location, shred.id()), shred);
        index_meta_working_set_entry.did_insert_occur = true;
        slot_meta_entry.did_insert_occur = true;
        if let BTreeMapEntry::Vacant(entry) = erasure_metas.entry(erasure_set)
            && let Some(meta) = self.erasure_meta(erasure_set).unwrap()
        {
            entry.insert(WorkingEntry::Clean(meta));
        }
        Ok(())
    }

    fn should_insert_coding_shred(shred: &Shred, max_root: Slot) -> bool {
        debug_assert_matches!(shred.sanitize(), Ok(()));
        shred.is_code() && shred.slot() > max_root
    }

    fn insert_coding_shred(
        &self,
        index_meta: &mut Index,
        shred: &Shred,
        write_batch: &mut WriteBatch,
    ) -> Result<()> {
        let slot = shred.slot();
        let shred_index = u64::from(shred.index());

        // Assert guaranteed by integrity checks on the shred that happen before
        // `insert_coding_shred` is called
        debug_assert_matches!(shred.sanitize(), Ok(()));
        assert!(shred.is_code());

        // Commit step: commit all changes to the mutable structures at once, or none at all.
        // We don't want only a subset of these changes going through.
        self.code_shred_cf
            .put_bytes_in_batch(write_batch, (slot, shred_index), shred.payload());
        index_meta.coding_mut().insert(shred_index);

        Ok(())
    }

    fn is_data_shred_present(shred: &Shred, slot_meta: &SlotMeta, data_index: &ShredIndex) -> bool {
        let shred_index = u64::from(shred.index());
        // Check that the shred doesn't already exist in blockstore
        shred_index < slot_meta.consumed || data_index.contains(shred_index)
    }

    /// Finds the corresponding shred at `shred_id` in the just inserted
    /// shreds or the backing store. Returns None if there is no shred.
    fn get_shred_from_just_inserted_or_db<'a>(
        &'a self,
        just_inserted_shreds: &'a HashMap<(BlockLocation, ShredId), Cow<'_, Shred>>,
        shred_id: ShredId,
        location: BlockLocation,
    ) -> Option<Cow<'a, shred::Payload>> {
        let (slot, index, shred_type) = shred_id.unpack();
        match (just_inserted_shreds.get(&(location, shred_id)), shred_type) {
            (Some(shred), _) => Some(Cow::Borrowed(shred.payload())),
            // If it doesn't exist in the just inserted set, it must exist in
            // the backing store
            (_, ShredType::Data) => self
                .get_data_shred_from_location(slot, u64::from(index), location)
                .unwrap()
                .map(shred::Payload::from)
                .map(Cow::Owned),
            (_, ShredType::Code) => {
                // Coding shreds can only be present in the Original column
                assert_matches!(location, BlockLocation::Original);
                self.get_coding_shred(slot, u64::from(index))
                    .unwrap()
                    .map(shred::Payload::from)
                    .map(Cow::Owned)
            }
        }
    }

    /// Returns true if there is no merkle root conflict between
    /// the existing `merkle_root_meta` and `shred`
    ///
    /// Otherwise return false and if not already present, add duplicate proof to
    /// `duplicate_shreds`.
    fn check_merkle_root_consistency(
        &self,
        just_inserted_shreds: &HashMap<(BlockLocation, ShredId), Cow<'_, Shred>>,
        slot: Slot,
        location: BlockLocation,
        merkle_root_meta: &MerkleRootMeta,
        shred: &Shred,
        duplicate_shreds: &mut Vec<PossibleDuplicateShred>,
    ) -> bool {
        let new_merkle_root = shred.merkle_root().ok();
        if merkle_root_meta.merkle_root() == new_merkle_root {
            // No conflict, either both merkle shreds with same merkle root
            // or both legacy shreds with merkle_root `None`
            return true;
        }

        warn!(
            "Received conflicting merkle roots for slot: {}, erasure_set: {:?} original merkle \
             root meta {:?} vs conflicting merkle root {:?} shred index {} type {:?}. Reporting \
             as duplicate",
            slot,
            shred.erasure_set(),
            merkle_root_meta,
            new_merkle_root,
            shred.index(),
            shred.shred_type(),
        );

        if !self.has_duplicate_shreds_in_slot(slot) {
            let shred_id = ShredId::new(
                slot,
                merkle_root_meta.first_received_shred_index(),
                merkle_root_meta.first_received_shred_type(),
            );
            let Some(conflicting_shred) = self
                .get_shred_from_just_inserted_or_db(just_inserted_shreds, shred_id, location)
                .map(Cow::into_owned)
            else {
                error!(
                    "Shred {shred_id:?} indicated by merkle root meta {merkle_root_meta:?} is \
                     missing from blockstore. This should only happen in extreme cases where \
                     blockstore cleanup has caught up to the root. Skipping the merkle root \
                     consistency check"
                );
                return true;
            };
            if let Err(e) = self.store_duplicate_slot(
                slot,
                conflicting_shred.clone(),
                shred.clone().into_payload(),
            ) {
                warn!(
                    "Unable to store conflicting merkle root duplicate proof for {slot} {:?} {e}",
                    shred.erasure_set(),
                );
            }
            duplicate_shreds.push(PossibleDuplicateShred::MerkleRootConflict(
                shred.clone(),
                conflicting_shred,
            ));
        }
        false
    }

    /// Returns true if there is no chaining conflict between
    /// the `shred` and `merkle_root_meta` of the next FEC set,
    /// or if shreds from the next set are yet to be received.
    ///
    /// Otherwise return false and add duplicate proof to
    /// `duplicate_shreds`.
    ///
    /// This is intended to be used right after `shred`'s `erasure_meta`
    /// has been created for the first time.
    ///
    /// This check is only to be performed on the Original column, as Alternate
    /// column shreds are already pre verified
    fn check_forward_chained_merkle_root_consistency(
        &self,
        shred: &Shred,
        next_erasure_set: ErasureSetId,
        just_inserted_shreds: &HashMap<(BlockLocation, ShredId), Cow<'_, Shred>>,
        merkle_root_metas: &HashMap<(BlockLocation, ErasureSetId), WorkingEntry<MerkleRootMeta>>,
    ) -> bool {
        let slot = shred.slot();
        let erasure_set = shred.erasure_set();

        // If a shred from the next fec set has already been inserted, check the chaining
        let Some(next_merkle_root_meta) = merkle_root_metas
            .get(&(BlockLocation::Original, next_erasure_set))
            .map(WorkingEntry::as_ref)
            .map(Cow::Borrowed)
            .or_else(|| {
                self.merkle_root_meta(next_erasure_set)
                    .unwrap()
                    .map(Cow::Owned)
            })
        else {
            // No shred from the next fec set has been received
            return true;
        };
        let next_shred_id = ShredId::new(
            slot,
            next_merkle_root_meta.first_received_shred_index(),
            next_merkle_root_meta.first_received_shred_type(),
        );
        let Some(next_shred) = Self::get_shred_from_just_inserted_or_db(
            self,
            just_inserted_shreds,
            next_shred_id,
            BlockLocation::Original,
        )
        .map(Cow::into_owned) else {
            error!(
                "Shred {next_shred_id:?} is missing from blockstore. This should only happen in \
                 extreme cases where  blockstore cleanup has caught up to the root. Skipping the \
                 forward chained merkle root consistency check"
            );
            return true;
        };
        let merkle_root = shred.merkle_root().ok();
        let chained_merkle_root = shred::layout::get_chained_merkle_root(&next_shred);

        if !self.check_chaining(merkle_root, chained_merkle_root) {
            warn!(
                "Received conflicting chained merkle roots for slot: {slot}, shred \
                 {erasure_set:?} type {:?} has merkle root {merkle_root:?}, however next fec set \
                 shred {next_erasure_set:?} type {:?} chains to merkle root \
                 {chained_merkle_root:?}. Reporting as duplicate",
                shred.shred_type(),
                next_merkle_root_meta.first_received_shred_type(),
            );

            return false;
        }

        true
    }

    /// Returns true if there is no chaining conflict between
    /// the `shred` and `merkle_root_meta` of the previous FEC set,
    /// or if shreds from the previous set are yet to be received.
    ///
    /// Otherwise return false and add duplicate proof to
    /// `duplicate_shreds`.
    ///
    /// This is intended to be used right after `shred`'s `merkle_root_meta`
    /// has been created for the first time.
    ///
    /// This check is only to be performed on the Original column, as Alternate
    /// column shreds are already pre verified
    fn check_backwards_chained_merkle_root_consistency(
        &self,
        shred: &Shred,
        prev_shred_id: ShredId,
        just_inserted_shreds: &HashMap<(BlockLocation, ShredId), Cow<'_, Shred>>,
    ) -> bool {
        let slot = shred.slot();
        let fec_set_index = shred.fec_set_index();

        if fec_set_index == 0 {
            // The first fec set chains to the last fec set of the parent block.
            // Cross-slot chaining is validated during replay via
            // check_chained_block_id, so this function only handles
            // intra-slot chaining consistency.
            return true;
        }

        let Some(prev_shred) = Self::get_shred_from_just_inserted_or_db(
            self,
            just_inserted_shreds,
            prev_shred_id,
            BlockLocation::Original,
        )
        .map(Cow::into_owned) else {
            warn!(
                "Shred {prev_shred_id:?} is missing from blockstore. This can happen if \
                 blockstore cleanup has caught up to the root. Skipping the backwards chained \
                 merkle root consistency check"
            );
            return true;
        };
        let merkle_root = shred::layout::get_merkle_root(&prev_shred);
        let chained_merkle_root = shred.chained_merkle_root().ok();

        if !self.check_chaining(merkle_root, chained_merkle_root) {
            warn!(
                "Received conflicting chained merkle roots for slot: {slot}, shred {:?} type {:?} \
                 chains to merkle root {chained_merkle_root:?}, however previous fec set shred \
                 {prev_shred_id:?} has merkle root {merkle_root:?}. Reporting as duplicate",
                shred.erasure_set(),
                shred.shred_type(),
            );

            return false;
        }

        true
    }

    /// Checks if the chained merkle root == merkle root
    ///
    /// Returns true if no conflict, or if chained merkle roots are not enabled
    fn check_chaining(&self, merkle_root: Option<Hash>, chained_merkle_root: Option<Hash>) -> bool {
        chained_merkle_root.is_none()  // Chained merkle roots have not been enabled yet
            || chained_merkle_root == merkle_root
    }

    fn should_insert_data_shred(
        &self,
        shred: &Shred,
        location: BlockLocation,
        slot_meta: &SlotMeta,
        just_inserted_shreds: &HashMap<(BlockLocation, ShredId), Cow<'_, Shred>>,
        max_root: Slot,
        shred_source: ShredSource,
        duplicate_shreds: &mut Vec<PossibleDuplicateShred>,
    ) -> bool {
        let shred_index = u64::from(shred.index());
        let slot = shred.slot();
        let last_in_slot = if shred.last_in_slot() {
            debug!("got last in slot");
            true
        } else {
            false
        };
        debug_assert_matches!(shred.sanitize(), Ok(()));
        // Check that we do not receive shred_index >= than the last_index
        // for the slot
        if let Some(meta_last_index) = slot_meta.last_index
            && shred_index >= meta_last_index
        {
            if !self.has_duplicate_shreds_in_slot(slot) {
                let shred_id = ShredId::new(
                    slot,
                    u32::try_from(meta_last_index).unwrap(),
                    ShredType::Data,
                );
                let Some(ending_shred) = self
                    .get_shred_from_just_inserted_or_db(just_inserted_shreds, shred_id, location)
                    .map(Cow::into_owned)
                else {
                    error!(
                        "Last index data shred {shred_id:?} indicated by slot meta {slot_meta:?} \
                         is missing from blockstore. This should only happen in extreme cases \
                         where blockstore cleanup has caught up to the root. Skipping data shred \
                         insertion"
                    );
                    return false;
                };

                if self
                    .store_duplicate_slot(slot, ending_shred.clone(), shred.payload().clone())
                    .is_err()
                {
                    warn!("store duplicate error");
                }
                duplicate_shreds.push(PossibleDuplicateShred::LastIndexConflict(
                    shred.clone(),
                    ending_shred,
                ));
            }

            datapoint_error!(
                "blockstore_error",
                (
                    "error",
                    format!(
                        "Received shred from {shred_source:?} with index={shred_index} >= \
                         meta.last_index={meta_last_index}; shred: {:?}",
                        shred.id()
                    ),
                    String
                )
            );
            return false;
        }
        // Check that we do not receive a shred with "last_index" true, but shred_index
        // less than our current received
        if last_in_slot && shred_index < slot_meta.received {
            if !self.has_duplicate_shreds_in_slot(slot) {
                let shred_id = ShredId::new(
                    slot,
                    u32::try_from(slot_meta.received - 1).unwrap(),
                    ShredType::Data,
                );
                let Some(ending_shred) = self
                    .get_shred_from_just_inserted_or_db(just_inserted_shreds, shred_id, location)
                    .map(Cow::into_owned)
                else {
                    error!(
                        "Last received data shred {shred_id:?} indicated by slot meta \
                         {slot_meta:?} is missing from blockstore. This should only happen in \
                         extreme cases where blockstore cleanup has caught up to the root. \
                         Skipping data shred insertion"
                    );
                    return false;
                };

                if self
                    .store_duplicate_slot(slot, ending_shred.clone(), shred.payload().clone())
                    .is_err()
                {
                    warn!("store duplicate error");
                }
                duplicate_shreds.push(PossibleDuplicateShred::LastIndexConflict(
                    shred.clone(),
                    ending_shred,
                ));
            }

            datapoint_error!(
                "blockstore_error",
                (
                    "error",
                    format!(
                        "Received shred from {shred_source:?} with LAST_SHRED_IN_SLOT flag and \
                         index={shred_index} < meta.received={}; shred: {:?}",
                        slot_meta.received,
                        shred.id()
                    ),
                    String
                )
            );
            return false;
        }

        let Ok(shred_parent) = shred.parent() else {
            warn!(
                "Invalid data shred could not get parent slot shred_id {:?}",
                shred.id()
            );
            return false;
        };

        if !verify_shred_slots(slot, shred_parent, max_root) {
            return false;
        }

        let Some(expected_shred_parent) =
            self.expected_shred_parent(slot, location, slot_meta, just_inserted_shreds)
        else {
            error!(
                "Unable to determine expected shred parent for shred_id {:?}, slot_meta: {:?}",
                shred.id(),
                slot_meta
            );
            return false;
        };

        if expected_shred_parent != shred_parent {
            datapoint_error!(
                "blockstore_error",
                (
                    "error",
                    format!(
                        "Received shred from {shred_source:?} with parent={shred_parent} != \
                         expected_shred_parent={expected_shred_parent}{}; shred: {:?}",
                        if slot_meta.has_update_parent() {
                            " after UpdateParent"
                        } else {
                            " before UpdateParent"
                        },
                        shred.id(),
                    ),
                    String
                )
            );

            return false;
        }

        true
    }

    /// Returns the stable shred-header parent that every data shred in `slot`
    /// must use.
    ///
    /// `SlotMeta.parent_slot` is the effective replay parent and is rewritten by
    /// UpdateParent. The shred-header parent intentionally stays fixed for the
    /// whole slot, so after UpdateParent we recover it from the marker shred
    /// that caused `SlotMeta` to switch parents.
    fn expected_shred_parent(
        &self,
        slot: Slot,
        location: BlockLocation,
        slot_meta: &SlotMeta,
        just_inserted_shreds: &HashMap<(BlockLocation, ShredId), Cow<'_, Shred>>,
    ) -> Option<Slot> {
        if !slot_meta.has_update_parent() {
            return slot_meta.parent_slot;
        }

        // There has been an UpdateParent, so we can't use the `slot_meta` to
        // determine the stable parent used for shred filtering purposes (this
        // parent is the replay parent). Derive the stable parent from the shred
        // header of the UpdateParent shred (or cache).
        let replay_fec_set_index = slot_meta.replay_fec_set_index;
        let cache_key = (location, slot, replay_fec_set_index);
        if let Some(parent) = self
            .update_parent_shred_parent_cache
            .lock()
            .unwrap()
            .get(&cache_key)
            .copied()
        {
            return Some(parent);
        }

        let update_parent_shred_id = ShredId::new(slot, replay_fec_set_index, ShredType::Data);
        let update_parent_shred = self.get_shred_from_just_inserted_or_db(
            just_inserted_shreds,
            update_parent_shred_id,
            location,
        )?;
        let parent = Self::parent_from_data_shred_payload(slot, &update_parent_shred)?;
        self.update_parent_shred_parent_cache
            .lock()
            .unwrap()
            .put(cache_key, parent);
        Some(parent)
    }

    fn parent_from_data_shred_payload(slot: Slot, payload: &[u8]) -> Option<Slot> {
        let parent_offset = shred::layout::get_parent_offset(payload)?;
        if parent_offset == 0 && slot != 0 {
            return None;
        }
        slot.checked_sub(Slot::from(parent_offset))
    }

    fn insert_data_shred<'a>(
        &self,
        slot_meta: &mut SlotMeta,
        data_index: &'a mut ShredIndex,
        shred: &Shred,
        location: BlockLocation,
        write_batch: &mut WriteBatch,
        shred_source: ShredSource,
    ) -> impl Iterator<Item = CompletedDataSetInfo> + 'a + use<'a> {
        let slot = shred.slot();
        let index = u64::from(shred.index());

        let last_in_slot = if shred.last_in_slot() {
            debug!("got last in slot");
            true
        } else {
            false
        };

        let last_in_data = if shred.data_complete() {
            debug!("got last in data");
            true
        } else {
            false
        };

        // Parent for slot meta should have been set by this point
        assert!(!slot_meta.is_orphan());

        let new_consumed = if slot_meta.consumed == index {
            let mut current_index = index + 1;

            while data_index.contains(current_index) {
                current_index += 1;
            }
            current_index
        } else {
            slot_meta.consumed
        };

        // Commit step: commit all changes to the mutable structures at once, or none at all.
        // We don't want only a subset of these changes going through.
        self.put_data_shred_in_batch(write_batch, slot, index, location, shred.payload());
        data_index.insert(index);
        let newly_completed_data_sets = update_slot_meta(
            last_in_slot,
            last_in_data,
            slot_meta,
            index as u32,
            new_consumed,
            data_index,
        )
        .map(move |indices| CompletedDataSetInfo { slot, indices });

        self.slots_stats.record_shred(
            shred.slot(),
            location,
            shred.fec_set_index(),
            shred_source,
            Some(slot_meta),
        );

        trace!("inserted shred into slot {slot:?} and index {index:?}");

        newly_completed_data_sets
    }

    pub fn get_data_shred(&self, slot: Slot, index: u64) -> Result<Option<Vec<u8>>> {
        self.data_shred_cf.get_bytes((slot, index))
    }

    /// Retrieves the chained merkle root from the first data shred (index 0)
    /// of the given slot. Per SIMD-0340, this is expected to match the merkle
    /// root of the parent slot's last FEC set (the parent's block ID).
    pub fn get_parent_chained_block_id(&self, slot: Slot) -> Result<Hash> {
        let shred_bytes = self
            .get_data_shred(slot, 0)?
            .ok_or(BlockstoreError::MissingShred(slot, 0))?;
        shred::layout::get_chained_merkle_root(&shred_bytes)
            .ok_or(BlockstoreError::LegacyShred(slot, 0))
    }

    /// Retrieves the merkle root of the last data shred in the given slot,
    /// which serves as the slot's block ID for chained merkle root validation
    /// in child slots (SIMD-0340).
    ///
    /// Returns `Ok(None)`` if the block is not complete
    pub fn get_last_shred_merkle_root(&self, slot: Slot) -> Result<Option<Hash>> {
        let Some(meta) = self.meta(slot)? else {
            return Ok(None);
        };
        let Some(last_index) = meta.last_index else {
            return Ok(None);
        };
        let shred_bytes = self
            .get_data_shred(slot, last_index)?
            .ok_or(BlockstoreError::MissingShred(slot, last_index))?;
        shred::layout::get_merkle_root(&shred_bytes)
            .map(Option::Some)
            .ok_or(BlockstoreError::MissingMerkleRoot(slot, last_index))
    }

    /// Retrieves the block id of this slot depending on the `migration_status`:
    /// - For TowerBFT blocks this is the merkle root of the last shred
    /// - For Alpenglow blocks this is the double merkle root
    ///
    /// Returns `Ok(None)` if the block is not complete
    pub fn get_block_id(
        &self,
        slot: Slot,
        migration_status: &MigrationStatus,
    ) -> Result<Option<Hash>> {
        if migration_status.should_use_double_merkle_block_id(slot) {
            self.get_double_merkle_root(slot, BlockLocation::Original)
        } else {
            self.get_last_shred_merkle_root(slot)
        }
    }

    pub fn get_data_shreds_for_slot(&self, slot: Slot, start_index: u64) -> Result<Vec<Shred>> {
        self.get_data_shreds_for_slot_from_location(slot, start_index, BlockLocation::Original)
    }

    #[cfg(test)]
    fn get_data_shreds(
        &self,
        slot: Slot,
        from_index: u64,
        to_index: u64,
        buffer: &mut [u8],
    ) -> Result<(u64, usize)> {
        let _lock = self.check_lowest_cleanup_slot(slot)?;
        let mut buffer_offset = 0;
        let mut last_index = 0;
        if let Some(meta) = self.meta_cf.get(slot)? {
            if !meta.is_full() {
                warn!("The slot is not yet full. Will not return any shreds");
                return Ok((last_index, buffer_offset));
            }
            let to_index = cmp::min(to_index, meta.consumed);
            for index in from_index..to_index {
                if let Some(shred_data) = self.get_data_shred(slot, index)? {
                    let shred_len = shred_data.len();
                    if buffer.len().saturating_sub(buffer_offset) >= shred_len {
                        buffer[buffer_offset..buffer_offset + shred_len]
                            .copy_from_slice(&shred_data[..shred_len]);
                        buffer_offset += shred_len;
                        last_index = index;
                        // All shreds are of the same length.
                        // Let's check if we have scope to accommodate another shred
                        // If not, let's break right away, as it'll save on 1 DB read
                        if buffer.len().saturating_sub(buffer_offset) < shred_len {
                            break;
                        }
                    } else {
                        break;
                    }
                }
            }
        }
        Ok((last_index, buffer_offset))
    }

    pub fn get_coding_shred(&self, slot: Slot, index: u64) -> Result<Option<Vec<u8>>> {
        self.code_shred_cf.get_bytes((slot, index))
    }

    pub fn get_coding_shreds_for_slot(
        &self,
        slot: Slot,
        start_index: u64,
    ) -> std::result::Result<Vec<Shred>, shred::Error> {
        self.slot_coding_iterator(slot, start_index)
            .expect("blockstore couldn't fetch iterator")
            .map(|(_, bytes)| Shred::new_from_serialized_shred(Vec::from(bytes)))
            .collect()
    }

    fn get_data_shred_from_location(
        &self,
        slot: Slot,
        index: u64,
        location: BlockLocation,
    ) -> Result<Option<Vec<u8>>> {
        match location {
            BlockLocation::Original => self.get_data_shred(slot, index),
            BlockLocation::Alternate { block_id } => {
                self.alt_data_shred_cf.get_bytes((slot, block_id, index))
            }
        }
    }

    /// Returns a data shred for a complete block identified by `block_id`.
    pub fn get_data_shred_for_block_id(
        &self,
        slot: Slot,
        index: u64,
        block_id: Hash,
    ) -> Result<Option<Vec<u8>>> {
        let _lock = self.switch_block_lock.lock();
        let Some(location) = self.get_block_location(slot, block_id)? else {
            return Ok(None);
        };
        self.get_data_shred_from_location(slot, index, location)
    }

    /// Returns true if an alternate data shred for `block_id` has been stored.
    pub fn has_alternate_data_shred(&self, slot: Slot, index: u64, block_id: Hash) -> Result<bool> {
        Ok(self
            .alt_index_cf
            .get((slot, block_id))?
            .map(|index_meta| index_meta.data().contains(index))
            .unwrap_or(false))
    }

    /// Gets all data shreds for a slot from the specified location.
    /// Returns shreds in index order starting from `start_index`.
    fn get_data_shreds_for_slot_from_location(
        &self,
        slot: Slot,
        start_index: u64,
        location: BlockLocation,
    ) -> Result<Vec<Shred>> {
        // Get the index to determine capacity for pre-allocation
        let Some(index) = self.get_index_from_location(slot, location)? else {
            return Ok(Vec::new());
        };
        let num_shreds = index.data().count_range(start_index..);
        let mut shreds = Vec::with_capacity(num_shreds);

        let shred_bytes_iter: Box<dyn Iterator<Item = Box<[u8]>>> = match location {
            BlockLocation::Original => {
                let iter = self
                    .data_shred_cf
                    .iter(IteratorMode::From(
                        (slot, start_index),
                        IteratorDirection::Forward,
                    ))?
                    .take_while(move |((shred_slot, _), _)| *shred_slot == slot)
                    .map(|(_, bytes)| bytes);
                Box::new(iter)
            }
            BlockLocation::Alternate { block_id } => {
                let iter = self
                    .alt_data_shred_cf
                    .iter(IteratorMode::From(
                        (slot, block_id, start_index),
                        IteratorDirection::Forward,
                    ))?
                    .take_while(move |((shred_slot, shred_block_id, _), _)| {
                        *shred_slot == slot && *shred_block_id == block_id
                    })
                    .map(|(_, bytes)| bytes);
                Box::new(iter)
            }
        };

        for bytes in shred_bytes_iter {
            let shred = Shred::new_from_serialized_shred(Vec::from(bytes)).map_err(|err| {
                BlockstoreError::InvalidShredData(format!(
                    "Could not reconstruct shred from shred payload: {err}"
                ))
            })?;
            shreds.push(shred);
        }

        Ok(shreds)
    }

    /// Puts the shred of the specified slot-index in the column for the specified location.
    fn put_data_shred_in_batch(
        &self,
        write_batch: &mut WriteBatch,
        slot: Slot,
        index: u64,
        location: BlockLocation,
        shred: &[u8],
    ) {
        match location {
            BlockLocation::Original => {
                self.data_shred_cf
                    .put_bytes_in_batch(write_batch, (slot, index), shred);
            }
            BlockLocation::Alternate { block_id } => {
                self.alt_data_shred_cf.put_bytes_in_batch(
                    write_batch,
                    (slot, block_id, index),
                    shred,
                );
            }
        }
    }

    // Only used by tests
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn write_entries(
        &self,
        start_slot: Slot,
        num_ticks_in_start_slot: u64,
        start_index: u32,
        ticks_per_slot: u64,
        parent: Option<u64>,
        is_full_slot: bool,
        keypair: &Keypair,
        entries: Vec<Entry>,
        version: u16,
    ) -> Result<usize /*num of data shreds*/> {
        let mut parent_slot = parent.map_or(start_slot.saturating_sub(1), |v| v);
        let num_slots = (start_slot - parent_slot).max(1); // Note: slot 0 has parent slot 0
        assert!(num_ticks_in_start_slot < num_slots * ticks_per_slot);
        let mut remaining_ticks_in_slot = num_slots * ticks_per_slot - num_ticks_in_start_slot;

        let mut current_slot = start_slot;
        let mut shredder = Shredder::new(current_slot, parent_slot, 0, version).unwrap();
        let mut all_shreds = vec![];
        let mut slot_entries = vec![];
        let reed_solomon_cache = ReedSolomonCache::default();
        let mut chained_merkle_root = self
            .get_last_shred_merkle_root(parent_slot)
            .unwrap()
            .unwrap_or_else(|| Hash::new_from_array(rand::rng().random()));
        // Find all the entries for start_slot
        for entry in entries.into_iter() {
            if remaining_ticks_in_slot == 0 {
                current_slot += 1;
                parent_slot = current_slot - 1;
                remaining_ticks_in_slot = ticks_per_slot;
                let current_entries = std::mem::take(&mut slot_entries);
                let start_index = {
                    if all_shreds.is_empty() {
                        start_index
                    } else {
                        0
                    }
                };
                let (mut data_shreds, mut coding_shreds) = shredder
                    .entries_to_merkle_shreds_for_tests(
                        keypair,
                        &current_entries,
                        true, // is_last_in_slot
                        chained_merkle_root,
                        start_index, // next_shred_index
                        start_index, // next_code_index
                        &reed_solomon_cache,
                        &mut ProcessShredsStats::default(),
                    );
                let next_chained_merkle_root = coding_shreds
                    .last()
                    .and_then(|shred| shred.merkle_root().ok())
                    .unwrap_or(chained_merkle_root);
                all_shreds.append(&mut data_shreds);
                all_shreds.append(&mut coding_shreds);
                chained_merkle_root = next_chained_merkle_root;
                shredder = Shredder::new(
                    current_slot,
                    parent_slot,
                    (ticks_per_slot - remaining_ticks_in_slot) as u8,
                    version,
                )
                .unwrap();
            }

            if entry.is_tick() {
                remaining_ticks_in_slot -= 1;
            }
            slot_entries.push(entry);
        }

        if !slot_entries.is_empty() {
            all_shreds.extend(shredder.make_merkle_shreds_from_entries(
                keypair,
                &slot_entries,
                is_full_slot,
                chained_merkle_root,
                0, // next_shred_index
                0, // next_code_index
                &reed_solomon_cache,
                &mut ProcessShredsStats::default(),
            ));
        }
        let num_data = all_shreds.iter().filter(|shred| shred.is_data()).count();
        // `write_entries` is a test helper that synthesizes local shreds.
        // Insert as trusted to avoid dead-slot marking from untrusted-path
        // filters (for example slot-0 coding shred checks).
        self.insert_shreds(all_shreds, true)?;
        Ok(num_data)
    }

    pub fn get_index(&self, slot: Slot) -> Result<Option<Index>> {
        self.index_cf.get(slot)
    }

    /// Get a DB pinned reference to the [`Index`] via [`IndexRef`].
    fn get_index_ref(
        &self,
        slot: Slot,
    ) -> Result<Option<DBPinnedT<'_, DefaultConfig, IndexRef<'_>>>> {
        self.index_cf.get_pinned_t(slot)
    }

    fn get_index_from_location(
        &self,
        slot: Slot,
        location: BlockLocation,
    ) -> Result<Option<Index>> {
        match location {
            BlockLocation::Original => self.get_index(slot),
            BlockLocation::Alternate { block_id } => self.alt_index_cf.get((slot, block_id)),
        }
    }

    /// Puts the Index of the specified erasure set in the column for the specified location
    fn put_index_in_batch(
        &self,
        write_batch: &mut WriteBatch,
        slot: Slot,
        location: BlockLocation,
        index: &Index,
    ) -> Result<()> {
        match location {
            BlockLocation::Original => self.index_cf.put_in_batch(write_batch, slot, index),
            BlockLocation::Alternate { block_id } => {
                self.alt_index_cf
                    .put_in_batch(write_batch, (slot, block_id), index)
            }
        }
    }

    /// Manually update the meta for a slot.
    /// Can interfere with automatic meta update and potentially break chaining.
    /// Dangerous. Use with care.
    pub fn put_meta_bytes(&self, slot: Slot, bytes: &[u8]) -> Result<()> {
        self.meta_cf.put_bytes(slot, bytes)
    }

    /// Manually update the meta for a slot.
    /// Can interfere with automatic meta update and potentially break chaining.
    /// Dangerous. Use with care.
    pub fn put_meta(&self, slot: Slot, meta: &SlotMeta) -> Result<()> {
        self.put_meta_bytes(slot, &cf::SlotMeta::serialize(meta)?)
    }

    /// Find missing shred indices for a given `slot` within the range
    /// [`start_index`, `end_index`].
    ///
    /// Arguments:
    ///  - `slot`: The slot to search for missing shreds for.
    ///  - `start_index`: Begin search (inclusively) at this shred index.
    ///  - `end_index`: Finish search (exclusively) at this shred index.
    ///  - `max_missing`: Limit result to this many indices.
    pub fn find_missing_data_indexes(
        &self,
        slot: Slot,
        start_index: u64,
        end_index: u64,
        max_missing: usize,
    ) -> Vec<u64> {
        if start_index >= end_index || max_missing == 0 {
            return vec![];
        }

        let max_missing = max_missing.min((end_index - start_index) as usize);

        let index = match self.get_index_ref(slot) {
            Ok(Some(index)) => index,
            Ok(None) => return (start_index..end_index).take(max_missing).collect(),
            Err(err) => {
                warn!("failed to read index for slot {slot}: {err}");
                return vec![];
            }
        };
        let index = index.data();

        if index.num_shreds() == 0 {
            return (start_index..end_index).take(max_missing).collect();
        }

        let mut missing_indexes = Vec::with_capacity(max_missing);
        let mut prev_index = start_index;
        for current_index in index.range(start_index..end_index) {
            let num_to_take = max_missing - missing_indexes.len();
            missing_indexes.extend((prev_index..current_index).take(num_to_take));

            if missing_indexes.len() == max_missing {
                break;
            }

            prev_index = current_index + 1;
        }

        if missing_indexes.len() < max_missing {
            let num_to_take = max_missing - missing_indexes.len();
            missing_indexes.extend((prev_index..end_index).take(num_to_take));
        }

        missing_indexes
    }

    fn get_block_time(&self, slot: Slot) -> Result<Option<UnixTimestamp>> {
        let _lock = self.check_lowest_cleanup_slot(slot)?;
        self.blocktime_cf.get(slot)
    }

    pub fn get_rooted_block_time(&self, slot: Slot) -> Result<UnixTimestamp> {
        let _lock = self.check_lowest_cleanup_slot(slot)?;

        if self.is_root(slot) {
            return self
                .blocktime_cf
                .get(slot)?
                .ok_or(BlockstoreError::SlotUnavailable);
        }
        Err(BlockstoreError::SlotNotRooted)
    }

    pub fn set_block_time(&self, slot: Slot, timestamp: UnixTimestamp) -> Result<()> {
        self.blocktime_cf.put(slot, &timestamp)
    }

    pub fn get_block_height(&self, slot: Slot) -> Result<Option<u64>> {
        let _lock = self.check_lowest_cleanup_slot(slot)?;

        self.block_height_cf.get(slot)
    }

    pub fn set_block_height(&self, slot: Slot, block_height: u64) -> Result<()> {
        self.block_height_cf.put(slot, &block_height)
    }

    /// The first complete block that is available in the Blockstore ledger
    pub fn get_first_available_block(&self) -> Result<Slot> {
        let mut root_iterator = self.rooted_slot_iterator(self.lowest_slot_with_genesis())?;
        let first_root = root_iterator.next().unwrap_or_default();
        // If the first root is slot 0, it is genesis. Genesis is always complete, so it is correct
        // to return it as first-available.
        if first_root == 0 {
            return Ok(first_root);
        }
        // Otherwise, the block at root-index 0 cannot ever be complete, because it is missing its
        // parent blockhash. A parent blockhash must be calculated from the entries of the previous
        // block. Therefore, the first available complete block is that at root-index 1.
        Ok(root_iterator.next().unwrap_or_default())
    }

    pub fn get_rooted_block(
        &self,
        slot: Slot,
        require_previous_blockhash: bool,
    ) -> Result<VersionedConfirmedBlock> {
        let _lock = self.check_lowest_cleanup_slot(slot)?;

        if self.is_root(slot) {
            return self.get_complete_block(slot, require_previous_blockhash);
        }
        Err(BlockstoreError::SlotNotRooted)
    }

    pub fn get_complete_block(
        &self,
        slot: Slot,
        require_previous_blockhash: bool,
    ) -> Result<VersionedConfirmedBlock> {
        self.do_get_complete_block_with_entries(
            slot,
            require_previous_blockhash,
            false,
            /*allow_dead_slots:*/ false,
        )
        .map(|result| result.block)
    }

    pub fn get_rooted_block_with_entries(
        &self,
        slot: Slot,
        require_previous_blockhash: bool,
    ) -> Result<VersionedConfirmedBlockWithEntries> {
        let _lock = self.check_lowest_cleanup_slot(slot)?;

        if self.is_root(slot) {
            return self.do_get_complete_block_with_entries(
                slot,
                require_previous_blockhash,
                true,
                /*allow_dead_slots:*/ false,
            );
        }
        Err(BlockstoreError::SlotNotRooted)
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn get_complete_block_with_entries(
        &self,
        slot: Slot,
        require_previous_blockhash: bool,
        populate_entries: bool,
        allow_dead_slots: bool,
    ) -> Result<VersionedConfirmedBlockWithEntries> {
        self.do_get_complete_block_with_entries(
            slot,
            require_previous_blockhash,
            populate_entries,
            allow_dead_slots,
        )
    }

    fn do_get_complete_block_with_entries(
        &self,
        slot: Slot,
        require_previous_blockhash: bool,
        populate_entries: bool,
        allow_dead_slots: bool,
    ) -> Result<VersionedConfirmedBlockWithEntries> {
        let Some(slot_meta) = self.meta_cf.get(slot)? else {
            trace!("do_get_complete_block_with_entries() failed for {slot} (missing SlotMeta)");
            return Err(BlockstoreError::SlotUnavailable);
        };

        if !slot_meta.is_full() {
            trace!("do_get_complete_block_with_entries() failed for {slot} (slot not full)");
            return Err(BlockstoreError::SlotUnavailable);
        }

        let (slot_entries, _, _) = self.get_slot_entries_with_shred_info(
            slot,
            /*shred_start_index:*/ 0,
            allow_dead_slots,
        )?;

        if slot_entries.is_empty() {
            trace!("do_get_complete_block_with_entries() failed for {slot} (no entries found)");
            return Err(BlockstoreError::SlotUnavailable);
        }

        let blockhash = slot_entries
            .last()
            .map(|entry| entry.hash)
            .unwrap_or_else(|| panic!("Rooted slot {slot:?} must have blockhash"));

        let mut starting_transaction_index = 0;
        let mut entries = if populate_entries {
            Vec::with_capacity(slot_entries.len())
        } else {
            Vec::new()
        };

        let slot_transaction_iterator = slot_entries
            .into_iter()
            .flat_map(|entry| {
                if populate_entries {
                    entries.push(solana_transaction_status::EntrySummary {
                        num_hashes: entry.num_hashes,
                        hash: entry.hash,
                        num_transactions: entry.transactions.len() as u64,
                        starting_transaction_index,
                    });
                    starting_transaction_index += entry.transactions.len();
                }
                entry.transactions
            })
            .map(|transaction| {
                if let Err(err) = transaction.sanitize() {
                    warn!(
                        "Blockstore::get_block sanitize failed: {err:?}, slot: {slot:?}, \
                         {transaction:?}",
                    );
                }
                transaction
            });

        let previous_blockhash = slot_meta.parent_slot.and_then(|parent_slot| {
            self.get_slot_entries_with_shred_info(
                parent_slot,
                /*shred_start_index:*/ 0,
                allow_dead_slots,
            )
            .ok()
            .and_then(|(entries, _, is_full)| {
                // The blockhash is specifically the final entry hash in a
                // block so ensure the block is full
                if is_full {
                    entries.last().map(|entry| entry.hash)
                } else {
                    None
                }
            })
        });
        if previous_blockhash.is_none() && require_previous_blockhash {
            return Err(BlockstoreError::ParentEntriesUnavailable);
        }
        let previous_blockhash = previous_blockhash.unwrap_or_else(Hash::default);

        let RewardsAndNumPartitions {
            rewards,
            num_partitions,
        } = self
            .read_rewards(slot)?
            .unwrap_or_else(|| RewardsAndNumPartitions {
                rewards: Vec::new(),
                num_partitions: None,
            });

        // The Blocktime and BlockHeight column families are updated asynchronously; they
        // may not be written by the time the complete slot entries are available. In this
        // case, these fields will be `None`.
        let block_time = self.blocktime_cf.get(slot)?;
        let block_height = self.block_height_cf.get(slot)?;

        let block = VersionedConfirmedBlock {
            previous_blockhash: previous_blockhash.to_string(),
            blockhash: blockhash.to_string(),
            // If the slot is full it should have parent_slot populated
            // from shreds received.
            parent_slot: slot_meta.parent_slot.unwrap(),
            transactions: self.map_transactions_to_statuses(slot, slot_transaction_iterator)?,
            rewards,
            num_partitions,
            block_time,
            block_height,
        };

        Ok(VersionedConfirmedBlockWithEntries { block, entries })
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn get_complete_block_with_components(
        &self,
        slot: Slot,
        require_previous_blockhash: bool,
        populate_components: bool,
        allow_dead_slots: bool,
    ) -> Result<VersionedConfirmedBlockWithComponents> {
        self.do_get_complete_block_with_components(
            slot,
            require_previous_blockhash,
            populate_components,
            allow_dead_slots,
        )
    }

    #[cfg(feature = "dev-context-only-utils")]
    fn do_get_complete_block_with_components(
        &self,
        slot: Slot,
        require_previous_blockhash: bool,
        populate_components: bool,
        allow_dead_slots: bool,
    ) -> Result<VersionedConfirmedBlockWithComponents> {
        let Some(slot_meta) = self.meta_cf.get(slot)? else {
            trace!("do_get_complete_block_with_components() failed for {slot} (missing SlotMeta)");
            return Err(BlockstoreError::SlotUnavailable);
        };

        if !slot_meta.is_full() {
            trace!("do_get_complete_block_with_components() failed for {slot} (slot not full)");
            return Err(BlockstoreError::SlotUnavailable);
        }

        let (slot_components, _, _) = self.get_slot_components_with_shred_info(
            slot,
            /*start_index:*/ 0,
            allow_dead_slots,
        )?;

        if slot_components.is_empty() {
            trace!(
                "do_get_complete_block_with_components() failed for {slot} (no components found)"
            );
            return Err(BlockstoreError::SlotUnavailable);
        }

        let blockhash = slot_components
            .iter()
            .rev()
            .find_map(|component| match component {
                BlockComponent::EntryBatch(entries) => entries.last().map(|entry| entry.hash),
                BlockComponent::BlockMarker(_) => None,
            })
            .unwrap_or_else(|| panic!("Rooted slot {slot:?} must have blockhash"));

        let mut starting_transaction_index = 0;
        let mut components = if populate_components {
            Vec::with_capacity(slot_components.len())
        } else {
            Vec::new()
        };

        let slot_transaction_iterator = slot_components
            .into_iter()
            .filter_map(|component| match component {
                BlockComponent::EntryBatch(entries) => {
                    if populate_components {
                        let entry_summaries = entries
                            .iter()
                            .map(|entry| {
                                let entry_summary = EntrySummary {
                                    num_hashes: entry.num_hashes,
                                    hash: entry.hash,
                                    num_transactions: entry.transactions.len() as u64,
                                    starting_transaction_index,
                                };
                                starting_transaction_index += entry.transactions.len();
                                entry_summary
                            })
                            .collect();
                        components.push(ConfirmedBlockComponent::EntryBatch(entry_summaries));
                    }
                    Some(entries)
                }
                BlockComponent::BlockMarker(marker) => {
                    if populate_components {
                        components.push(ConfirmedBlockComponent::BlockMarker(marker));
                    }
                    None
                }
            })
            .flatten()
            .flat_map(|entry| entry.transactions)
            .map(|transaction| {
                if let Err(err) = transaction.sanitize() {
                    warn!(
                        "Blockstore::get_complete_block_with_components sanitize failed: {err:?}, \
                         slot: {slot:?}, {transaction:?}",
                    );
                }
                transaction
            });

        let block = self.build_versioned_confirmed_block(
            slot,
            require_previous_blockhash,
            allow_dead_slots,
            &slot_meta,
            &blockhash,
            slot_transaction_iterator,
        )?;

        Ok(VersionedConfirmedBlockWithComponents { block, components })
    }

    // Helper to build VersionConfirmedBlock from blockhash and transactions
    #[cfg(feature = "dev-context-only-utils")]
    fn build_versioned_confirmed_block(
        &self,
        slot: Slot,
        require_previous_blockhash: bool,
        allow_dead_slots: bool,
        slot_meta: &SlotMeta,
        blockhash: &Hash,
        slot_transaction_iterator: impl Iterator<Item = VersionedTransaction>,
    ) -> Result<VersionedConfirmedBlock> {
        let previous_blockhash = slot_meta.parent_slot.and_then(|parent_slot| {
            self.get_slot_components_with_shred_info(
                parent_slot,
                /*shred_start_index:*/ 0,
                allow_dead_slots,
            )
            .ok()
            .and_then(|(components, _, is_full)| {
                // The blockhash is specifically the final entry hash in a
                // block so ensure the block is full
                if is_full {
                    components
                        .iter()
                        .rev()
                        .find_map(|component| match component {
                            BlockComponent::EntryBatch(entries) => {
                                entries.last().map(|entry| entry.hash)
                            }
                            BlockComponent::BlockMarker(_) => None,
                        })
                } else {
                    None
                }
            })
        });
        if previous_blockhash.is_none() && require_previous_blockhash {
            return Err(BlockstoreError::ParentEntriesUnavailable);
        }
        let previous_blockhash = previous_blockhash.unwrap_or_default();

        let RewardsAndNumPartitions {
            rewards,
            num_partitions,
        } = self
            .read_rewards(slot)?
            .unwrap_or_else(|| RewardsAndNumPartitions {
                rewards: Vec::new(),
                num_partitions: None,
            });

        // The Blocktime and BlockHeight column families are updated asynchronously; they
        // may not be written by the time the complete slot entries are available. In this
        // case, these fields will be `None`.
        let block_time = self.blocktime_cf.get(slot)?;
        let block_height = self.block_height_cf.get(slot)?;

        Ok(VersionedConfirmedBlock {
            previous_blockhash: previous_blockhash.to_string(),
            blockhash: blockhash.to_string(),
            // If the slot is full it should have parent_slot populated
            // from shreds received.
            parent_slot: slot_meta.parent_slot.unwrap(),
            transactions: self.map_transactions_to_statuses(slot, slot_transaction_iterator)?,
            rewards,
            num_partitions,
            block_time,
            block_height,
        })
    }

    pub fn map_transactions_to_statuses(
        &self,
        slot: Slot,
        iterator: impl Iterator<Item = VersionedTransaction>,
    ) -> Result<Vec<VersionedTransactionWithStatusMeta>> {
        iterator
            .map(|transaction| {
                let signature = transaction.signatures[0];
                Ok(VersionedTransactionWithStatusMeta {
                    transaction,
                    meta: self
                        .read_transaction_status((signature, slot))?
                        .ok_or(BlockstoreError::MissingTransactionMetadata)?,
                })
            })
            .collect()
    }

    pub fn read_transaction_status(
        &self,
        index: (Signature, Slot),
    ) -> Result<Option<TransactionStatusMeta>> {
        Ok(self
            .transaction_status_cf
            .get_protobuf(index)?
            .and_then(|meta| meta.try_into().ok()))
    }

    #[inline]
    fn write_transaction_status_helper<'a, F>(
        &self,
        slot: Slot,
        signature: Signature,
        keys_with_writable: impl Iterator<Item = (&'a Pubkey, bool)>,
        status: TransactionStatusMeta,
        transaction_index: usize,
        mut write_fn: F,
    ) -> Result<()>
    where
        F: FnMut(&Pubkey, Slot, u32, Signature, bool) -> Result<()>,
    {
        let status = status.into();
        let transaction_index = u32::try_from(transaction_index)
            .map_err(|_| BlockstoreError::TransactionIndexOverflow)?;
        self.transaction_status_cf
            .put_protobuf((signature, slot), &status)?;

        for (address, writeable) in keys_with_writable {
            write_fn(address, slot, transaction_index, signature, writeable)?;
        }

        Ok(())
    }

    pub fn write_transaction_status<'a>(
        &self,
        slot: Slot,
        signature: Signature,
        keys_with_writable: impl Iterator<Item = (&'a Pubkey, bool)>,
        status: TransactionStatusMeta,
        transaction_index: usize,
    ) -> Result<()> {
        self.write_transaction_status_helper(
            slot,
            signature,
            keys_with_writable,
            status,
            transaction_index,
            |address, slot, tx_index, signature, writeable| {
                self.address_signatures_cf.put(
                    (*address, slot, tx_index, signature),
                    &AddressSignatureMeta { writeable },
                )
            },
        )
    }

    pub fn add_transaction_status_to_batch<'a>(
        &self,
        slot: Slot,
        signature: Signature,
        keys_with_writable: impl Iterator<Item = (&'a Pubkey, bool)>,
        status: TransactionStatusMeta,
        transaction_index: usize,
        db_write_batch: &mut WriteBatch,
    ) -> Result<()> {
        self.write_transaction_status_helper(
            slot,
            signature,
            keys_with_writable,
            status,
            transaction_index,
            |address, slot, tx_index, signature, writeable| {
                self.address_signatures_cf.put_in_batch(
                    db_write_batch,
                    (*address, slot, tx_index, signature),
                    &AddressSignatureMeta { writeable },
                )
            },
        )
    }

    pub fn read_transaction_memos(
        &self,
        signature: Signature,
        slot: Slot,
    ) -> Result<Option<String>> {
        self.transaction_memos_cf.get((signature, slot))
    }

    pub fn write_transaction_memos(
        &self,
        signature: &Signature,
        slot: Slot,
        memos: String,
    ) -> Result<()> {
        self.transaction_memos_cf.put((*signature, slot), &memos)
    }

    pub fn add_transaction_memos_to_batch(
        &self,
        signature: &Signature,
        slot: Slot,
        memos: String,
        db_write_batch: &mut WriteBatch,
    ) -> Result<()> {
        self.transaction_memos_cf
            .put_in_batch(db_write_batch, (*signature, slot), &memos)
    }

    /// Acquires the `lowest_cleanup_slot` lock and returns a tuple of the held lock
    /// and lowest available slot.
    ///
    /// The function will return BlockstoreError::SlotCleanedUp if the input
    /// `slot` has already been cleaned-up.
    fn check_lowest_cleanup_slot(
        &self,
        slot: Slot,
    ) -> Result<std::sync::RwLockReadGuard<'_, Slot>> {
        // lowest_cleanup_slot is the last slot that was not cleaned up by LedgerCleanupService
        let lowest_cleanup_slot = self.lowest_cleanup_slot.read().unwrap();
        if *lowest_cleanup_slot > 0 && *lowest_cleanup_slot >= slot {
            return Err(BlockstoreError::SlotCleanedUp);
        }
        // Make caller hold this lock properly; otherwise LedgerCleanupService can purge/compact
        // needed slots here at any given moment
        Ok(lowest_cleanup_slot)
    }

    /// Acquires the lock of `lowest_cleanup_slot` and returns the tuple of
    /// the held lock and the lowest available slot.
    ///
    /// This function ensures a consistent result by using lowest_cleanup_slot
    /// as the lower bound for reading columns that do not employ strong read
    /// consistency with slot-based delete_range.
    fn ensure_lowest_cleanup_slot(&self) -> (std::sync::RwLockReadGuard<'_, Slot>, Slot) {
        let lowest_cleanup_slot = self.lowest_cleanup_slot.read().unwrap();
        let lowest_available_slot = (*lowest_cleanup_slot)
            .checked_add(1)
            .expect("overflow from trusted value");

        // Make caller hold this lock properly; otherwise LedgerCleanupService can purge/compact
        // needed slots here at any given moment.
        // Blockstore callers, like rpc, can process concurrent read queries
        (lowest_cleanup_slot, lowest_available_slot)
    }

    // Returns a transaction status, as well as a loop counter for unit testing
    fn get_transaction_status_with_counter(
        &self,
        signature: Signature,
        confirmed_unrooted_slots: &HashSet<Slot>,
    ) -> Result<(Option<(Slot, TransactionStatusMeta)>, u64)> {
        let mut counter = 0;
        let (lock, _) = self.ensure_lowest_cleanup_slot();
        let first_available_block = self.get_first_available_block()?;

        let iterator = self.transaction_status_cf.iter(IteratorMode::From(
            (signature, first_available_block),
            IteratorDirection::Forward,
        ))?;

        for ((sig, slot), _data) in iterator {
            counter += 1;
            if sig != signature {
                break;
            }
            if !self.is_root(slot) && !confirmed_unrooted_slots.contains(&slot) {
                continue;
            }
            let status = self
                .transaction_status_cf
                .get_protobuf((signature, slot))?
                .and_then(|status| status.try_into().ok())
                .map(|status| (slot, status));
            return Ok((status, counter));
        }

        drop(lock);
        Ok((None, counter))
    }

    /// Returns a transaction status
    pub fn get_rooted_transaction_status(
        &self,
        signature: Signature,
    ) -> Result<Option<(Slot, TransactionStatusMeta)>> {
        self.get_transaction_status(signature, &HashSet::default())
    }

    /// Returns a transaction status
    pub fn get_transaction_status(
        &self,
        signature: Signature,
        confirmed_unrooted_slots: &HashSet<Slot>,
    ) -> Result<Option<(Slot, TransactionStatusMeta)>> {
        self.get_transaction_status_with_counter(signature, confirmed_unrooted_slots)
            .map(|(status, _)| status)
    }

    /// Returns a complete transaction if it was processed in a root
    pub fn get_rooted_transaction(
        &self,
        signature: Signature,
    ) -> Result<Option<ConfirmedTransactionWithStatusMeta>> {
        self.get_transaction_with_status(signature, &HashSet::default())
    }

    /// Returns a complete transaction
    pub fn get_complete_transaction(
        &self,
        signature: Signature,
        highest_confirmed_slot: Slot,
    ) -> Result<Option<ConfirmedTransactionWithStatusMeta>> {
        let max_root = self.max_root();
        let confirmed_unrooted_slots: HashSet<_> =
            AncestorIterator::new_inclusive(highest_confirmed_slot, self)
                .take_while(|&slot| slot > max_root)
                .collect();
        self.get_transaction_with_status(signature, &confirmed_unrooted_slots)
    }

    fn get_transaction_with_status(
        &self,
        signature: Signature,
        confirmed_unrooted_slots: &HashSet<Slot>,
    ) -> Result<Option<ConfirmedTransactionWithStatusMeta>> {
        if let Some((slot, meta)) =
            self.get_transaction_status(signature, confirmed_unrooted_slots)?
        {
            let (transaction, index) = self
                .find_transaction_in_slot(slot, signature)?
                .ok_or(BlockstoreError::TransactionStatusSlotMismatch)?; // Should not happen

            let block_time = self.get_block_time(slot)?;
            Ok(Some(ConfirmedTransactionWithStatusMeta {
                slot,
                tx_with_meta: TransactionWithStatusMeta::Complete(
                    VersionedTransactionWithStatusMeta { transaction, meta },
                ),
                block_time,
                index,
            }))
        } else {
            Ok(None)
        }
    }

    /// Finds a transaction by signature in the given slot and returns it along with its index.
    ///
    /// The index represents the transaction's 0-based position in the flattened list of all
    /// transactions across all entries in this slot. This matches the `transaction_index`
    /// stored in `AddressSignatures` when `write_transaction_status` is called during block
    /// processing.
    fn find_transaction_in_slot(
        &self,
        slot: Slot,
        signature: Signature,
    ) -> Result<Option<(VersionedTransaction, u32)>> {
        let slot_entries = self.get_slot_entries(slot, 0)?;
        Ok(slot_entries
            .into_iter()
            .flat_map(|entry| entry.transactions)
            .enumerate()
            .map(|(index, transaction)| {
                if let Err(err) = transaction.sanitize() {
                    warn!(
                        "Blockstore::find_transaction_in_slot sanitize failed: {err:?}, slot: \
                         {slot:?}, {transaction:?}",
                    );
                }
                (index, transaction)
            })
            .find(|(_, transaction)| transaction.signatures[0] == signature)
            .map(|(index, transaction)| (transaction, index as u32)))
    }

    // Returns all signatures for an address in a particular slot, regardless of whether that slot
    // has been rooted. The transactions will be ordered by their occurrence in the block
    fn find_address_signatures_for_slot(
        &self,
        pubkey: Pubkey,
        slot: Slot,
    ) -> Result<Vec<(Slot, Signature, u32)>> {
        let (lock, lowest_available_slot) = self.ensure_lowest_cleanup_slot();
        let mut signatures: Vec<(Slot, Signature, u32)> = vec![];
        if slot < lowest_available_slot {
            return Ok(signatures);
        }
        let index_iterator = self.address_signatures_cf.iter(IteratorMode::From(
            (
                pubkey,
                slot.max(lowest_available_slot),
                0,
                Signature::default(),
            ),
            IteratorDirection::Forward,
        ))?;
        for ((address, transaction_slot, transaction_index, signature), _) in index_iterator {
            if transaction_slot > slot || address != pubkey {
                break;
            }
            signatures.push((transaction_slot, signature, transaction_index));
        }
        drop(lock);
        Ok(signatures)
    }

    fn get_block_signatures_rev(&self, slot: Slot) -> Result<Vec<Signature>> {
        let block = self.get_complete_block(slot, false).map_err(|err| {
            BlockstoreError::Io(IoError::other(format!("Unable to get block: {err}")))
        })?;

        Ok(block
            .transactions
            .into_iter()
            .rev()
            .filter_map(|transaction_with_meta| {
                transaction_with_meta
                    .transaction
                    .signatures
                    .into_iter()
                    .next()
            })
            .collect())
    }

    pub fn get_confirmed_signatures_for_address2(
        &self,
        address: Pubkey,
        highest_slot: Slot, // highest_super_majority_root or highest_confirmed_slot
        before: Option<Signature>,
        until: Option<Signature>,
        limit: usize,
    ) -> Result<SignatureInfosForAddress> {
        let max_root = self.max_root();
        let confirmed_unrooted_slots: HashSet<_> =
            AncestorIterator::new_inclusive(highest_slot, self)
                .take_while(|&slot| slot > max_root)
                .collect();

        // Figure the `slot` to start listing signatures at, based on the ledger location of the
        // `before` signature if present.  Also generate a HashSet of signatures that should
        // be excluded from the results.
        let mut get_before_slot_timer = Measure::start("get_before_slot_timer");
        let (slot, mut before_excluded_signatures) = match before {
            None => (highest_slot, None),
            Some(before) => {
                let transaction_status =
                    self.get_transaction_status(before, &confirmed_unrooted_slots)?;
                match transaction_status {
                    None => return Ok(SignatureInfosForAddress::default()),
                    Some((slot, _)) => {
                        let mut slot_signatures = self.get_block_signatures_rev(slot)?;
                        if let Some(pos) = slot_signatures.iter().position(|&x| x == before) {
                            slot_signatures.truncate(pos + 1);
                        }

                        (
                            slot,
                            Some(slot_signatures.into_iter().collect::<HashSet<_>>()),
                        )
                    }
                }
            }
        };
        get_before_slot_timer.stop();

        let first_available_block = self.get_first_available_block()?;
        // Generate a HashSet of signatures that should be excluded from the results based on
        // `until` signature
        let mut get_until_slot_timer = Measure::start("get_until_slot_timer");
        let (lowest_slot, until_excluded_signatures, found_until) = match until {
            None => (first_available_block, HashSet::new(), false),
            Some(until) => {
                let transaction_status =
                    self.get_transaction_status(until, &confirmed_unrooted_slots)?;
                match transaction_status {
                    None => (first_available_block, HashSet::new(), false),
                    Some((slot, _)) => {
                        let mut slot_signatures = self.get_block_signatures_rev(slot)?;
                        if let Some(pos) = slot_signatures.iter().position(|&x| x == until) {
                            slot_signatures = slot_signatures.split_off(pos);
                        }

                        (
                            slot,
                            slot_signatures.into_iter().collect::<HashSet<_>>(),
                            true,
                        )
                    }
                }
            }
        };
        get_until_slot_timer.stop();

        // Fetch the list of signatures that affect the given address
        let mut address_signatures: Vec<(Slot, Signature, u32)> = vec![];

        // Get signatures in `slot`
        let mut get_initial_slot_timer = Measure::start("get_initial_slot_timer");
        let mut signatures = self.find_address_signatures_for_slot(address, slot)?;
        signatures.reverse();
        if let Some(excluded_signatures) = before_excluded_signatures.take() {
            address_signatures.extend(
                signatures
                    .into_iter()
                    .filter(|(_, signature, _)| !excluded_signatures.contains(signature)),
            )
        } else {
            address_signatures.append(&mut signatures);
        }
        get_initial_slot_timer.stop();

        let mut address_signatures_iter_timer = Measure::start("iter_timer");
        let mut iterator = self.address_signatures_cf.iter(IteratorMode::From(
            // Regardless of whether a `before` signature is provided, the latest relevant
            // `slot` is queried directly with the `find_address_signatures_for_slot()`
            // call above. Thus, this iterator starts at the lowest entry of `address,
            // slot` and iterates backwards to continue reporting the next earliest
            // signatures.
            (address, slot, 0, Signature::default()),
            IteratorDirection::Reverse,
        ))?;

        // Iterate until limit is reached
        while address_signatures.len() < limit {
            if let Some(((key_address, slot, transaction_index, signature), _)) = iterator.next() {
                if slot < lowest_slot {
                    break;
                }
                if key_address == address {
                    if self.is_root(slot) || confirmed_unrooted_slots.contains(&slot) {
                        address_signatures.push((slot, signature, transaction_index));
                    }
                    continue;
                }
            }
            break;
        }
        address_signatures_iter_timer.stop();

        let address_signatures_iter = address_signatures
            .into_iter()
            .filter(|(_, signature, _)| !until_excluded_signatures.contains(signature))
            .take(limit);

        // Fill in the status information for each found transaction
        let mut get_status_info_timer = Measure::start("get_status_info_timer");
        let mut infos = vec![];
        for (slot, signature, index) in address_signatures_iter {
            let transaction_status =
                self.get_transaction_status(signature, &confirmed_unrooted_slots)?;
            let err = transaction_status.and_then(|(_slot, status)| status.status.err());
            let memo = self.read_transaction_memos(signature, slot)?;
            let block_time = self.get_block_time(slot)?;
            infos.push(ConfirmedTransactionStatusWithSignature {
                signature,
                slot,
                err,
                memo,
                block_time,
                index,
            });
        }
        get_status_info_timer.stop();

        datapoint_info!(
            "blockstore-get-conf-sigs-for-addr-2",
            (
                "get_before_slot_us",
                get_before_slot_timer.as_us() as i64,
                i64
            ),
            (
                "get_initial_slot_us",
                get_initial_slot_timer.as_us() as i64,
                i64
            ),
            (
                "address_signatures_iter_us",
                address_signatures_iter_timer.as_us() as i64,
                i64
            ),
            (
                "get_status_info_us",
                get_status_info_timer.as_us() as i64,
                i64
            ),
            (
                "get_until_slot_us",
                get_until_slot_timer.as_us() as i64,
                i64
            )
        );

        Ok(SignatureInfosForAddress {
            infos,
            found_before: true, // if `before` signature was not found, this method returned early
            found_until,
        })
    }

    fn read_rewards(&self, slot: Slot) -> Result<Option<RewardsAndNumPartitions>> {
        self.rewards_cf
            .get_protobuf(slot)
            .map(|result| result.map(|option| option.into()))
    }

    pub fn write_rewards(&self, index: Slot, rewards: RewardsAndNumPartitions) -> Result<()> {
        let rewards = rewards.into();
        self.rewards_cf.put_protobuf(index, &rewards)
    }

    pub fn get_recent_perf_samples(&self, num: usize) -> Result<Vec<(Slot, PerfSample)>> {
        // When reading `PerfSamples`, the database may contain samples with either `PerfSampleV1`
        // or `PerfSampleV2` encoding.  We expect `PerfSampleV1` to be a prefix of the
        // `PerfSampleV2` encoding (see [`perf_sample_v1_is_prefix_of_perf_sample_v2`]), so we try
        // them in order.
        let samples = self
            .perf_samples_cf
            .iter(IteratorMode::End)?
            .take(num)
            .map(|(slot, data)| cf::PerfSamples::deserialize(&data).map(|sample| (slot, sample)));

        samples.collect()
    }

    pub fn write_perf_sample(&self, index: Slot, perf_sample: &PerfSample) -> Result<()> {
        // Always write as the current version.
        let bytes =
            cf::PerfSamples::serialize(perf_sample).expect("`PerfSample` can be serialized");
        self.perf_samples_cf.put_bytes(index, &bytes)
    }

    /// Returns the entry vector for the slot starting with `shred_start_index`
    pub fn get_slot_entries(&self, slot: Slot, shred_start_index: u64) -> Result<Vec<Entry>> {
        self.get_slot_entries_with_shred_info(slot, shred_start_index, false)
            .map(|x| x.0)
    }

    /// Helper function that contains the common logic for getting slot data with shred info
    fn get_slot_data_with_shred_info_common(
        &self,
        slot: Slot,
        start_index: u64,
        allow_dead_slots: bool,
    ) -> Result<Option<(CompletedRanges, SlotMeta, u64)>> {
        let (completed_ranges, slot_meta) = self.get_completed_ranges(slot, start_index)?;

        // Check if the slot is dead *after* fetching completed ranges to avoid a race
        // where a slot is marked dead by another thread before the completed range query finishes.
        // This should be sufficient because full slots will never be marked dead from another thread,
        // this can only happen during entry processing during replay stage.
        if self.is_dead(slot) && !allow_dead_slots {
            return Err(BlockstoreError::DeadSlot);
        } else if completed_ranges.is_empty() {
            return Ok(None);
        }

        let slot_meta = slot_meta.unwrap();
        let num_shreds = completed_ranges
            .last()
            .map(|&Range { end, .. }| u64::from(end) - start_index)
            .unwrap_or(0);

        Ok(Some((completed_ranges, slot_meta, num_shreds)))
    }

    /// Returns the entry vector for the slot starting with `shred_start_index`, the number of
    /// shreds that comprise the entry vector, and whether the slot is full (consumed all shreds).
    pub fn get_slot_entries_with_shred_info(
        &self,
        slot: Slot,
        start_index: u64,
        allow_dead_slots: bool,
    ) -> Result<(Vec<Entry>, u64, bool)> {
        let Some((completed_ranges, slot_meta, num_shreds)) =
            self.get_slot_data_with_shred_info_common(slot, start_index, allow_dead_slots)?
        else {
            return Ok((vec![], 0, false));
        };

        let entries = self.get_slot_entries_in_block(slot, &completed_ranges, Some(&slot_meta))?;
        Ok((entries, num_shreds, slot_meta.is_full()))
    }

    /// Returns the components vector for the slot starting with `shred_start_index`, the number of
    /// shreds that comprise the components vector, and whether the slot is full (consumed all
    /// shreds).
    pub fn get_slot_components_with_shred_info(
        &self,
        slot: Slot,
        start_index: u64,
        allow_dead_slots: bool,
    ) -> Result<(Vec<BlockComponent>, Vec<Range<u32>>, bool)> {
        let Some((completed_ranges, slot_meta, _)) =
            self.get_slot_data_with_shred_info_common(slot, start_index, allow_dead_slots)?
        else {
            return Ok((vec![], vec![], false));
        };

        let components =
            self.get_slot_components_in_block(slot, &completed_ranges, Some(&slot_meta))?;
        debug_assert_eq!(completed_ranges.len(), components.len());
        Ok((components, completed_ranges, slot_meta.is_full()))
    }

    /// Gets accounts used in transactions in the slot range [starting_slot, ending_slot].
    /// Additionally returns a bool indicating if the set may be incomplete.
    /// Used by ledger-tool to create a minimized snapshot
    pub fn get_accounts_used_in_range(
        &self,
        bank: &Bank,
        starting_slot: Slot,
        ending_slot: Slot,
    ) -> (DashSet<Pubkey>, bool) {
        let result = DashSet::new();
        let lookup_tables = DashSet::new();
        let possible_cpi_alt_extend = AtomicBool::new(false);

        fn add_to_set<'a>(set: &DashSet<Pubkey>, iter: impl IntoIterator<Item = &'a Pubkey>) {
            iter.into_iter().for_each(|key| {
                set.insert(*key);
            });
        }

        (starting_slot..=ending_slot)
            .into_par_iter()
            .for_each(|slot| {
                if let Ok(entries) = self.get_slot_entries(slot, 0) {
                    entries.into_par_iter().for_each(|entry| {
                        entry.transactions.into_iter().for_each(|tx| {
                            if let Some(lookups) = tx.message.address_table_lookups() {
                                add_to_set(
                                    &lookup_tables,
                                    lookups.iter().map(|lookup| &lookup.account_key),
                                );
                            }
                            // Attempt to verify transaction and load addresses from the current bank,
                            // or manually scan the transaction for addresses if the transaction.
                            if let Ok(tx) = bank.verify_transaction(
                                tx.clone(),
                                TransactionVerificationMode::FullVerification,
                            ) {
                                add_to_set(&result, tx.message().account_keys().iter());
                            } else {
                                add_to_set(&result, tx.message.static_account_keys());

                                let tx = SanitizedVersionedTransaction::try_from(tx)
                                    .expect("transaction failed to sanitize");

                                let alt_scan_extensions = scan_transaction(&tx);
                                add_to_set(&result, &alt_scan_extensions.accounts);
                                if alt_scan_extensions.possibly_incomplete {
                                    possible_cpi_alt_extend.store(true, Ordering::Relaxed);
                                }
                            }
                        });
                    });
                }
            });

        // For each unique lookup table add all accounts to the minimized set.
        lookup_tables.into_par_iter().for_each(|lookup_table_key| {
            bank.get_account(&lookup_table_key)
                .map(|lookup_table_account| {
                    add_to_set(&result, &[lookup_table_key]);
                    AddressLookupTable::deserialize(lookup_table_account.data()).map(|t| {
                        add_to_set(&result, &t.addresses[..]);
                    })
                });
        });

        (result, possible_cpi_alt_extend.into_inner())
    }

    fn get_completed_ranges(
        &self,
        slot: Slot,
        start_index: u64,
    ) -> Result<(CompletedRanges, Option<SlotMeta>)> {
        let Some(slot_meta) = self.meta_cf.get(slot)? else {
            return Ok((vec![], None));
        };
        // Find all the ranges for the completed data blocks
        let completed_ranges = Self::get_completed_data_ranges(
            start_index as u32,
            &slot_meta.completed_data_indexes,
            slot_meta.consumed as u32,
        );

        Ok((completed_ranges, Some(slot_meta)))
    }

    // Get the range of indexes [start_index, end_index] of every completed data block
    fn get_completed_data_ranges(
        start_index: u32,
        completed_data_indexes: &CompletedDataIndexes,
        consumed: u32,
    ) -> CompletedRanges {
        // `consumed` is the next missing shred index, but shred `i` existing in
        // completed_data_end_indexes implies it's not missing
        assert!(!completed_data_indexes.contains(&consumed));

        // When using UpdateParent's replay_fec_set_index as start_index, the
        // shreds before that index might not have been received yet. For now,
        // let's do the dumb thing of waiting for the previous shreds to arrive
        // prior to replay so earlier conflicting UpdateParent markers are
        // observed before the suffix can execute.
        if start_index >= consumed {
            return vec![];
        }
        completed_data_indexes
            .range(start_index..consumed)
            .scan(start_index, |start, index| {
                let out = *start..index + 1;
                *start = index + 1;
                Some(out)
            })
            .collect()
    }

    /// Fetch the data corresponding to all of the shred indices in `completed_ranges`
    /// This function takes advantage of the fact that `completed_ranges` are both
    /// contiguous and in sorted order. To clarify, suppose completed_ranges is as follows:
    ///   completed_ranges = [..., (s_i..e_i), (s_i+1..e_i+1), ...]
    /// Then, the following statements are true:
    ///   s_i < e_i == s_i+1 < e_i+1
    fn get_slot_data_in_block<T>(
        &self,
        slot: Slot,
        completed_ranges: &CompletedRanges,
        slot_meta: Option<&SlotMeta>,
        mut deserialize: impl FnMut(Vec<u8>) -> Result<Vec<T>>,
    ) -> Result<Vec<T>> {
        debug_assert!(
            completed_ranges
                .iter()
                .tuple_windows()
                .all(|(a, b)| a.start < a.end && a.end == b.start && b.start < b.end)
        );
        let maybe_panic = |index: u64| {
            if let Some(slot_meta) = slot_meta
                && slot > self.lowest_cleanup_slot()
            {
                panic!("Missing shred. slot: {slot}, index: {index}, slot meta: {slot_meta:?}");
            }
        };
        let Some((&Range { start, .. }, &Range { end, .. })) =
            completed_ranges.first().zip(completed_ranges.last())
        else {
            return Ok(vec![]);
        };
        let indices = u64::from(start)..u64::from(end);
        let keys = indices.clone().map(|index| (slot, index));
        let keys = self.data_shred_cf.multi_get_keys(keys);
        let mut shreds =
            self.data_shred_cf
                .multi_get_bytes(&keys)
                .zip(indices)
                .map(|(shred, index)| {
                    shred?.ok_or_else(|| {
                        maybe_panic(index);
                        BlockstoreError::MissingShred(slot, index)
                    })
                });
        completed_ranges
            .iter()
            .map(|Range { start, end }| end - start)
            .map(|num_shreds| {
                shreds
                    .by_ref()
                    .take(num_shreds as usize)
                    .process_results(|shreds| Shredder::deshred(shreds))?
                    .map_err(|e| {
                        BlockstoreError::InvalidShredData(format!(
                            "could not reconstruct data buffer from shreds: {e}"
                        ))
                    })
                    .and_then(&mut deserialize)
            })
            .flatten_ok()
            .collect()
    }

    /// Fetch the components corresponding to all of the shred indices in `completed_ranges`.
    /// Note that one range in CompletedRanges corresponds to one BlockComponent.
    fn get_slot_components_in_block(
        &self,
        slot: Slot,
        completed_ranges: &CompletedRanges,
        slot_meta: Option<&SlotMeta>,
    ) -> Result<Vec<BlockComponent>> {
        self.get_slot_data_in_block(slot, completed_ranges, slot_meta, |payload| {
            wincode::deserialize(&payload)
                .map(|component| vec![component])
                .map_err(|e| {
                    if BlockComponent::infer_is_empty_entry_batch(&payload) {
                        BlockstoreError::BlockAborted(slot)
                    } else {
                        BlockstoreError::InvalidShredData(format!(
                            "could not reconstruct block component: {e}"
                        ))
                    }
                })
        })
    }

    /// Fetch the entries corresponding to all of the shred indices in `completed_ranges`.
    /// Block markers are skipped because they do not contain entries.
    fn get_slot_entries_in_block(
        &self,
        slot: Slot,
        completed_ranges: &CompletedRanges,
        slot_meta: Option<&SlotMeta>,
    ) -> Result<Vec<Entry>> {
        self.get_slot_data_in_block(slot, completed_ranges, slot_meta, |payload| {
            wincode::deserialize(&payload)
                .map(|component| match component {
                    BlockComponent::BlockMarker(_) => vec![],
                    BlockComponent::EntryBatch(entries) => entries,
                })
                .map_err(|e| {
                    if BlockComponent::infer_is_empty_entry_batch(&payload) {
                        BlockstoreError::BlockAborted(slot)
                    } else {
                        BlockstoreError::InvalidShredData(format!(
                            "could not reconstruct block component: {e}"
                        ))
                    }
                })
        })
    }

    pub fn get_entries_in_data_block(
        &self,
        slot: Slot,
        range: Range<u32>,
        slot_meta: Option<&SlotMeta>,
    ) -> Result<Vec<Entry>> {
        self.get_slot_entries_in_block(slot, &vec![range], slot_meta)
    }

    /// Returns a mapping from each elements of `slots` to a list of the
    /// element's children slots.
    pub fn get_slots_since(&self, slots: &[Slot]) -> Result<HashMap<Slot, Vec<Slot>>> {
        let keys = self.meta_cf.multi_get_keys(slots.iter().copied());
        let slot_metas = self.meta_cf.multi_get(&keys);

        let mut slots_since: HashMap<Slot, Vec<Slot>> = HashMap::with_capacity(slots.len());
        for meta in slot_metas.into_iter() {
            let meta = meta?;
            if let Some(meta) = meta {
                slots_since.insert(meta.slot, meta.next_slots);
            }
        }

        Ok(slots_since)
    }

    pub fn is_root(&self, slot: Slot) -> bool {
        matches!(self.roots_cf.get(slot), Ok(Some(true)))
    }

    /// Returns true if a slot is between the rooted slot bounds of the ledger, but has not itself
    /// been rooted. This is either because the slot was skipped, or due to a gap in ledger data,
    /// as when booting from a newer snapshot.
    pub fn is_skipped(&self, slot: Slot) -> bool {
        let lowest_root = self
            .rooted_slot_iterator(0)
            .ok()
            .and_then(|mut iter| iter.next())
            .unwrap_or_default();
        match self.roots_cf.get(slot).ok().flatten() {
            Some(_) => false,
            None => slot < self.max_root() && slot > lowest_root,
        }
    }

    pub fn insert_bank_hash(&self, slot: Slot, frozen_hash: Hash, is_duplicate_confirmed: bool) {
        if let Some(prev_value) = self.bank_hash_cf.get(slot).unwrap()
            && prev_value.frozen_hash() == frozen_hash
            && prev_value.is_duplicate_confirmed()
        {
            // Don't overwrite is_duplicate_confirmed == true with is_duplicate_confirmed == false,
            // which may happen on startup when processing from blockstore processor because the
            // blocks may not reflect earlier observed gossip votes from before the restart.
            return;
        }
        let data = FrozenHashVersioned::Current(FrozenHashStatus {
            frozen_hash,
            is_duplicate_confirmed,
        });
        self.bank_hash_cf.put(slot, &data).unwrap()
    }

    pub fn get_bank_hash(&self, slot: Slot) -> Option<Hash> {
        self.bank_hash_cf
            .get(slot)
            .unwrap()
            .map(|versioned| versioned.frozen_hash())
    }

    pub fn is_duplicate_confirmed(&self, slot: Slot) -> bool {
        self.bank_hash_cf
            .get(slot)
            .unwrap()
            .map(|versioned| versioned.is_duplicate_confirmed())
            .unwrap_or(false)
    }

    pub fn insert_optimistic_slot(
        &self,
        slot: Slot,
        hash: &Hash,
        timestamp: UnixTimestamp,
    ) -> Result<()> {
        let slot_data = OptimisticSlotMetaVersioned::new(*hash, timestamp);
        self.optimistic_slots_cf.put(slot, &slot_data)
    }

    /// Returns information about a single optimistically confirmed slot
    pub fn get_optimistic_slot(&self, slot: Slot) -> Result<Option<(Hash, UnixTimestamp)>> {
        Ok(self
            .optimistic_slots_cf
            .get(slot)?
            .map(|meta| (meta.hash(), meta.timestamp())))
    }

    /// Returns information about the `num` latest optimistically confirmed slot
    pub fn get_latest_optimistic_slots(
        &self,
        num: usize,
    ) -> Result<Vec<(Slot, Hash, UnixTimestamp)>> {
        let iter = self.reversed_optimistic_slots_iterator()?;
        Ok(iter.take(num).collect())
    }

    pub fn set_duplicate_confirmed_slots_and_hashes(
        &self,
        duplicate_confirmed_slot_hashes: impl Iterator<Item = (Slot, Hash)>,
    ) -> Result<()> {
        let mut write_batch = self.get_write_batch()?;
        for (slot, frozen_hash) in duplicate_confirmed_slot_hashes {
            let data = FrozenHashVersioned::Current(FrozenHashStatus {
                frozen_hash,
                is_duplicate_confirmed: true,
            });
            self.bank_hash_cf
                .put_in_batch(&mut write_batch, slot, &data)?;
        }

        self.write_batch(write_batch)?;
        Ok(())
    }

    pub fn set_roots<'a>(&self, rooted_slots: impl Iterator<Item = &'a Slot>) -> Result<()> {
        let mut write_batch = self.get_write_batch()?;
        let mut max_new_rooted_slot = 0;
        for slot in rooted_slots {
            max_new_rooted_slot = std::cmp::max(max_new_rooted_slot, *slot);
            self.roots_cf.put_in_batch(&mut write_batch, *slot, &true)?;
        }

        self.write_batch(write_batch)?;
        self.max_root
            .fetch_max(max_new_rooted_slot, Ordering::Relaxed);
        Ok(())
    }

    pub fn mark_slots_as_if_rooted_normally_at_startup(
        &self,
        slots: Vec<(Slot, Option<Hash>)>,
        with_hash: bool,
    ) -> Result<()> {
        self.set_roots(slots.iter().map(|(slot, _hash)| slot))?;
        if with_hash {
            self.set_duplicate_confirmed_slots_and_hashes(
                slots
                    .into_iter()
                    .map(|(slot, maybe_hash)| (slot, maybe_hash.unwrap())),
            )?;
        }
        Ok(())
    }

    pub fn is_dead(&self, slot: Slot) -> bool {
        matches!(
            self.dead_slots_cf
                .get(slot)
                .expect("fetch from DeadSlots column family failed"),
            Some(true)
        )
    }

    pub fn set_dead_slot(&self, slot: Slot) -> Result<()> {
        self.dead_slots_cf.put(slot, &true)
    }

    pub fn remove_dead_slot(&self, slot: Slot) -> Result<()> {
        self.dead_slots_cf.delete(slot)
    }

    pub fn remove_slot_duplicate_proof(&self, slot: Slot) -> Result<()> {
        self.duplicate_slots_cf.delete(slot)
    }

    pub fn get_first_duplicate_proof(&self) -> Option<(Slot, DuplicateSlotProof)> {
        let mut iter = self
            .duplicate_slots_cf
            .iter(IteratorMode::From(0, IteratorDirection::Forward))
            .unwrap();
        iter.next().map(|(slot, proof_bytes)| {
            (slot, cf::DuplicateSlots::deserialize(&proof_bytes).unwrap())
        })
    }

    pub fn store_duplicate_slot<S, T>(&self, slot: Slot, shred1: S, shred2: T) -> Result<()>
    where
        shred::Payload: From<S> + From<T>,
    {
        let duplicate_slot_proof = DuplicateSlotProof::new(shred1, shred2);
        self.duplicate_slots_cf.put(slot, &duplicate_slot_proof)
    }

    pub fn get_duplicate_slot(&self, slot: u64) -> Option<DuplicateSlotProof> {
        self.duplicate_slots_cf
            .get(slot)
            .expect("fetch from DuplicateSlots column family failed")
    }

    /// Returns the shred already stored in blockstore if it has a different
    /// payload than the given `shred` but the same (slot, index, shred-type).
    /// This implies the leader generated two different shreds with the same
    /// slot, index and shred-type.
    /// The payload is modified so that it has the same retransmitter's
    /// signature as the `shred` argument.
    pub fn is_shred_duplicate(&self, shred: &Shred) -> Option<Vec<u8>> {
        let (slot, index, shred_type) = shred.id().unpack();
        let mut other = match shred_type {
            ShredType::Data => self.get_data_shred(slot, u64::from(index)),
            ShredType::Code => self.get_coding_shred(slot, u64::from(index)),
        }
        .expect("fetch from DuplicateSlots column family failed")?;
        if let Ok(signature) = shred.retransmitter_signature()
            && let Err(err) = shred::layout::set_retransmitter_signature(&mut other, &signature)
        {
            error!("set retransmitter signature failed: {err:?}");
        }
        (other != **shred.payload()).then_some(other)
    }

    pub fn has_duplicate_shreds_in_slot(&self, slot: Slot) -> bool {
        self.duplicate_slots_cf
            .get(slot)
            .expect("fetch from DuplicateSlots column family failed")
            .is_some()
    }

    pub fn orphans_iterator(&self, slot: Slot) -> Result<impl Iterator<Item = u64> + '_> {
        let orphans_iter = self
            .orphans_cf
            .iter(IteratorMode::From(slot, IteratorDirection::Forward))?;
        Ok(orphans_iter.map(|(slot, _)| slot))
    }

    pub fn dead_slots_iterator(&self, slot: Slot) -> Result<impl Iterator<Item = Slot> + '_> {
        let dead_slots_iterator = self
            .dead_slots_cf
            .iter(IteratorMode::From(slot, IteratorDirection::Forward))?;
        Ok(dead_slots_iterator.map(|(slot, _)| slot))
    }

    pub fn duplicate_slots_iterator(&self, slot: Slot) -> Result<impl Iterator<Item = Slot> + '_> {
        let duplicate_slots_iterator = self
            .duplicate_slots_cf
            .iter(IteratorMode::From(slot, IteratorDirection::Forward))?;
        Ok(duplicate_slots_iterator.map(|(slot, _)| slot))
    }

    pub fn has_existing_shreds_for_slot(&self, slot: Slot) -> bool {
        match self.meta(slot).unwrap() {
            Some(meta) => meta.received > 0,
            None => false,
        }
    }

    /// Returns the max root or 0 if it does not exist
    pub fn max_root(&self) -> Slot {
        self.max_root.load(Ordering::Relaxed)
    }

    // find the first available slot in blockstore that has some data in it
    pub fn lowest_slot(&self) -> Slot {
        for (slot, meta) in self
            .slot_meta_iterator(0)
            .expect("unable to iterate over meta")
        {
            if slot > 0 && meta.received > 0 {
                return slot;
            }
        }
        // This means blockstore is empty, should never get here aside from right at boot.
        self.max_root()
    }

    fn lowest_slot_with_genesis(&self) -> Slot {
        for (slot, meta) in self
            .slot_meta_iterator(0)
            .expect("unable to iterate over meta")
        {
            if meta.received > 0 {
                return slot;
            }
        }
        // This means blockstore is empty, should never get here aside from right at boot.
        self.max_root()
    }

    /// Returns the highest available slot in the blockstore
    pub fn highest_slot(&self) -> Result<Option<Slot>> {
        let highest_slot = self
            .meta_cf
            .iter(IteratorMode::End)?
            .next()
            .map(|(slot, _)| slot);
        Ok(highest_slot)
    }

    pub fn lowest_cleanup_slot(&self) -> Slot {
        *self.lowest_cleanup_slot.read().unwrap()
    }

    /// Returns whether the blockstore has primary (read and write) access
    pub fn is_primary_access(&self) -> bool {
        self.db.is_primary_access()
    }

    /// Scan for any ancestors of the supplied `start_root` that are not
    /// marked as roots themselves. Mark any found slots as roots since
    /// the ancestor of a root is also inherently a root. Returns the
    /// number of slots that were actually updated.
    ///
    /// Arguments:
    ///  - `start_root`: The root to start scan from, or the highest root in
    ///    the blockstore if this value is `None`. This slot must be a root.
    ///  - `end_slot``: The slot to stop the scan at; the scan will continue to
    ///    the earliest slot in the Blockstore if this value is `None`.
    ///  - `exit`: Exit early if this flag is set to `true`.
    pub fn scan_and_fix_roots(
        &self,
        start_root: Option<Slot>,
        end_slot: Option<Slot>,
        exit: &AtomicBool,
    ) -> Result<usize> {
        // Hold the lowest_cleanup_slot read lock to prevent any cleaning of
        // the blockstore from another thread. Doing so will prevent a
        // possible inconsistency across column families where a slot is:
        //  - Identified as needing root repair by this thread
        //  - Cleaned from the blockstore by another thread (LedgerCleanupSerivce)
        //  - Marked as root via Self::set_root() by this this thread
        let lowest_cleanup_slot = self.lowest_cleanup_slot.read().unwrap();

        let start_root = if let Some(slot) = start_root {
            if !self.is_root(slot) {
                return Err(BlockstoreError::SlotNotRooted);
            }
            slot
        } else {
            self.max_root()
        };
        let end_slot = end_slot.unwrap_or(*lowest_cleanup_slot);
        let ancestor_iterator =
            AncestorIterator::new(start_root, self).take_while(|&slot| slot >= end_slot);

        let mut find_missing_roots = Measure::start("find_missing_roots");
        let mut roots_to_fix = vec![];
        for slot in ancestor_iterator.filter(|slot| !self.is_root(*slot)) {
            if exit.load(Ordering::Relaxed) {
                return Ok(0);
            }
            roots_to_fix.push(slot);
        }
        find_missing_roots.stop();
        let mut fix_roots = Measure::start("fix_roots");
        if !roots_to_fix.is_empty() {
            info!("{} slots to be rooted", roots_to_fix.len());
            let chunk_size = 100;
            for (i, chunk) in roots_to_fix.chunks(chunk_size).enumerate() {
                if exit.load(Ordering::Relaxed) {
                    return Ok(i * chunk_size);
                }
                trace!("{chunk:?}");
                self.set_roots(chunk.iter())?;
            }
        } else {
            debug!("No missing roots found in range {start_root} to {end_slot}");
        }
        fix_roots.stop();
        datapoint_info!(
            "blockstore-scan_and_fix_roots",
            (
                "find_missing_roots_us",
                find_missing_roots.as_us() as i64,
                i64
            ),
            ("num_roots_to_fix", roots_to_fix.len() as i64, i64),
            ("fix_roots_us", fix_roots.as_us() as i64, i64),
        );
        Ok(roots_to_fix.len())
    }

    /// Mark a root `slot` as connected, traverse `slot`'s children and update
    /// the children's connected status if appropriate.
    ///
    /// A ledger with a full path of blocks from genesis to the latest root will
    /// have all of the rooted blocks marked as connected such that new blocks
    /// could also be connected. However, starting from some root (such as from
    /// a snapshot) is a valid way to join a cluster. For this case, mark this
    /// root as connected such that the node that joined midway through can
    /// have their slots considered connected.
    pub fn set_and_chain_connected_on_root_and_next_slots(&self, root: Slot) -> Result<()> {
        let mut root_meta = self
            .meta(root)?
            .unwrap_or_else(|| SlotMeta::new(root, None));
        // If the slot was already connected, there is nothing to do as this slot's
        // children are also assumed to be appropriately connected
        if root_meta.is_connected() {
            return Ok(());
        }
        info!("Marking slot {root} and any full children slots as connected");
        let mut write_batch = self.get_write_batch()?;

        // Mark both connected bits on the root slot so that the flags for this
        // slot match the flags of slots that become connected the typical way.
        root_meta.set_parent_connected();
        root_meta.set_connected();
        self.meta_cf
            .put_in_batch(&mut write_batch, root_meta.slot, &root_meta)?;

        let mut next_slots = VecDeque::from(root_meta.next_slots);
        while !next_slots.is_empty() {
            let slot = next_slots.pop_front().unwrap();
            let mut meta = self.meta(slot)?.unwrap_or_else(|| {
                panic!("Slot {slot} is a child but has no SlotMeta in blockstore")
            });

            if meta.set_parent_connected() {
                next_slots.extend(meta.next_slots.iter());
            }
            self.meta_cf
                .put_in_batch(&mut write_batch, meta.slot, &meta)?;
        }

        self.write_batch(write_batch)?;
        Ok(())
    }

    /// For each entry in `working_set` whose `did_insert_occur` is true, this
    /// function handles its chaining effect by updating the SlotMeta of both
    /// the slot and its parent slot to reflect the slot descends from the
    /// parent slot.  In addition, when a slot is newly connected, it also
    /// checks whether any of its direct and indirect children slots are connected
    /// or not.
    ///
    /// This function also handles chaining updates when an UpdateParent marker
    /// overrides a previously set parent from a BlockHeader. The dirty SlotMetas
    /// identify slots that need reparenting.
    ///
    /// Note: This chaining only occurs for `SlotMeta`s in the column associated with
    /// `BlockLocation::Original`
    ///
    /// This function may update column families [`cf::SlotMeta`] and
    /// [`cf::Orphans`].
    ///
    /// For more information about the chaining, check the previous discussion here:
    /// https://github.com/solana-labs/solana/pull/2253
    ///
    /// Arguments:
    /// - `db`: the blockstore db that stores both shreds and their metadata.
    /// - `write_batch`: the write batch which includes all the updates of the
    ///   the current write and ensures their atomicity.
    /// - `working_set`: a (location, slot-id) to SlotMetaWorkingSetEntry map.  This function
    ///   will remove all entries which insertion did not actually occur.
    fn handle_chaining(
        &self,
        write_batch: &mut WriteBatch,
        working_set: &mut HashMap<(BlockLocation, u64), SlotMetaWorkingSetEntry>,
        metrics: &mut BlockstoreInsertionMetrics,
    ) -> Result<()> {
        let mut start = Measure::start("Shred chaining");
        // Handle chaining for all the SlotMetas that were inserted into
        working_set.retain(|_, entry| entry.did_insert_occur);
        let mut new_chained_slots = HashMap::new();
        for (location, slot) in working_set.keys() {
            if !matches!(location, BlockLocation::Original) {
                // We do not perform SlotMeta chaining for alternate versions of slots.
                // We only chain SlotMeta across the original column.
                //
                // Alternate versions of blocks are stored in blockstore, but switching them into replay is
                // handled separately.
                continue;
            }
            self.handle_chaining_for_slot(write_batch, working_set, &mut new_chained_slots, *slot)?;
        }

        // Handle reparenting from UpdateParent markers
        self.update_chaining_for_updated_parent_slots(
            write_batch,
            working_set,
            &mut new_chained_slots,
        )?;

        // Write all the newly changed slots in new_chained_slots to the write_batch
        for (slot, meta) in new_chained_slots.iter() {
            let meta: &SlotMeta = &RefCell::borrow(meta);
            self.meta_cf.put_in_batch(write_batch, *slot, meta)?;
        }
        start.stop();
        metrics.chaining_elapsed_us += start.as_us();
        Ok(())
    }

    /// A helper function of handle_chaining which handles the chaining based
    /// on the `SlotMetaWorkingSetEntry` of the specified `slot`.  Specifically,
    /// it handles the following two things:
    ///
    /// 1. based on the `SlotMetaWorkingSetEntry` for `slot`, check if `slot`
    ///    did not previously have a parent slot but does now.  If `slot` satisfies
    ///    this condition, update the Orphan property of both `slot` and its parent
    ///    slot based on their current orphan status.  Specifically:
    ///  - updates the orphan property of slot to no longer be an orphan because
    ///    it has a parent.
    ///  - adds the parent to the orphan column family if the parent's parent is
    ///    currently unknown.
    ///
    /// 2. if the `SlotMetaWorkingSetEntry` for `slot` indicates this slot
    ///    is newly connected to a parent slot, then this function will update
    ///    the is_connected property of all its direct and indirect children slots.
    ///
    /// This function may update column family [`cf::Orphans`] and indirectly
    /// update SlotMeta from its output parameter `new_chained_slots`.
    ///
    /// Note: This function works under the assumption that `slot` refers to the
    /// column associated with `BlockLocation::Original`. `SlotMeta` chaining for
    /// alternate versions is not supported.
    ///
    /// Arguments:
    /// `db`: the underlying db for blockstore
    /// `write_batch`: the write batch which includes all the updates of the
    ///   the current write and ensures their atomicity.
    /// `working_set`: the working set which include the specified `slot`
    /// `new_chained_slots`: an output parameter which includes all the slots
    ///   which connectivity have been updated.
    /// `slot`: the slot which we want to handle its chaining effect.
    fn handle_chaining_for_slot(
        &self,
        write_batch: &mut WriteBatch,
        working_set: &HashMap<(BlockLocation, u64), SlotMetaWorkingSetEntry>,
        new_chained_slots: &mut HashMap<u64, Rc<RefCell<SlotMeta>>>,
        slot: Slot,
    ) -> Result<()> {
        let slot_meta_entry = working_set
            .get(&(BlockLocation::Original, slot))
            .expect("Slot must exist in the working_set hashmap");

        let meta = &slot_meta_entry.new_slot_meta;
        let meta_backup = &slot_meta_entry.old_slot_meta;
        {
            let mut meta_mut = meta.borrow_mut();
            let was_orphan_slot =
                meta_backup.is_some() && meta_backup.as_ref().unwrap().is_orphan();

            // If:
            // 1) This is a new slot
            // 2) slot != 0
            // then try to chain this slot to a previous slot
            if slot != 0 && meta_mut.parent_slot.is_some() {
                let prev_slot = meta_mut.parent_slot.unwrap();

                // Check if the slot represented by meta_mut is either a new slot or a orphan.
                // In both cases we need to run the chaining logic b/c the parent on the slot was
                // previously unknown.
                if meta_backup.is_none() || was_orphan_slot {
                    let prev_slot_meta =
                        self.find_slot_meta_else_create(working_set, new_chained_slots, prev_slot)?;

                    // This is a newly inserted slot/orphan so run the chaining logic to link it to a
                    // newly discovered parent
                    chain_new_slot_to_prev_slot(
                        &mut prev_slot_meta.borrow_mut(),
                        slot,
                        &mut meta_mut,
                    );

                    // If the parent of `slot` is a newly inserted orphan, insert it into the orphans
                    // column family
                    if RefCell::borrow(&*prev_slot_meta).is_orphan() {
                        self.orphans_cf
                            .put_in_batch(write_batch, prev_slot, &true)?;
                    }
                }
            }

            // At this point this slot has received a parent, so it's no longer an orphan
            if was_orphan_slot {
                self.orphans_cf.delete_in_batch(write_batch, slot);
            }
        }

        // If this is a newly completed slot and the parent is connected, then the
        // slot is now connected. Mark the slot as connected, and then traverse the
        // children to update their parent_connected and connected status.
        let should_propagate_is_connected =
            is_newly_completed_slot(&RefCell::borrow(meta), meta_backup)
                && RefCell::borrow(meta).is_parent_connected();

        if should_propagate_is_connected {
            meta.borrow_mut().set_connected();
            self.propagate_parent_connected_to_children(meta, working_set, new_chained_slots)?;
        }

        Ok(())
    }

    /// Propagate `parent_connected` to children. Requires `slot_meta` to be connected.
    fn propagate_parent_connected_to_children(
        &self,
        slot_meta: &Rc<RefCell<SlotMeta>>,
        working_set: &HashMap<(BlockLocation, u64), SlotMetaWorkingSetEntry>,
        new_chained_slots: &mut HashMap<u64, Rc<RefCell<SlotMeta>>>,
    ) -> Result<()> {
        debug_assert!(slot_meta.borrow().is_connected());
        self.traverse_children_mut(
            slot_meta,
            working_set,
            new_chained_slots,
            SlotMeta::set_parent_connected,
        )
    }

    /// Handles chaining updates when an `UpdateParent` marker overrides a
    /// previously set parent in `SlotMeta`.
    fn update_chaining_for_updated_parent_slots(
        &self,
        write_batch: &mut WriteBatch,
        working_set: &HashMap<(BlockLocation, u64), SlotMetaWorkingSetEntry>,
        new_chained_slots: &mut HashMap<u64, Rc<RefCell<SlotMeta>>>,
    ) -> Result<()> {
        // Process only existing slots in Original whose parent was updated by UpdateParent.
        for (slot, slot_meta, old_parent_slot, new_parent_slot) in
            working_set
                .iter()
                .filter_map(|(&(location, slot), slot_meta_entry)| {
                    if !matches!(location, BlockLocation::Original) {
                        return None;
                    }

                    let old_slot_meta = slot_meta_entry.old_slot_meta.as_ref()?;
                    if old_slot_meta.is_orphan() {
                        return None;
                    }

                    let new_slot_meta = slot_meta_entry.new_slot_meta.borrow();
                    if new_slot_meta.populated_from_block_header() {
                        return None;
                    }

                    let new_parent_slot = new_slot_meta.parent_slot?;
                    let old_parent_slot = old_slot_meta.parent_slot?;
                    (old_parent_slot != new_parent_slot).then_some((
                        slot,
                        slot_meta_entry.new_slot_meta.clone(),
                        old_parent_slot,
                        new_parent_slot,
                    ))
                })
        {
            // Remove slot from old parent's next_slots if parent changed.
            self.find_slot_meta_else_create(working_set, new_chained_slots, old_parent_slot)?
                .borrow_mut()
                .next_slots
                .retain(|&s| s != slot);

            // Add slot to new parent's next_slots.
            let new_parent_meta =
                self.find_slot_meta_else_create(working_set, new_chained_slots, new_parent_slot)?;
            {
                let mut new_meta = new_parent_meta.borrow_mut();
                if !new_meta.next_slots.contains(&slot) {
                    new_meta.next_slots.push(slot);
                }
            }

            // If the new parent of `slot` is a newly inserted orphan, insert it into the orphans
            // column family
            if new_parent_meta.borrow().is_orphan() {
                self.orphans_cf
                    .put_in_batch(write_batch, new_parent_slot, &true)?;
            }

            // Propagate or clear connectivity based on new parent's state.
            if new_parent_meta.borrow().is_connected() {
                if !slot_meta.borrow().is_parent_connected() {
                    let became_connected = slot_meta.borrow_mut().set_parent_connected();
                    if became_connected {
                        self.propagate_parent_connected_to_children(
                            &slot_meta,
                            working_set,
                            new_chained_slots,
                        )?;
                    }
                }
            } else if slot_meta.borrow_mut().clear_parent_connected() {
                self.traverse_children_mut(
                    &slot_meta,
                    working_set,
                    new_chained_slots,
                    SlotMeta::clear_parent_connected,
                )?;
            }
        }

        Ok(())
    }

    /// Traverse all the children (direct and indirect) of `slot_meta`, and apply
    /// `slot_function` to each of the children in `BlockLocation::Original`
    /// (but not `slot_meta`).
    ///
    /// Arguments:
    /// `db`: the blockstore db that stores shreds and their metadata.
    /// `slot_meta`: the SlotMeta of the above `slot`.
    /// `working_set`: a (location, slot-id) to SlotMetaWorkingSetEntry map which is used
    ///   to traverse the graph.
    /// `passed_visited_slots`: all the traversed slots which have passed the
    ///   slot_function.  This may also include the input `slot`.
    /// `slot_function`: a function which updates the SlotMeta of the visisted
    ///   slots and determine whether to further traverse the children slots of
    ///   a given slot.
    fn traverse_children_mut<F>(
        &self,
        slot_meta: &Rc<RefCell<SlotMeta>>,
        working_set: &HashMap<(BlockLocation, u64), SlotMetaWorkingSetEntry>,
        passed_visited_slots: &mut HashMap<u64, Rc<RefCell<SlotMeta>>>,
        slot_function: F,
    ) -> Result<()>
    where
        F: Fn(&mut SlotMeta) -> bool,
    {
        let slot_meta = slot_meta.borrow();
        let mut next_slots: VecDeque<u64> = slot_meta.next_slots.to_vec().into();
        while !next_slots.is_empty() {
            let slot = next_slots.pop_front().unwrap();
            let meta_ref =
                self.find_slot_meta_else_create(working_set, passed_visited_slots, slot)?;
            let mut meta = meta_ref.borrow_mut();
            if slot_function(&mut meta) {
                meta.next_slots
                    .iter()
                    .for_each(|slot| next_slots.push_back(*slot));
            }
        }
        Ok(())
    }

    /// For each slot in the slot_meta_working_set which has any change, include
    /// corresponding updates to cf::SlotMeta via the specified `write_batch`.
    /// The `write_batch` will later be atomically committed to the blockstore.
    ///
    /// Arguments:
    /// - `slot_meta_working_set`: a map that maintains slot-id to its `SlotMeta`
    ///   mapping.
    /// - `write_batch`: the write batch which includes all the updates of the
    ///   the current write and ensures their atomicity.
    ///
    /// On success, the function returns an Ok result with <should_signal,
    /// newly_completed_slots> pair where:
    ///  - `should_signal`: a boolean flag indicating whether to send signal.
    ///  - `newly_completed_slots`: a subset of slot_meta_working_set which are
    ///    newly completed.
    fn commit_slot_meta_working_set(
        &self,
        slot_meta_working_set: &HashMap<(BlockLocation, u64), SlotMetaWorkingSetEntry>,
        write_batch: &mut WriteBatch,
    ) -> Result<(
        /* signal slot updates */ bool,
        /* slots updated */ Vec<u64>,
        /* update parent signals */ Vec<UpdateParentSignal>,
    )> {
        let mut should_signal = false;
        let mut newly_completed_slots = vec![];
        let mut update_parent_signals = Vec::new();
        let completed_slots_senders = self.completed_slots_senders.lock().unwrap();

        // Check if any metadata was changed, if so, insert the new version of the
        // metadata into the write batch
        for (&(location, slot), slot_meta_entry) in slot_meta_working_set.iter() {
            // Any slot that wasn't written to should have been filtered out by now.
            assert!(slot_meta_entry.did_insert_occur);
            let meta: &SlotMeta = &RefCell::borrow(&*slot_meta_entry.new_slot_meta);
            let meta_backup = &slot_meta_entry.old_slot_meta;
            if !completed_slots_senders.is_empty() && is_newly_completed_slot(meta, meta_backup) {
                newly_completed_slots.push(slot);
            }
            // Check if the working copy of the metadata has changed
            if Some(meta) != meta_backup.as_ref() {
                should_signal = should_signal || slot_has_updates(meta, meta_backup);
                self.put_meta_in_batch(write_batch, slot, location, meta)?;
            }

            // Check for new ``UpdateParent`s in the `Original` column
            // This signal is only used for replay, so we don't need to consider `Alternate` columns
            if location == BlockLocation::Original
                && meta.has_update_parent()
                && meta_backup
                    .as_ref()
                    .is_some_and(|m| m.populated_from_block_header())
            {
                update_parent_signals.push(UpdateParentSignal { slot });
            }
        }

        Ok((should_signal, newly_completed_slots, update_parent_signals))
    }

    /// Obtain the SlotMeta from the in-memory slot_meta_working_set or load
    /// it from the database if it does not exist in slot_meta_working_set.
    ///
    /// In case none of the above has the specified SlotMeta, a new one will
    /// be created.
    ///
    /// Note that this function will also update the parent slot of the specified
    /// slot.
    ///
    /// Arguments:
    /// - `db`: the database
    /// - `slot_meta_working_set`: a in-memory structure for storing the cached
    ///   SlotMeta.
    /// - `slot`: the slot for loading its meta.
    /// - `location`: the column to query
    /// - `parent_slot`: the parent slot to be assigned to the specified slot meta
    ///
    /// This function returns the matched `SlotMetaWorkingSetEntry`.  If such entry
    /// does not exist in the database, a new entry will be created.
    fn get_slot_meta_entry<'a>(
        &self,
        slot_meta_working_set: &'a mut HashMap<(BlockLocation, u64), SlotMetaWorkingSetEntry>,
        slot: Slot,
        location: BlockLocation,
        parent_slot: Slot,
    ) -> Result<&'a mut SlotMetaWorkingSetEntry> {
        // Check if we've already inserted the slot metadata for this shred's slot
        let entry = match slot_meta_working_set.entry((location, slot)) {
            HashMapEntry::Occupied(occupied_entry) => occupied_entry.into_mut(),
            HashMapEntry::Vacant(vacant_entry) => {
                let meta = self.meta_from_location(slot, location)?;
                // Insert a new 2-tuple of the metadata (working copy, backup copy)
                let slot_meta_entry = if let Some(mut meta) = meta {
                    let backup = Some(meta.clone());
                    if meta.is_orphan() {
                        meta.parent_slot = Some(parent_slot);
                    }
                    SlotMetaWorkingSetEntry::new(Rc::new(RefCell::new(meta)), backup)
                } else {
                    SlotMetaWorkingSetEntry::new(
                        Rc::new(RefCell::new(SlotMeta::new(slot, Some(parent_slot)))),
                        None,
                    )
                };
                vacant_entry.insert(slot_meta_entry)
            }
        };
        Ok(entry)
    }

    /// Returns the `SlotMeta` with the specified `slot_index` from the column associated
    /// with `BlockLocation::Original`. The resulting `SlotMeta` could be either from the cache
    /// or from the DB. Specifically, the function:
    ///
    /// 1) Finds the slot metadata in the cache of dirty slot metadata we've
    ///    previously touched, otherwise:
    /// 2) Searches the database for that slot metadata. If still no luck, then:
    /// 3) Create a dummy orphan slot in the database.
    ///
    /// Also see [`find_slot_meta_in_cached_state`] and [`find_slot_meta_in_db_else_create`].
    fn find_slot_meta_else_create<'a>(
        &self,
        working_set: &'a HashMap<(BlockLocation, u64), SlotMetaWorkingSetEntry>,
        chained_slots: &'a mut HashMap<u64, Rc<RefCell<SlotMeta>>>,
        slot_index: u64,
    ) -> Result<Rc<RefCell<SlotMeta>>> {
        let result = find_slot_meta_in_cached_state(working_set, chained_slots, slot_index);
        if let Some(slot) = result {
            Ok(slot)
        } else {
            self.find_slot_meta_in_db_else_create(slot_index, chained_slots)
        }
    }

    /// A helper function to [`find_slot_meta_else_create`] that searches the
    /// `SlotMeta` based on the specified `slot` in the `BlockLocation::Original` column
    /// of `db` and updates `insert_map`.
    ///
    /// If the specified `db` does not contain a matched entry, then it will create
    /// a dummy orphan slot in the database.
    fn find_slot_meta_in_db_else_create(
        &self,
        slot: Slot,
        insert_map: &mut HashMap<u64, Rc<RefCell<SlotMeta>>>,
    ) -> Result<Rc<RefCell<SlotMeta>>> {
        if let Some(slot_meta) = self.meta_cf.get(slot)? {
            insert_map.insert(slot, Rc::new(RefCell::new(slot_meta)));
        } else {
            // If this slot doesn't exist, make a orphan slot. This way we
            // remember which slots chained to this one when we eventually get a real shred
            // for this slot
            insert_map.insert(slot, Rc::new(RefCell::new(SlotMeta::new_orphan(slot))));
        }
        Ok(insert_map.get(&slot).unwrap().clone())
    }

    fn get_index_meta_entry<'a>(
        &self,
        slot: Slot,
        location: BlockLocation,
        index_working_set: &'a mut HashMap<(BlockLocation, u64), IndexMetaWorkingSetEntry>,
        index_meta_time_us: &mut u64,
    ) -> Result<&'a mut IndexMetaWorkingSetEntry> {
        let mut total_start = Measure::start("Total elapsed");
        let index_meta_entry = match index_working_set.entry((location, slot)) {
            HashMapEntry::Occupied(occupied_entry) => occupied_entry.into_mut(),
            HashMapEntry::Vacant(vacant_entry) => {
                let index = self
                    .get_index_from_location(slot, location)?
                    .unwrap_or_else(|| Index::new(slot));
                let index_entry = IndexMetaWorkingSetEntry {
                    index,
                    did_insert_occur: false,
                };
                vacant_entry.insert(index_entry)
            }
        };
        total_start.stop();
        *index_meta_time_us += total_start.as_us();

        Ok(index_meta_entry)
    }

    pub fn get_write_batch(&self) -> Result<WriteBatch> {
        self.db.batch()
    }

    pub fn write_batch(&self, write_batch: WriteBatch) -> Result<()> {
        self.db.write(write_batch)
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn insert_shreds_for_bank(&self, bank: Arc<Bank>) {
        let entries = create_ticks(bank.ticks_per_slot(), 1, Hash::new_unique());
        let shreds = entries_to_test_shreds(&entries, bank.slot(), bank.parent_slot(), true, 0);
        self.insert_shreds(shreds, false).unwrap();
    }
}

// Updates the `completed_data_indexes` with a new shred `new_shred_index`.
// If a data set is complete, returns the range of shred indexes
//     start_index..end_index
// for that completed data set.
fn update_completed_data_indexes<'a>(
    is_last_in_data: bool,
    new_shred_index: u32,
    received_data_shreds: &'a ShredIndex,
    // Shreds indices which are marked data complete.
    completed_data_indexes: &mut CompletedDataIndexes,
) -> impl Iterator<Item = Range<u32>> + 'a + use<'a> {
    // new_shred_index is data complete, so need to insert here into
    // the completed_data_indexes.
    if is_last_in_data {
        completed_data_indexes.insert(new_shred_index);
    }
    // Consecutive entries i, j, k in this array represent potential ranges
    // [i, j), [j, k) that could be completed data ranges
    [
        completed_data_indexes
            .previous_completed_index(new_shred_index)
            .map(|index| index + 1)
            .or(Some(0u32)),
        is_last_in_data.then_some(new_shred_index + 1),
        completed_data_indexes
            .next_completed_index(new_shred_index + 1)
            .map(|index| index + 1),
    ]
    .into_iter()
    .flatten()
    .tuple_windows()
    .filter(|&(start, end)| received_data_shreds.contains_range(u64::from(start)..u64::from(end)))
    .map(|(start, end)| start..end)
}

fn update_slot_meta<'a>(
    is_last_in_slot: bool,
    is_last_in_data: bool,
    slot_meta: &mut SlotMeta,
    index: u32,
    new_consumed: u64,
    received_data_shreds: &'a ShredIndex,
) -> impl Iterator<Item = Range<u32>> + 'a + use<'a> {
    let first_insert = slot_meta.received == 0;
    // Index is zero-indexed, while the "received" height starts from 1,
    // so received = index + 1 for the same shred.
    slot_meta.received = cmp::max(u64::from(index) + 1, slot_meta.received);
    if first_insert {
        slot_meta.first_shred_timestamp = timestamp();
    }
    slot_meta.consumed = new_consumed;
    // If the last index in the slot hasn't been set before, then
    // set it to this shred index
    if is_last_in_slot && slot_meta.last_index.is_none() {
        slot_meta.last_index = Some(u64::from(index));
    }
    update_completed_data_indexes(
        is_last_in_slot || is_last_in_data,
        index,
        received_data_shreds,
        &mut slot_meta.completed_data_indexes,
    )
}

fn send_signals(
    new_shreds_signals: &[Sender<bool>],
    completed_slots_senders: &[Sender<Vec<u64>>],
    update_parent_senders: &[UpdateParentSender],
    should_signal: bool,
    newly_completed_slots: Vec<u64>,
    update_parent_signals: Vec<UpdateParentSignal>,
) {
    if should_signal {
        for signal in new_shreds_signals {
            match signal.try_send(true) {
                Ok(_) => {}
                Err(TrySendError::Full(_)) => {
                    trace!("replay wake up signal channel is full.")
                }
                Err(TrySendError::Disconnected(_)) => {
                    trace!("replay wake up signal channel is disconnected.")
                }
            }
        }
    }

    if !completed_slots_senders.is_empty() && !newly_completed_slots.is_empty() {
        let mut slots: Vec<_> = (0..completed_slots_senders.len() - 1)
            .map(|_| newly_completed_slots.clone())
            .collect();

        slots.push(newly_completed_slots);

        for (signal, slots) in completed_slots_senders.iter().zip(slots) {
            let res = signal.try_send(slots);
            if let Err(TrySendError::Full(_)) = res {
                datapoint_error!(
                    "blockstore_error",
                    (
                        "error",
                        "Unable to send newly completed slot because channel is full",
                        String
                    ),
                );
            }
        }
    }

    for signal in update_parent_signals {
        for sender in update_parent_senders {
            if let Err(TrySendError::Full(_)) = sender.try_send(signal.clone()) {
                error!(
                    "update_parent channel full, dropping signal for slot {}",
                    signal.slot
                );
                datapoint_error!(
                    "blockstore_error",
                    ("error", "update_parent channel full", String),
                    ("slot", signal.slot, i64),
                );
            }
        }
    }
}

/// Returns the `SlotMeta` of the specified `slot` associated with `BlockLocation::Original`
/// from the two cached states: `working_set` and `chained_slots`.  If both contain the `SlotMeta`,
/// then the latest one from the `working_set` will be returned.
fn find_slot_meta_in_cached_state<'a>(
    working_set: &'a HashMap<(BlockLocation, u64), SlotMetaWorkingSetEntry>,
    chained_slots: &'a HashMap<u64, Rc<RefCell<SlotMeta>>>,
    slot: Slot,
) -> Option<Rc<RefCell<SlotMeta>>> {
    if let Some(entry) = working_set.get(&(BlockLocation::Original, slot)) {
        Some(entry.new_slot_meta.clone())
    } else {
        chained_slots.get(&slot).cloned()
    }
}

// 1) Chain current_slot to the previous slot defined by prev_slot_meta
fn chain_new_slot_to_prev_slot(
    prev_slot_meta: &mut SlotMeta,
    current_slot: Slot,
    current_slot_meta: &mut SlotMeta,
) {
    prev_slot_meta.next_slots.push(current_slot);
    if prev_slot_meta.is_connected() {
        current_slot_meta.set_parent_connected();
    }
}

fn is_newly_completed_slot(slot_meta: &SlotMeta, backup_slot_meta: &Option<SlotMeta>) -> bool {
    slot_meta.is_full()
        && (backup_slot_meta.is_none()
            || slot_meta.consumed != backup_slot_meta.as_ref().unwrap().consumed)
}

/// Returns a boolean indicating whether a slot has received additional shreds
/// that can be replayed since the previous update to the slot's SlotMeta.
fn slot_has_updates(slot_meta: &SlotMeta, slot_meta_backup: &Option<SlotMeta>) -> bool {
    // First, this slot's parent must be connected in order to even consider
    // starting replay; otherwise, the replayed results may not be valid.
    slot_meta.is_parent_connected() &&
        // Then,
        // If the slot didn't exist in the db before, any consecutive shreds
        // at the start of the slot are ready to be replayed.
        ((slot_meta_backup.is_none() && slot_meta.consumed != 0) ||
        // Or,
        // If the slot has more consecutive shreds than it last did from the
        // last update, those shreds are new and also ready to be replayed.
        (slot_meta_backup.is_some() && slot_meta_backup.as_ref().unwrap().consumed != slot_meta.consumed))
}

// Creates a new ledger with slot 0 full of ticks (and only ticks).
//
// Returns the blockhash that can be used to append entries with.
pub fn create_new_ledger(
    ledger_path: &Path,
    genesis_config: &GenesisConfig,
    max_genesis_archive_unpacked_size: u64,
    column_options: LedgerColumnOptions,
) -> Result<Hash> {
    Blockstore::destroy(ledger_path)?;
    genesis_config.write(ledger_path)?;

    // Fill slot 0 with ticks that link back to the genesis_config to bootstrap the ledger.
    let blockstore_dir = BLOCKSTORE_DIRECTORY_ROCKS_LEVEL;
    let blockstore = Blockstore::open_with_options(
        ledger_path,
        BlockstoreOptions {
            column_options,
            ..BlockstoreOptions::default()
        },
    )?;
    let ticks_per_slot = genesis_config.ticks_per_slot;
    // Slot-0 tick entries are created before a Bank exists, so use the
    // genesis PoH config directly.
    let hashes_per_tick = genesis_config.poh_config.hashes_per_tick.unwrap_or(0);
    let entries = create_ticks(ticks_per_slot, hashes_per_tick, genesis_config.hash());
    let last_hash = entries.last().unwrap().hash;
    let version = solana_shred_version::version_from_hash(&last_hash);
    // Slot 0 has no parent slot so there is nothing to chain to; instead,
    // initialize the chained merkle root with the genesis hash
    let chained_merkle_root = genesis_config.hash();

    let shredder = Shredder::new(0, 0, 0, version).unwrap();
    let (shreds, _) = shredder.entries_to_merkle_shreds_for_tests(
        &Keypair::new(),
        &entries,
        true, // is_last_in_slot
        chained_merkle_root,
        0, // next_shred_index
        0, // next_code_index
        &ReedSolomonCache::default(),
        &mut ProcessShredsStats::default(),
    );
    assert!(shreds.last().unwrap().last_in_slot());

    blockstore.insert_shreds(shreds, false)?;
    blockstore.set_roots(std::iter::once(&0))?;
    // Explicitly close the blockstore before we create the archived genesis file
    drop(blockstore);

    let archive_path = ledger_path.join(DEFAULT_GENESIS_ARCHIVE);
    let archive_file = File::create(&archive_path)?;
    let encoder = bzip2::write::BzEncoder::new(archive_file, bzip2::Compression::best());
    let mut archive = tar::Builder::new(encoder);
    archive.append_path_with_name(ledger_path.join(DEFAULT_GENESIS_FILE), DEFAULT_GENESIS_FILE)?;
    archive.append_dir_all(blockstore_dir, ledger_path.join(blockstore_dir))?;
    archive.into_inner()?;

    // ensure the genesis archive can be unpacked and it is under
    // max_genesis_archive_unpacked_size, immediately after creating it above.
    {
        let temp_dir = tempfile::tempdir_in(ledger_path).unwrap();
        // unpack into a temp dir, while completely discarding the unpacked files
        let unpack_check = unpack_genesis_archive(
            &archive_path,
            temp_dir.path(),
            max_genesis_archive_unpacked_size,
        );
        if let Err(unpack_err) = unpack_check {
            // stash problematic original archived genesis related files to
            // examine them later and to prevent validator and ledger-tool from
            // naively consuming them
            let mut error_messages = String::new();

            fs::rename(
                ledger_path.join(DEFAULT_GENESIS_ARCHIVE),
                ledger_path.join(format!("{DEFAULT_GENESIS_ARCHIVE}.failed")),
            )
            .unwrap_or_else(|e| {
                let _ = write!(
                    &mut error_messages,
                    "/failed to stash problematic {DEFAULT_GENESIS_ARCHIVE}: {e}"
                );
            });
            fs::rename(
                ledger_path.join(DEFAULT_GENESIS_FILE),
                ledger_path.join(format!("{DEFAULT_GENESIS_FILE}.failed")),
            )
            .unwrap_or_else(|e| {
                let _ = write!(
                    &mut error_messages,
                    "/failed to stash problematic {DEFAULT_GENESIS_FILE}: {e}"
                );
            });
            fs::rename(
                ledger_path.join(blockstore_dir),
                ledger_path.join(format!("{blockstore_dir}.failed")),
            )
            .unwrap_or_else(|e| {
                let _ = write!(
                    &mut error_messages,
                    "/failed to stash problematic {blockstore_dir}: {e}"
                );
            });

            return Err(BlockstoreError::Io(IoError::other(format!(
                "Error checking to unpack genesis archive: {unpack_err}{error_messages}"
            ))));
        }
    }

    Ok(last_hash)
}

#[macro_export]
macro_rules! tmp_ledger_name {
    () => {
        &format!("{}-{}", file!(), line!())
    };
}

#[macro_export]
macro_rules! get_tmp_ledger_path {
    () => {
        $crate::blockstore::get_ledger_path_from_name($crate::tmp_ledger_name!())
    };
}

#[macro_export]
macro_rules! get_tmp_ledger_path_auto_delete {
    () => {
        $crate::blockstore::get_ledger_path_from_name_auto_delete($crate::tmp_ledger_name!())
    };
}

pub fn get_ledger_path_from_name_auto_delete(name: &str) -> TempDir {
    let mut path = get_ledger_path_from_name(name);
    // path is a directory so .file_name() returns the last component of the path
    let last = path.file_name().unwrap().to_str().unwrap().to_string();
    path.pop();
    fs::create_dir_all(&path).unwrap();
    Builder::new()
        .prefix(&last)
        .rand_bytes(0)
        .tempdir_in(path)
        .unwrap()
}

pub fn get_ledger_path_from_name(name: &str) -> PathBuf {
    use std::env;
    let out_dir = env::var("FARF_DIR").unwrap_or_else(|_| "farf".to_string());
    let keypair = Keypair::new();

    let path = [
        out_dir,
        "ledger".to_string(),
        format!("{}-{}", name, keypair.pubkey()),
    ]
    .iter()
    .collect();

    // whack any possible collision
    let _ignored = fs::remove_dir_all(&path);

    path
}

#[macro_export]
macro_rules! create_new_tmp_ledger {
    ($genesis_config:expr) => {
        $crate::blockstore::create_new_ledger_from_name(
            $crate::tmp_ledger_name!(),
            $genesis_config,
            $crate::macro_reexports::MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            $crate::blockstore_options::LedgerColumnOptions::default(),
        )
    };
}

#[macro_export]
macro_rules! create_new_tmp_ledger_auto_delete {
    ($genesis_config:expr) => {
        $crate::blockstore::create_new_ledger_from_name_auto_delete(
            $crate::tmp_ledger_name!(),
            $genesis_config,
            $crate::macro_reexports::MAX_GENESIS_ARCHIVE_UNPACKED_SIZE,
            $crate::blockstore_options::LedgerColumnOptions::default(),
        )
    };
}

pub(crate) fn verify_shred_slots(slot: Slot, parent: Slot, root: Slot) -> bool {
    if slot == 0 && parent == 0 && root == 0 {
        return true; // valid write to slot zero.
    }
    // Ignore shreds that chain to slots before the root,
    // or have invalid parent >= slot.
    root <= parent && parent < slot
}

// Same as `create_new_ledger()` but use a temporary ledger name based on the provided `name`
//
// Note: like `create_new_ledger` the returned ledger will have slot 0 full of ticks (and only
// ticks)
pub fn create_new_ledger_from_name(
    name: &str,
    genesis_config: &GenesisConfig,
    max_genesis_archive_unpacked_size: u64,
    column_options: LedgerColumnOptions,
) -> (PathBuf, Hash) {
    let (ledger_path, blockhash) = create_new_ledger_from_name_auto_delete(
        name,
        genesis_config,
        max_genesis_archive_unpacked_size,
        column_options,
    );
    (ledger_path.keep(), blockhash)
}

// Same as `create_new_ledger()` but use a temporary ledger name based on the provided `name`
//
// Note: like `create_new_ledger` the returned ledger will have slot 0 full of ticks (and only
// ticks)
pub fn create_new_ledger_from_name_auto_delete(
    name: &str,
    genesis_config: &GenesisConfig,
    max_genesis_archive_unpacked_size: u64,
    column_options: LedgerColumnOptions,
) -> (TempDir, Hash) {
    let ledger_path = get_ledger_path_from_name_auto_delete(name);
    let blockhash = create_new_ledger(
        ledger_path.path(),
        genesis_config,
        max_genesis_archive_unpacked_size,
        column_options,
    )
    .unwrap();
    (ledger_path, blockhash)
}

#[cfg(feature = "dev-context-only-utils")]
pub fn entries_to_test_shreds(
    entries: &[Entry],
    slot: Slot,
    parent_slot: Slot,
    is_full_slot: bool,
    version: u16,
) -> Vec<Shred> {
    Shredder::new(slot, parent_slot, 0, version)
        .unwrap()
        .make_merkle_shreds_from_entries(
            &Keypair::new(),
            entries,
            is_full_slot,
            Hash::new_from_array(rand::rng().random()), // chained_merkle_root
            0,                                          // next_shred_index,
            0,                                          // next_code_index
            &ReedSolomonCache::default(),
            &mut ProcessShredsStats::default(),
        )
        .filter(Shred::is_data)
        .collect()
}

#[cfg(feature = "dev-context-only-utils")]
pub fn make_slot_entries(
    slot: Slot,
    parent_slot: Slot,
    num_entries: u64,
) -> (Vec<Shred>, Vec<Entry>) {
    let entries = create_ticks(num_entries, 1, Hash::new_unique());
    let shreds = entries_to_test_shreds(&entries, slot, parent_slot, true, 0);
    (shreds, entries)
}

#[cfg(feature = "dev-context-only-utils")]
pub fn make_many_slot_entries(
    start_slot: Slot,
    num_slots: u64,
    entries_per_slot: u64,
) -> (Vec<Shred>, Vec<Entry>) {
    let mut shreds = vec![];
    let mut entries = vec![];
    for slot in start_slot..start_slot + num_slots {
        let parent_slot = if slot == 0 { 0 } else { slot - 1 };

        let (slot_shreds, slot_entries) = make_slot_entries(slot, parent_slot, entries_per_slot);
        shreds.extend(slot_shreds);
        entries.extend(slot_entries);
    }

    (shreds, entries)
}

// Create shreds for slots that have a parent-child relationship defined by the input `chain`
// used for tests only
#[cfg(feature = "dev-context-only-utils")]
pub fn make_chaining_slot_entries(
    chain: &[u64],
    entries_per_slot: u64,
    first_parent: u64,
) -> Vec<(Vec<Shred>, Vec<Entry>)> {
    let mut slots_shreds_and_entries = vec![];
    for (i, slot) in chain.iter().enumerate() {
        let parent_slot = {
            if *slot == 0 || i == 0 {
                first_parent
            } else {
                chain[i - 1]
            }
        };

        let result = make_slot_entries(*slot, parent_slot, entries_per_slot);
        slots_shreds_and_entries.push(result);
    }

    slots_shreds_and_entries
}

#[cfg(test)]
pub mod tests;
