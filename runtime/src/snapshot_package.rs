use {
    crate::{
        bank::{Bank, BankFieldsToSerialize, BankHashStats, BankSlotDelta},
        serde_snapshot::BankIncrementalSnapshotPersistence,
        snapshot_hash::SnapshotHash,
    },
    agave_feature_set as feature_set,
    log::*,
    solana_accounts_db::{
        accounts::Accounts,
        accounts_db::AccountStorageEntry,
        accounts_hash::{
            AccountsDeltaHash, AccountsHash, AccountsHashKind, MerkleOrLatticeAccountsHash,
        },
    },
    solana_clock::Slot,
    solana_epoch_schedule::EpochSchedule,
    solana_hash::Hash,
    solana_rent_collector::RentCollector,
    std::{
        sync::{atomic::Ordering, Arc},
        time::Instant,
    },
};

mod compare;
pub use compare::*;

/// This struct packages up fields to send from AccountsBackgroundService to AccountsHashVerifier
pub struct AccountsPackage {
    pub package_kind: AccountsPackageKind,
    pub slot: Slot,
    pub block_height: Slot,
    pub snapshot_storages: Vec<Arc<AccountStorageEntry>>,
    pub expected_capitalization: u64,
    pub accounts: Arc<Accounts>,
    pub epoch_schedule: EpochSchedule,
    pub rent_collector: RentCollector,
    pub accounts_hash_algorithm: AccountsHashAlgorithm,

    /// Supplemental information needed for snapshots
    pub snapshot_info: Option<SupplementalSnapshotInfo>,

    /// The instant this accounts package was send to the queue.
    /// Used to track how long accounts packages wait before processing.
    pub enqueued: Instant,
}

impl AccountsPackage {
    /// Package up bank files, storages, and slot deltas for a snapshot
    pub fn new_for_snapshot(
        package_kind: AccountsPackageKind,
        bank: &Bank,
        snapshot_storages: Vec<Arc<AccountStorageEntry>>,
        status_cache_slot_deltas: Vec<BankSlotDelta>,
    ) -> Self {
        let slot = bank.slot();
        let AccountsPackageKind::Snapshot(snapshot_kind) = package_kind;
        info!(
            "Package snapshot for bank {} has {} account storage entries (snapshot kind: {:?})",
            slot,
            snapshot_storages.len(),
            snapshot_kind,
        );
        if let SnapshotKind::IncrementalSnapshot(incremental_snapshot_base_slot) = snapshot_kind {
            assert!(
                slot > incremental_snapshot_base_slot,
                "Incremental snapshot base slot must be less than the bank being snapshotted!"
            );
        }

        let snapshot_info = {
            let accounts_db = &bank.rc.accounts.accounts_db;
            let write_version = accounts_db.write_version.load(Ordering::Acquire);
            let accounts_delta_hash = if bank
                .feature_set
                .is_active(&feature_set::remove_accounts_delta_hash::id())
            {
                AccountsDeltaHash(Hash::default())
            } else {
                // SAFETY: There *must* be an accounts delta hash for this slot.
                // Since we only snapshot rooted slots, and we know rooted slots must be frozen,
                // that guarantees this slot will have an accounts delta hash.
                accounts_db.get_accounts_delta_hash(slot).unwrap()
            };
            let bank_hash_stats = bank.get_bank_hash_stats();
            let bank_fields_to_serialize = bank.get_fields_to_serialize();
            SupplementalSnapshotInfo {
                status_cache_slot_deltas,
                bank_fields_to_serialize,
                bank_hash_stats,
                accounts_delta_hash,
                write_version,
            }
        };

        let accounts_hash_algorithm = if bank.is_snapshots_lt_hash_enabled() {
            AccountsHashAlgorithm::Lattice
        } else {
            AccountsHashAlgorithm::Merkle
        };
        Self::_new(
            package_kind,
            bank,
            snapshot_storages,
            accounts_hash_algorithm,
            Some(snapshot_info),
        )
    }

    fn _new(
        package_kind: AccountsPackageKind,
        bank: &Bank,
        snapshot_storages: Vec<Arc<AccountStorageEntry>>,
        accounts_hash_algorithm: AccountsHashAlgorithm,
        snapshot_info: Option<SupplementalSnapshotInfo>,
    ) -> Self {
        Self {
            package_kind,
            slot: bank.slot(),
            block_height: bank.block_height(),
            snapshot_storages,
            expected_capitalization: bank.capitalization(),
            accounts: bank.accounts(),
            epoch_schedule: bank.epoch_schedule().clone(),
            rent_collector: bank.rent_collector().clone(),
            accounts_hash_algorithm,
            snapshot_info,
            enqueued: Instant::now(),
        }
    }

    /// Create a new Accounts Package where basically every field is defaulted.
    /// Only use for tests; many of the fields are invalid!
    #[cfg(feature = "dev-context-only-utils")]
    pub fn default_for_tests() -> Self {
        use solana_accounts_db::accounts_db::AccountsDb;
        let accounts_db = AccountsDb::default_for_tests();
        let accounts = Accounts::new(Arc::new(accounts_db));
        Self {
            package_kind: AccountsPackageKind::Snapshot(SnapshotKind::FullSnapshot),
            slot: Slot::default(),
            block_height: Slot::default(),
            snapshot_storages: Vec::default(),
            expected_capitalization: u64::default(),
            accounts: Arc::new(accounts),
            epoch_schedule: EpochSchedule::default(),
            rent_collector: RentCollector::default(),
            accounts_hash_algorithm: AccountsHashAlgorithm::Merkle,
            snapshot_info: Some(SupplementalSnapshotInfo {
                status_cache_slot_deltas: Vec::default(),
                bank_fields_to_serialize: BankFieldsToSerialize::default_for_tests(),
                bank_hash_stats: BankHashStats::default(),
                accounts_delta_hash: AccountsDeltaHash(Hash::default()),
                write_version: u64::default(),
            }),
            enqueued: Instant::now(),
        }
    }
}

impl std::fmt::Debug for AccountsPackage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountsPackage")
            .field("kind", &self.package_kind)
            .field("slot", &self.slot)
            .field("block_height", &self.block_height)
            .field("accounts_hash_algorithm", &self.accounts_hash_algorithm)
            .finish_non_exhaustive()
    }
}

/// Supplemental information needed for snapshots
pub struct SupplementalSnapshotInfo {
    pub status_cache_slot_deltas: Vec<BankSlotDelta>,
    pub bank_fields_to_serialize: BankFieldsToSerialize,
    pub bank_hash_stats: BankHashStats,
    pub accounts_delta_hash: AccountsDeltaHash,
    pub write_version: u64,
}

/// Accounts packages are sent to the Accounts Hash Verifier for processing.  There are multiple
/// types of accounts packages, which are specified as variants in this enum.  All accounts
/// packages do share some processing: such as calculating the accounts hash.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum AccountsPackageKind {
    Snapshot(SnapshotKind),
}

/// This struct packages up fields to send from AccountsHashVerifier to SnapshotPackagerService
pub struct SnapshotPackage {
    pub snapshot_kind: SnapshotKind,
    pub slot: Slot,
    pub block_height: Slot,
    pub hash: SnapshotHash,
    pub snapshot_storages: Vec<Arc<AccountStorageEntry>>,
    pub status_cache_slot_deltas: Vec<BankSlotDelta>,
    pub bank_fields_to_serialize: BankFieldsToSerialize,
    pub bank_hash_stats: BankHashStats,
    pub accounts_delta_hash: AccountsDeltaHash,
    pub accounts_hash: AccountsHash,
    pub write_version: u64,
    pub bank_incremental_snapshot_persistence: Option<BankIncrementalSnapshotPersistence>,

    /// The instant this snapshot package was sent to the queue.
    /// Used to track how long snapshot packages wait before handling.
    pub enqueued: Instant,
}

impl SnapshotPackage {
    pub fn new(
        accounts_package: AccountsPackage,
        merkle_or_lattice_accounts_hash: MerkleOrLatticeAccountsHash,
        bank_incremental_snapshot_persistence: Option<BankIncrementalSnapshotPersistence>,
    ) -> Self {
        let AccountsPackageKind::Snapshot(snapshot_kind) = accounts_package.package_kind;
        let Some(snapshot_info) = accounts_package.snapshot_info else {
            panic!(
                "The AccountsPackage must have snapshot info in order to make a SnapshotPackage!"
            );
        };

        let accounts_hash = match merkle_or_lattice_accounts_hash {
            MerkleOrLatticeAccountsHash::Merkle(accounts_hash_kind) => {
                match accounts_hash_kind {
                    AccountsHashKind::Full(accounts_hash) => accounts_hash,
                    AccountsHashKind::Incremental(_) => {
                        // The accounts hash is only needed when serializing a full snapshot.
                        // When serializing an incremental snapshot, there will not be a full accounts hash
                        // at `slot`.  In that case, use the default, because it doesn't actually get used.
                        // The incremental snapshot will use the BankIncrementalSnapshotPersistence
                        // field, so ensure it is Some.
                        assert!(bank_incremental_snapshot_persistence.is_some());
                        AccountsHash(Hash::default())
                    }
                }
            }
            MerkleOrLatticeAccountsHash::Lattice => {
                // This is the merkle-based accounts hash, which isn't used in the Lattice case,
                // so any value is fine here.
                AccountsHash(Hash::default())
            }
        };

        Self {
            snapshot_kind,
            slot: accounts_package.slot,
            block_height: accounts_package.block_height,
            hash: SnapshotHash::new(
                &merkle_or_lattice_accounts_hash,
                snapshot_info
                    .bank_fields_to_serialize
                    .accounts_lt_hash
                    .as_ref()
                    .map(|accounts_lt_hash| accounts_lt_hash.0.checksum()),
            ),
            snapshot_storages: accounts_package.snapshot_storages,
            status_cache_slot_deltas: snapshot_info.status_cache_slot_deltas,
            bank_fields_to_serialize: snapshot_info.bank_fields_to_serialize,
            accounts_delta_hash: snapshot_info.accounts_delta_hash,
            bank_hash_stats: snapshot_info.bank_hash_stats,
            accounts_hash,
            bank_incremental_snapshot_persistence,
            write_version: snapshot_info.write_version,
            enqueued: Instant::now(),
        }
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl SnapshotPackage {
    /// Create a new SnapshotPackage where basically every field is defaulted.
    /// Only use for tests; many of the fields are invalid!
    pub fn default_for_tests() -> Self {
        Self {
            snapshot_kind: SnapshotKind::FullSnapshot,
            slot: Slot::default(),
            block_height: Slot::default(),
            hash: SnapshotHash(Hash::default()),
            snapshot_storages: Vec::default(),
            status_cache_slot_deltas: Vec::default(),
            bank_fields_to_serialize: BankFieldsToSerialize::default_for_tests(),
            accounts_delta_hash: AccountsDeltaHash(Hash::default()),
            bank_hash_stats: BankHashStats::default(),
            accounts_hash: AccountsHash(Hash::default()),
            bank_incremental_snapshot_persistence: None,
            write_version: u64::default(),
            enqueued: Instant::now(),
        }
    }
}

impl std::fmt::Debug for SnapshotPackage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotPackage")
            .field("kind", &self.snapshot_kind)
            .field("slot", &self.slot)
            .field("block_height", &self.block_height)
            .finish_non_exhaustive()
    }
}

/// Snapshots come in two kinds, Full and Incremental.  The IncrementalSnapshot has a Slot field,
/// which is the incremental snapshot base slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotKind {
    FullSnapshot,
    IncrementalSnapshot(Slot),
}

impl SnapshotKind {
    pub fn is_full_snapshot(&self) -> bool {
        matches!(self, SnapshotKind::FullSnapshot)
    }
    pub fn is_incremental_snapshot(&self) -> bool {
        matches!(self, SnapshotKind::IncrementalSnapshot(_))
    }
}

/// Which algorithm should be used to calculate the accounts hash?
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum AccountsHashAlgorithm {
    /// Merkle-based accounts hash algorithm
    Merkle,
    /// Lattice-based accounts hash algorithm
    Lattice,
}
