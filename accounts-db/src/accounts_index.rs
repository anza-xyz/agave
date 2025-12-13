mod account_map_entry;
mod accounts_index_storage;
mod bucket_map_holder;
pub(crate) mod in_mem_accounts_index;
mod iter;
mod roots_tracker;
mod secondary;
mod stats;
use {
    crate::{
        ancestors::Ancestors, contains::Contains, is_zero_lamport::IsZeroLamport,
        pubkey_bins::PubkeyBinCalculator24, rolling_bit_field::RollingBitField,
    },
    account_map_entry::{AccountMapEntry, PreAllocatedAccountMapEntry, SlotListWriteGuard},
    accounts_index_storage::AccountsIndexStorage,
    bucket_map_holder::Age,
    in_mem_accounts_index::{
        ExistedLocation, InMemAccountsIndex, InsertNewEntryResults, StartupStats,
    },
    iter::{AccountsIndexPubkeyIterOrder, AccountsIndexPubkeyIterator},
    log::*,
    rand::{rng, Rng},
    rayon::iter::{IntoParallelIterator, ParallelIterator},
    roots_tracker::RootsTracker,
    secondary::{RwLockSecondaryIndexEntry, SecondaryIndex, SecondaryIndexEntry},
    smallvec::SmallVec,
    solana_account::ReadableAccount,
    solana_clock::{BankId, Slot},
    solana_measure::measure::Measure,
    solana_pubkey::Pubkey,
    stats::Stats,
    std::{
        collections::{btree_map::BTreeMap, HashSet},
        fmt::Debug,
        num::NonZeroUsize,
        ops::{Bound, Range, RangeBounds},
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
            Arc, Mutex, RwLock,
        },
    },
    thiserror::Error,
};
pub use {
    iter::ITER_BATCH_SIZE,
    secondary::{
        AccountIndex, AccountSecondaryIndexes, AccountSecondaryIndexesIncludeExclude, IndexKey,
    },
};

pub const BINS_DEFAULT: usize = 8192;
pub const BINS_FOR_TESTING: usize = 2; // we want > 1, but each bin is a few disk files with a disk based index, so fewer is better
pub const BINS_FOR_BENCHMARKS: usize = 8192;
// The unsafe is safe because we're using a fixed, known non-zero value
pub const FLUSH_THREADS_TESTING: NonZeroUsize = NonZeroUsize::new(1).unwrap();
pub const ACCOUNTS_INDEX_CONFIG_FOR_TESTING: AccountsIndexConfig = AccountsIndexConfig {
    bins: Some(BINS_FOR_TESTING),
    num_flush_threads: Some(FLUSH_THREADS_TESTING),
    drives: None,
    index_limit: IndexLimit::InMemOnly,
    ages_to_stay_in_cache: None,
    scan_results_limit_bytes: None,
    num_initial_accounts: None,
};
pub const ACCOUNTS_INDEX_CONFIG_FOR_BENCHMARKS: AccountsIndexConfig = AccountsIndexConfig {
    bins: Some(BINS_FOR_BENCHMARKS),
    num_flush_threads: Some(FLUSH_THREADS_TESTING),
    drives: None,
    index_limit: IndexLimit::InMemOnly,
    ages_to_stay_in_cache: None,
    scan_results_limit_bytes: None,
    num_initial_accounts: None,
};
pub type ScanResult<T> = Result<T, ScanError>;
pub type SlotList<T> = SmallVec<[SlotListItem<T>; 1]>;
pub type ReclaimsSlotList<T> = Vec<SlotListItem<T>>;
pub type SlotListItem<T> = (Slot, T);

// The ref count cannot be higher than the total number of storages, and we should never have more
// than 1 million storages. A 32-bit ref count should be *significantly* more than enough.
// (We already effectively limit the number of storages to 2^32 since the storage ID type is a u32.)
// The majority of accounts should only exist in one storage, so the most common ref count is '1'.
// Heavily updated accounts should still have a ref count that is < 100.
pub type RefCount = u32;
pub type AtomicRefCount = AtomicU32;

/// values returned from `insert_new_if_missing_into_primary_index()`
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct InsertNewIfMissingIntoPrimaryIndexInfo {
    /// number of accounts inserted in the index
    pub count: usize,
    /// Number of accounts added to the index that didn't already exist in the index
    pub num_did_not_exist: u64,
    /// Number of accounts added to the index that already existed, and were in-mem
    pub num_existed_in_mem: u64,
    /// Number of accounts added to the index that already existed, and were on-disk
    pub num_existed_on_disk: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
/// which accounts `scan` should load from disk
pub enum ScanFilter {
    /// Scan both in-memory and on-disk index
    #[default]
    All,

    /// abnormal = ref_count != 1 or slot list.len() != 1
    /// Scan only in-memory index and skip on-disk index
    OnlyAbnormal,

    /// Similar to `OnlyAbnormal but also check on-disk index to verify the
    /// entry on-disk is indeed normal.
    OnlyAbnormalWithVerify,

    /// Similar to `OnlyAbnormal but mark entries in memory as not found
    /// if they are normal
    /// This removes the possibility of any race conditions with index
    /// flushing and simulates the system running an uncached disk index
    /// where nothing 'normal' is ever held in the in memory index as far as
    /// callers are concerned. This could also be a  correct/ideal future api
    /// to similarly provide consistency and remove race condition behavior.
    OnlyAbnormalTest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// how accounts index 'upsert' should handle reclaims
pub enum UpsertReclaim {
    /// previous entry for this slot in the index is expected to be cached, so irrelevant to reclaims
    PreviousSlotEntryWasCached,
    /// previous entry for this slot in the index may need to be reclaimed, so return it.
    /// reclaims is the only output of upsert, requiring a synchronous execution
    PopulateReclaims,
    /// overwrite existing data in the same slot and do not return in 'reclaims'
    IgnoreReclaims,
    // Reclaim all older versions of the account from the index and return
    // in the 'reclaims'
    ReclaimOldSlots,
}

#[derive(Debug)]
pub struct ScanConfig {
    /// checked by the scan. When true, abort scan.
    pub abort: Option<Arc<AtomicBool>>,

    /// In what order should items be scanned?
    pub scan_order: ScanOrder,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            abort: None,
            scan_order: ScanOrder::Unsorted,
        }
    }
}

impl ScanConfig {
    pub fn new(scan_order: ScanOrder) -> Self {
        Self {
            scan_order,
            ..Default::default()
        }
    }

    /// mark the scan as aborted
    pub fn abort(&self) {
        if let Some(abort) = self.abort.as_ref() {
            abort.store(true, Ordering::Relaxed)
        }
    }

    /// use existing 'abort' if available, otherwise allocate one
    pub fn recreate_with_abort(&self) -> Self {
        ScanConfig {
            abort: Some(self.abort.clone().unwrap_or_default()),
            scan_order: self.scan_order,
        }
    }

    /// true if scan should abort
    pub fn is_aborted(&self) -> bool {
        if let Some(abort) = self.abort.as_ref() {
            abort.load(Ordering::Relaxed)
        } else {
            false
        }
    }
}

/// In what order should items be scanned?
///
/// Users should prefer `Unsorted`, unless required otherwise,
/// as sorting incurs additional runtime cost.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ScanOrder {
    /// Scan items in any order
    Unsorted,
    /// Scan items in sorted order
    Sorted,
}

pub trait IsCached {
    fn is_cached(&self) -> bool;
}

pub trait IndexValue: 'static + IsCached + IsZeroLamport + DiskIndexValue {}

pub trait DiskIndexValue:
    'static + Clone + Debug + PartialEq + Copy + Default + Sync + Send
{
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum ScanError {
    #[error(
        "Node detected it replayed bad version of slot {slot:?} with id {bank_id:?}, thus the \
         scan on said slot was aborted"
    )]
    SlotRemoved { slot: Slot, bank_id: BankId },
    #[error("scan aborted: {0}")]
    Aborted(String),
}

enum ScanTypes<R: RangeBounds<Pubkey>> {
    Unindexed(Option<R>),
    Indexed(IndexKey),
}

/// specification of how much memory the in-mem portion of account index can use
#[derive(Debug, Copy, Clone)]
pub enum IndexLimit {
    /// use disk index while keeping a minimal amount in-mem
    Minimal,
    /// in-mem-only was specified, no disk index
    InMemOnly,
}

#[derive(Debug, Clone)]
pub struct AccountsIndexConfig {
    pub bins: Option<usize>,
    pub num_flush_threads: Option<NonZeroUsize>,
    pub drives: Option<Vec<PathBuf>>,
    pub index_limit: IndexLimit,
    pub ages_to_stay_in_cache: Option<Age>,
    pub scan_results_limit_bytes: Option<usize>,
    /// Initial number of accounts, used to pre-allocate HashMap capacity at startup.
    pub num_initial_accounts: Option<usize>,
}

impl Default for AccountsIndexConfig {
    fn default() -> Self {
        Self {
            bins: None,
            num_flush_threads: None,
            drives: None,
            index_limit: IndexLimit::InMemOnly,
            ages_to_stay_in_cache: None,
            scan_results_limit_bytes: None,
            num_initial_accounts: None,
        }
    }
}

pub fn default_num_flush_threads() -> NonZeroUsize {
    NonZeroUsize::new(std::cmp::max(2, num_cpus::get() / 4)).expect("non-zero system threads")
}

#[derive(Debug, Default)]
pub struct AccountsIndexRootsStats {
    pub roots_len: Option<usize>,
    pub uncleaned_roots_len: Option<usize>,
    pub roots_range: Option<u64>,
    pub rooted_cleaned_count: usize,
    pub unrooted_cleaned_count: usize,
    pub clean_unref_from_storage_us: u64,
    pub clean_dead_slot_us: u64,
}

#[derive(Copy, Clone)]
pub enum AccountsIndexScanResult {
    /// if the entry is not in the in-memory index, do not add it unless the entry becomes dirty
    OnlyKeepInMemoryIfDirty,
    /// keep the entry in the in-memory index
    KeepInMemory,
    /// reduce refcount by 1
    Unref,
    /// reduce refcount by 1 and assert that ref_count = 0 after unref
    UnrefAssert0,
    /// reduce refcount by 1 and log if ref_count != 0 after unref
    UnrefLog0,
}

#[derive(Debug)]
/// T: account info type to interact in in-memory items
/// U: account info type to be persisted to disk
pub struct AccountsIndex<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> {
    pub account_maps: Box<[Arc<InMemAccountsIndex<T, U>>]>,
    pub bin_calculator: PubkeyBinCalculator24,
    program_id_index: SecondaryIndex<RwLockSecondaryIndexEntry>,
    spl_token_mint_index: SecondaryIndex<RwLockSecondaryIndexEntry>,
    spl_token_owner_index: SecondaryIndex<RwLockSecondaryIndexEntry>,
    pub roots_tracker: RwLock<RootsTracker>,
    ongoing_scan_roots: RwLock<BTreeMap<Slot, u64>>,
    // Each scan has some latest slot `S` that is the tip of the fork the scan
    // is iterating over. The unique id of that slot `S` is recorded here (note we don't use
    // `S` as the id because there can be more than one version of a slot `S`). If a fork
    // is abandoned, all of the slots on that fork up to `S` will be removed via
    // `AccountsDb::remove_unrooted_slots()`. When the scan finishes, it'll realize that the
    // results of the scan may have been corrupted by `remove_unrooted_slots` and abort its results.
    //
    // `removed_bank_ids` tracks all the slot ids that were removed via `remove_unrooted_slots()` so any attempted scans
    // on any of these slots fails. This is safe to purge once the associated Bank is dropped and
    // scanning the fork with that Bank at the tip is no longer possible.
    pub removed_bank_ids: Mutex<HashSet<BankId>>,

    storage: AccountsIndexStorage<T, U>,

    /// when a scan's accumulated data exceeds this limit, abort the scan
    pub scan_results_limit_bytes: Option<usize>,

    pub purge_older_root_entries_one_slot_list: AtomicUsize,

    /// # roots added since last check
    pub roots_added: AtomicUsize,
    /// # roots removed since last check
    pub roots_removed: AtomicUsize,
    /// # scans active currently
    pub active_scans: AtomicUsize,
    /// # of slots between latest max and latest scan
    pub max_distance_to_min_scan_slot: AtomicU64,
}

impl<T: IndexValue, U: DiskIndexValue + From<T> + Into<T>> AccountsIndex<T, U> {
    pub fn default_for_tests() -> Self {
        Self::new(&ACCOUNTS_INDEX_CONFIG_FOR_TESTING, Arc::default())
    }

    pub fn new(config: &AccountsIndexConfig, exit: Arc<AtomicBool>) -> Self {
        let scan_results_limit_bytes = config.scan_results_limit_bytes;
        let (account_maps, bin_calculator, storage) = Self::allocate_accounts_index(config, exit);
        Self {
            purge_older_root_entries_one_slot_list: AtomicUsize::default(),
            account_maps,
            bin_calculator,
            program_id_index: SecondaryIndex::<RwLockSecondaryIndexEntry>::new(
                "program_id_index_stats",
            ),
            spl_token_mint_index: SecondaryIndex::<RwLockSecondaryIndexEntry>::new(
                "spl_token_mint_index_stats",
            ),
            spl_token_owner_index: SecondaryIndex::<RwLockSecondaryIndexEntry>::new(
                "spl_token_owner_index_stats",
            ),
            roots_tracker: RwLock::<RootsTracker>::default(),
            ongoing_scan_roots: RwLock::<BTreeMap<Slot, u64>>::default(),
            removed_bank_ids: Mutex::<HashSet<BankId>>::default(),
            storage,
            scan_results_limit_bytes,
            roots_added: AtomicUsize::default(),
            roots_removed: AtomicUsize::default(),
            active_scans: AtomicUsize::default(),
            max_distance_to_min_scan_slot: AtomicU64::default(),
        }
    }

    /// return the bin index for a given pubkey
    fn bin_from_pubkey(&self, pubkey: &Pubkey) -> usize {
        self.bin_calculator.bin_from_pubkey(pubkey)
    }

    /// returns the start bin and the end bin (inclusive) to scan.
    ///
    /// Note that start_bin maybe larger than highest bin index. Therefore, the
    /// caller should not assume that start_bin is a valid bin index. So don't
    /// index into `account_maps` with start_bin. Use `start_bin..=end_bin` to
    /// iterate over the bins.
    fn bin_start_end_inclusive<R>(&self, range: &R) -> (usize, usize)
    where
        R: RangeBounds<Pubkey>,
    {
        let start_bin = match range.start_bound() {
            Bound::Included(start) => self.bin_from_pubkey(start),
            Bound::Excluded(start) => {
                // check if start == self.account_maps[start_bin].highest_pubkey(), then
                // we should return start_bin + 1
                let start_bin = self.bin_from_pubkey(start);
                if start == &self.account_maps[start_bin].highest_pubkey {
                    start_bin + 1
                } else {
                    start_bin
                }
            }
            Bound::Unbounded => 0,
        };

        let end_bin_inclusive = match range.end_bound() {
            Bound::Included(end) => self.bin_from_pubkey(end),
            Bound::Excluded(end) => {
                // check if end == self.account_maps[end_bin].lowest_pubkey(), then
                // we should return end_bin - 1
                let end_bin = self.bin_from_pubkey(end);
                if end == &self.account_maps[end_bin].lowest_pubkey {
                    end_bin.saturating_sub(1)
                } else {
                    end_bin
                }
            }
            Bound::Unbounded => self.account_maps.len().saturating_sub(1),
        };

        (start_bin, end_bin_inclusive)
    }

    #[allow(clippy::type_complexity)]
    fn allocate_accounts_index(
        config: &AccountsIndexConfig,
        exit: Arc<AtomicBool>,
    ) -> (
        Box<[Arc<InMemAccountsIndex<T, U>>]>,
        PubkeyBinCalculator24,
        AccountsIndexStorage<T, U>,
    ) {
        let bins = config.bins.unwrap_or(BINS_DEFAULT);
        // create bin_calculator early to verify # bins is reasonable
        let bin_calculator = PubkeyBinCalculator24::new(bins);
        let storage = AccountsIndexStorage::new(bins, config, exit);

        let account_maps: Box<_> = (0..bins)
            .map(|bin| Arc::clone(&storage.in_mem[bin]))
            .collect();
        (account_maps, bin_calculator, storage)
    }

    fn iter<'a, R>(
        &'a self,
        range: Option<&'a R>,
        iter_order: AccountsIndexPubkeyIterOrder,
    ) -> AccountsIndexPubkeyIterator<'a, T, U>
    where
        R: RangeBounds<Pubkey>,
    {
        AccountsIndexPubkeyIterator::new(self, range, iter_order)
    }

    /// is the accounts index using disk as a backing store
    pub fn is_disk_index_enabled(&self) -> bool {
        self.storage.storage.is_disk_index_enabled()
    }

    fn min_ongoing_scan_root_from_btree(ongoing_scan_roots: &BTreeMap<Slot, u64>) -> Option<Slot> {
        ongoing_scan_roots.keys().next().cloned()
    }

    fn do_checked_scan_accounts<F, R>(
        &self,
        metric_name: &'static str,
        ancestors: &Ancestors,
        scan_bank_id: BankId,
        func: F,
        scan_type: ScanTypes<R>,
        config: &ScanConfig,
    ) -> Result<(), ScanError>
    where
        F: FnMut(&Pubkey, (&T, Slot)),
        R: RangeBounds<Pubkey> + std::fmt::Debug,
    {
        {
            let locked_removed_bank_ids = self.removed_bank_ids.lock().unwrap();
            if locked_removed_bank_ids.contains(&scan_bank_id) {
                return Err(ScanError::SlotRemoved {
                    slot: ancestors.max_slot(),
                    bank_id: scan_bank_id,
                });
            }
        }

        self.active_scans.fetch_add(1, Ordering::Relaxed);
        let max_root = {
            let mut w_ongoing_scan_roots = self
                // This lock is also grabbed by clean_accounts(), so clean
                // has at most cleaned up to the current `max_root` (since
                // clean only happens *after* BankForks::set_root() which sets
                // the `max_root`)
                .ongoing_scan_roots
                .write()
                .unwrap();
            // `max_root()` grabs a lock while
            // the `ongoing_scan_roots` lock is held,
            // make sure inverse doesn't happen to avoid
            // deadlock
            let max_root_inclusive = self.max_root_inclusive();
            if let Some(min_ongoing_scan_root) =
                Self::min_ongoing_scan_root_from_btree(&w_ongoing_scan_roots)
            {
                if min_ongoing_scan_root < max_root_inclusive {
                    let current = max_root_inclusive - min_ongoing_scan_root;
                    self.max_distance_to_min_scan_slot
                        .fetch_max(current, Ordering::Relaxed);
                }
            }
            *w_ongoing_scan_roots.entry(max_root_inclusive).or_default() += 1;

            max_root_inclusive
        };

        // First we show that for any bank `B` that is a descendant of
        // the current `max_root`, it must be true that and `B.ancestors.contains(max_root)`,
        // regardless of the pattern of `squash()` behavior, where `ancestors` is the set
        // of ancestors that is tracked in each bank.
        //
        // Proof: At startup, if starting from a snapshot, generate_index() adds all banks
        // in the snapshot to the index via `add_root()` and so `max_root` will be the
        // greatest of these. Thus, so the claim holds at startup since there are no
        // descendants of `max_root`.
        //
        // Now we proceed by induction on each `BankForks::set_root()`.
        // Assume the claim holds when the `max_root` is `R`. Call the set of
        // descendants of `R` present in BankForks `R_descendants`.
        //
        // Then for any banks `B` in `R_descendants`, it must be that `B.ancestors.contains(S)`,
        // where `S` is any ancestor of `B` such that `S >= R`.
        //
        // For example:
        //          `R` -> `A` -> `C` -> `B`
        // Then `B.ancestors == {R, A, C}`
        //
        // Next we call `BankForks::set_root()` at some descendant of `R`, `R_new`,
        // where `R_new > R`.
        //
        // When we squash `R_new`, `max_root` in the AccountsIndex here is now set to `R_new`,
        // and all nondescendants of `R_new` are pruned.
        //
        // Now consider any outstanding references to banks in the system that are descended from
        // `max_root == R_new`. Take any one of these references and call it `B`. Because `B` is
        // a descendant of `R_new`, this means `B` was also a descendant of `R`. Thus `B`
        // must be a member of `R_descendants` because `B` was constructed and added to
        // BankForks before the `set_root`.
        //
        // This means by the guarantees of `R_descendants` described above, because
        // `R_new` is an ancestor of `B`, and `R < R_new < B`, then `B.ancestors.contains(R_new)`.
        //
        // Now until the next `set_root`, any new banks constructed from `new_from_parent` will
        // also have `max_root == R_new` in their ancestor set, so the claim holds for those descendants
        // as well. Once the next `set_root` happens, we once again update `max_root` and the same
        // inductive argument can be applied again to show the claim holds.

        // Check that the `max_root` is present in `ancestors`. From the proof above, if
        // `max_root` is not present in `ancestors`, this means the bank `B` with the
        // given `ancestors` is not descended from `max_root, which means
        // either:
        // 1) `B` is on a different fork or
        // 2) `B` is an ancestor of `max_root`.
        // In both cases we can ignore the given ancestors and instead just rely on the roots
        // present as `max_root` indicates the roots present in the index are more up to date
        // than the ancestors given.
        let empty = Ancestors::default();
        let ancestors = if ancestors.contains_key(&max_root) {
            ancestors
        } else {
            /*
            This takes of edge cases like:

            Diagram 1:

                        slot 0
                          |
                        slot 1
                      /        \
                 slot 2         |
                    |       slot 3 (max root)
            slot 4 (scan)

            By the time the scan on slot 4 is called, slot 2 may already have been
            cleaned by a clean on slot 3, but slot 4 may not have been cleaned.
            The state in slot 2 would have been purged and is not saved in any roots.
            In this case, a scan on slot 4 wouldn't accurately reflect the state when bank 4
            was frozen. In cases like this, we default to a scan on the latest roots by
            removing all `ancestors`.
            */
            &empty
        };

        /*
        Now there are two cases, either `ancestors` is empty or nonempty:

        1) If ancestors is empty, then this is the same as a scan on a rooted bank,
        and `ongoing_scan_roots` provides protection against cleanup of roots necessary
        for the scan, and  passing `Some(max_root)` to `do_scan_accounts()` ensures newer
        roots don't appear in the scan.

        2) If ancestors is non-empty, then from the `ancestors_contains(&max_root)` above, we know
        that the fork structure must look something like:

        Diagram 2:

                Build fork structure:
                        slot 0
                          |
                    slot 1 (max_root)
                    /            \
             slot 2              |
                |            slot 3 (potential newer max root)
              slot 4
                |
             slot 5 (scan)

        Consider both types of ancestors, ancestor <= `max_root` and
        ancestor > `max_root`, where `max_root == 1` as illustrated above.

        a) The set of `ancestors <= max_root` are all rooted, which means their state
        is protected by the same guarantees as 1).

        b) As for the `ancestors > max_root`, those banks have at least one reference discoverable
        through the chain of `Bank::BankRc::parent` starting from the calling bank. For instance
        bank 5's parent reference keeps bank 4 alive, which will prevent the `Bank::drop()` from
        running and cleaning up bank 4. Furthermore, no cleans can happen past the saved max_root == 1,
        so a potential newer max root at 3 will not clean up any of the ancestors > 1, so slot 4
        will not be cleaned in the middle of the scan either. (NOTE similar reasoning is employed for
        assert!() justification in AccountsDb::retry_to_get_account_accessor)
        */
        match scan_type {
            ScanTypes::Unindexed(range) => {
                // Pass "" not to log metrics, so RPC doesn't get spammy
                self.do_scan_accounts(metric_name, ancestors, func, range, Some(max_root), config);
            }
            ScanTypes::Indexed(IndexKey::ProgramId(program_id)) => {
                self.do_scan_secondary_index(
                    ancestors,
                    func,
                    &self.program_id_index,
                    &program_id,
                    Some(max_root),
                    config,
                );
            }
            ScanTypes::Indexed(IndexKey::SplTokenMint(mint_key)) => {
                self.do_scan_secondary_index(
                    ancestors,
                    func,
                    &self.spl_token_mint_index,
                    &mint_key,
                    Some(max_root),
                    config,
                );
            }
            ScanTypes::Indexed(IndexKey::SplTokenOwner(owner_key)) => {
                self.do_scan_secondary_index(
                    ancestors,
                    func,
                    &self.spl_token_owner_index,
                    &owner_key,
                    Some(max_root),
                    config,
                );
            }
        }

        {
            self.active_scans.fetch_sub(1, Ordering::Relaxed);
            let mut ongoing_scan_roots = self.ongoing_scan_roots.write().unwrap();
            let count = ongoing_scan_roots.get_mut(&max_root).unwrap();
            *count -= 1;
            if *count == 0 {
                ongoing_scan_roots.remove(&max_root);
            }
        }

        // If the fork with tip at bank `scan_bank_id` was removed during our scan, then the scan
        // may have been corrupted, so abort the results.
        let was_scan_corrupted = self
            .removed_bank_ids
            .lock()
            .unwrap()
            .contains(&scan_bank_id);

        if was_scan_corrupted {
            Err(ScanError::SlotRemoved {
                slot: ancestors.max_slot(),
                bank_id: scan_bank_id,
            })
        } else {
            Ok(())
        }
    }

    // Scan accounts and return latest version of each account that is either:
    // 1) rooted or
    // 2) present in ancestors
    fn do_scan_accounts<F, R>(
        &self,
        metric_name: &'static str,
        ancestors: &Ancestors,
        mut func: F,
        range: Option<R>,
        max_root: Option<Slot>,
        config: &ScanConfig,
    ) where
        F: FnMut(&Pubkey, (&T, Slot)),
        R: RangeBounds<Pubkey> + std::fmt::Debug,
    {
        let returns_items = match config.scan_order {
            ScanOrder::Unsorted => AccountsIndexPubkeyIterOrder::Unsorted,
            ScanOrder::Sorted => AccountsIndexPubkeyIterOrder::Sorted,
        };

        // TODO: expand to use mint index to find the `pubkey_list` below more efficiently
        // instead of scanning the entire range
        let mut total_elapsed_timer = Measure::start("total");
        let mut num_keys_iterated = 0;
        let mut latest_slot_elapsed = 0;
        let mut load_account_elapsed = 0;
        let mut read_lock_elapsed = 0;
        let mut iterator_elapsed = 0;
        let mut iterator_timer = Measure::start("iterator_elapsed");

        for pubkeys in self.iter(range.as_ref(), returns_items) {
            iterator_timer.stop();
            iterator_elapsed += iterator_timer.as_us();
            for pubkey in pubkeys {
                num_keys_iterated += 1;
                self.get_and_then(&pubkey, |entry| {
                    if let Some(list) = entry {
                        let mut read_lock_timer = Measure::start("read_lock");
                        let list_r = &list.slot_list_read_lock();
                        read_lock_timer.stop();
                        read_lock_elapsed += read_lock_timer.as_us();
                        let mut latest_slot_timer = Measure::start("latest_slot");
                        if let Some(index) = self.latest_slot(Some(ancestors), list_r, max_root) {
                            latest_slot_timer.stop();
                            latest_slot_elapsed += latest_slot_timer.as_us();
                            let mut load_account_timer = Measure::start("load_account");
                            func(&pubkey, (&list_r[index].1, list_r[index].0));
                            load_account_timer.stop();
                            load_account_elapsed += load_account_timer.as_us();
                        }
                    }
                    let add_to_in_mem_cache = false;
                    (add_to_in_mem_cache, ())
                });
                if config.is_aborted() {
                    return;
                }
            }
            iterator_timer = Measure::start("iterator_elapsed");
        }

        total_elapsed_timer.stop();
        if !metric_name.is_empty() {
            datapoint_info!(
                metric_name,
                ("total_elapsed", total_elapsed_timer.as_us(), i64),
                ("latest_slot_elapsed", latest_slot_elapsed, i64),
                ("read_lock_elapsed", read_lock_elapsed, i64),
                ("load_account_elapsed", load_account_elapsed, i64),
                ("iterator_elapsed", iterator_elapsed, i64),
                ("num_keys_iterated", num_keys_iterated, i64),
            )
        }
    }

    fn do_scan_secondary_index<
        F,
        SecondaryIndexEntryType: SecondaryIndexEntry + Default + Sync + Send,
    >(
        &self,
        ancestors: &Ancestors,
        mut func: F,
        index: &SecondaryIndex<SecondaryIndexEntryType>,
        index_key: &Pubkey,
        max_root: Option<Slot>,
        config: &ScanConfig,
    ) where
        F: FnMut(&Pubkey, (&T, Slot)),
    {
        for pubkey in index.get(index_key) {
            if config.is_aborted() {
                break;
            }
            self.get_with_and_then(
                &pubkey,
                Some(ancestors),
                max_root,
                true,
                |(slot, account_info)| func(&pubkey, (&account_info, slot)),
            );
        }
    }

    /// Gets the index's entry for `pubkey` and applies `callback` to it
    ///
    /// If `callback`'s boolean return value is true, add this entry to the in-mem cache.
    pub fn get_and_then<R>(
        &self,
        pubkey: &Pubkey,
        callback: impl FnOnce(Option<&AccountMapEntry<T>>) -> (bool, R),
    ) -> R {
        self.get_bin(pubkey).get_internal_inner(pubkey, callback)
    }

    /// Gets the index's entry for `pubkey`, with `ancestors` and `max_root`,
    /// and applies `callback` to it
    pub(crate) fn get_with_and_then<R>(
        &self,
        pubkey: &Pubkey,
        ancestors: Option<&Ancestors>,
        max_root: Option<Slot>,
        should_add_to_in_mem_cache: bool,
        callback: impl FnOnce(SlotListItem<T>) -> R,
    ) -> Option<R> {
        self.get_and_then(pubkey, |entry| {
            let callback_result = entry.and_then(|entry| {
                self.get_account_info_with_and_then(entry, ancestors, max_root, callback)
            });
            (should_add_to_in_mem_cache, callback_result)
        })
    }

    /// Gets the account info (and slot) in `entry`, with `ancestors` and `max_root`,
    /// and applies `callback` to it
    pub(crate) fn get_account_info_with_and_then<R>(
        &self,
        entry: &AccountMapEntry<T>,
        ancestors: Option<&Ancestors>,
        max_root: Option<Slot>,
        callback: impl FnOnce(SlotListItem<T>) -> R,
    ) -> Option<R> {
        let slot_list = entry.slot_list_read_lock();
        self.latest_slot(ancestors, &slot_list, max_root)
            .map(|found_index| callback(slot_list[found_index]))
    }

    /// Is `pubkey` in the index?
    pub fn contains(&self, pubkey: &Pubkey) -> bool {
        self.get_and_then(pubkey, |entry| (false, entry.is_some()))
    }

    /// Is `pubkey`, with `ancestors` and `max_root`, in the index?
    #[cfg(test)]
    pub(crate) fn contains_with(
        &self,
        pubkey: &Pubkey,
        ancestors: Option<&Ancestors>,
        max_root: Option<Slot>,
    ) -> bool {
        self.get_with_and_then(pubkey, ancestors, max_root, false, |_| ())
            .is_some()
    }

    fn slot_list_mut<RT>(
        &self,
        pubkey: &Pubkey,
        user_fn: impl FnOnce(SlotListWriteGuard<T>) -> RT,
    ) -> Option<RT> {
        let read_lock = self.get_bin(pubkey);
        read_lock.slot_list_mut(pubkey, user_fn)
    }

    /// Remove keys from the account index if the key's slot list is empty.
    /// Returns the keys that were removed from the index. These keys should not be accessed again in the current code path.
    #[must_use]
    pub fn handle_dead_keys(
        &self,
        dead_keys: &[Pubkey],
        account_indexes: &AccountSecondaryIndexes,
    ) -> HashSet<Pubkey> {
        let mut pubkeys_removed_from_accounts_index = HashSet::default();
        if !dead_keys.is_empty() {
            for key in dead_keys.iter() {
                let w_index = self.get_bin(key);
                if w_index.remove_if_slot_list_empty(*key) {
                    pubkeys_removed_from_accounts_index.insert(*key);
                    // Note it's only safe to remove all the entries for this key
                    // because we have the lock for this key's entry in the AccountsIndex,
                    // so no other thread is also updating the index
                    self.purge_secondary_indexes_by_inner_key(key, account_indexes);
                }
            }
        }
        pubkeys_removed_from_accounts_index
    }

    /// call func with every pubkey and index visible from a given set of ancestors
    pub(crate) fn scan_accounts<F>(
        &self,
        ancestors: &Ancestors,
        scan_bank_id: BankId,
        func: F,
        config: &ScanConfig,
    ) -> Result<(), ScanError>
    where
        F: FnMut(&Pubkey, (&T, Slot)),
    {
        // Pass "" not to log metrics, so RPC doesn't get spammy
        self.do_checked_scan_accounts(
            "",
            ancestors,
            scan_bank_id,
            func,
            ScanTypes::Unindexed(None::<Range<Pubkey>>),
            config,
        )
    }

    /// call func with every pubkey and index visible from a given set of ancestors
    pub(crate) fn index_scan_accounts<F>(
        &self,
        ancestors: &Ancestors,
        scan_bank_id: BankId,
        index_key: IndexKey,
        func: F,
        config: &ScanConfig,
    ) -> Result<(), ScanError>
    where
        F: FnMut(&Pubkey, (&T, Slot)),
    {
        // Pass "" not to log metrics, so RPC doesn't get spammy
        self.do_checked_scan_accounts(
            "",
            ancestors,
            scan_bank_id,
            func,
            ScanTypes::<Range<Pubkey>>::Indexed(index_key),
            config,
        )
    }

    pub fn get_rooted_entries(
        &self,
        slot_list: &[SlotListItem<T>],
        max_inclusive: Option<Slot>,
    ) -> SlotList<T> {
        let max_inclusive = max_inclusive.unwrap_or(Slot::MAX);
        let lock = &self.roots_tracker.read().unwrap().alive_roots;
        slot_list
            .iter()
            .filter(|(slot, _)| *slot <= max_inclusive && lock.contains(slot))
            .cloned()
            .collect()
    }

    /// returns true if, after this fn call:
    /// accounts index entry for `pubkey` has an empty slot list
    /// or `pubkey` does not exist in accounts index
    pub(crate) fn purge_exact(
        &self,
        pubkey: &Pubkey,
        slots_to_purge: impl for<'a> Contains<'a, Slot>,
        reclaims: &mut ReclaimsSlotList<T>,
    ) -> bool {
        self.slot_list_mut(pubkey, |mut slot_list| {
            slot_list.retain_and_count(|(slot, item)| {
                let should_purge = slots_to_purge.contains(slot);
                if should_purge {
                    reclaims.push((*slot, *item));
                    false
                } else {
                    true
                }
            }) == 0
        })
        .unwrap_or(true)
    }

    pub fn min_ongoing_scan_root(&self) -> Option<Slot> {
        Self::min_ongoing_scan_root_from_btree(&self.ongoing_scan_roots.read().unwrap())
    }

    // Given a SlotList `L`, a list of ancestors and a maximum slot, find the latest element
    // in `L`, where the slot `S` is an ancestor or root, and if `S` is a root, then `S <= max_root`
    pub(crate) fn latest_slot(
        &self,
        ancestors: Option<&Ancestors>,
        slot_list: &[SlotListItem<T>],
        max_root_inclusive: Option<Slot>,
    ) -> Option<usize> {
        let mut current_max = 0;
        let mut rv = None;
        if let Some(ancestors) = ancestors {
            if !ancestors.is_empty() {
                for (i, (slot, _t)) in slot_list.iter().rev().enumerate() {
                    if (rv.is_none() || *slot > current_max) && ancestors.contains_key(slot) {
                        rv = Some(i);
                        current_max = *slot;
                    }
                }
            }
        }

        let max_root_inclusive = max_root_inclusive.unwrap_or(Slot::MAX);
        let mut tracker = None;

        for (i, (slot, _t)) in slot_list.iter().rev().enumerate() {
            if (rv.is_none() || *slot > current_max) && *slot <= max_root_inclusive {
                let lock = match tracker {
                    Some(inner) => inner,
                    None => self.roots_tracker.read().unwrap(),
                };
                if lock.alive_roots.contains(slot) {
                    rv = Some(i);
                    current_max = *slot;
                }
                tracker = Some(lock);
            }
        }

        rv.map(|index| slot_list.len() - 1 - index)
    }

    pub(crate) fn stats(&self) -> &Stats {
        &self.storage.storage.stats
    }

    /// get stats related to startup
    pub(crate) fn get_startup_stats(&self) -> &StartupStats {
        &self.storage.storage.startup_stats
    }

    pub(crate) fn set_startup(&self, value: Startup) {
        self.storage.set_startup(value);
    }

    pub fn get_startup_remaining_items_to_flush_estimate(&self) -> usize {
        self.storage.get_startup_remaining_items_to_flush_estimate()
    }

    /// Scan AccountsIndex for a given iterator of Pubkeys.
    ///
    /// This fn takes 4 arguments.
    ///  - an iterator of pubkeys to scan
    ///  - callback fn to run for each pubkey in the accounts index
    ///  - avoid_callback_result. If it is Some(default), then callback is ignored and
    ///    default is returned instead.
    ///  - provide_entry_in_callback. If true, populate the ref of the Arc of the
    ///    index entry to `callback` fn. Otherwise, provide None.
    ///
    /// The `callback` fn must return `AccountsIndexScanResult`, which is
    /// used to indicates whether the AccountIndex Entry should be added to
    /// in-memory cache. The `callback` fn takes in 3 arguments:
    ///   - the first an immutable ref of the pubkey,
    ///   - the second an option of the SlotList and RefCount
    ///   - the third an option of the AccountMapEntry, which is only populated
    ///     when `provide_entry_in_callback` is true. Otherwise, it will be
    ///     None.
    pub(crate) fn scan<'a, F, I>(
        &self,
        pubkeys: I,
        mut callback: F,
        avoid_callback_result: Option<AccountsIndexScanResult>,
        filter: ScanFilter,
    ) where
        F: FnMut(&'a Pubkey, Option<(&[SlotListItem<T>], RefCount)>) -> AccountsIndexScanResult,
        I: Iterator<Item = &'a Pubkey>,
    {
        let mut lock = None;
        let mut last_bin = self.bins(); // too big, won't match
        pubkeys.into_iter().for_each(|pubkey| {
            let bin = self.bin_calculator.bin_from_pubkey(pubkey);
            if bin != last_bin {
                // cannot reuse lock since next pubkey is in a different bin than previous one
                lock = Some(&self.account_maps[bin]);
                last_bin = bin;
            }

            let mut internal_callback = |entry: Option<&AccountMapEntry<T>>| {
                let mut cache = false;
                match entry {
                    Some(locked_entry) => {
                        let result = if let Some(result) = avoid_callback_result.as_ref() {
                            *result
                        } else {
                            let slot_list = locked_entry.slot_list_read_lock();
                            callback(pubkey, Some((slot_list.as_ref(), locked_entry.ref_count())))
                        };
                        cache = match result {
                            AccountsIndexScanResult::Unref => {
                                locked_entry.unref();
                                true
                            }
                            AccountsIndexScanResult::UnrefAssert0 => {
                                assert_eq!(
                                    locked_entry.unref(),
                                    1,
                                    "ref count expected to be zero, but is {}! {pubkey}, {:?}",
                                    locked_entry.ref_count(),
                                    locked_entry.slot_list_read_lock(),
                                );
                                true
                            }
                            AccountsIndexScanResult::UnrefLog0 => {
                                let old_ref = locked_entry.unref();
                                if old_ref != 1 {
                                    info!(
                                        "Unexpected unref {pubkey} with {old_ref} {:?}, expect \
                                         old_ref to be 1",
                                        locked_entry.slot_list_read_lock()
                                    );
                                    datapoint_warn!(
                                        "accounts_db-unexpected-unref-zero",
                                        ("old_ref", old_ref, i64),
                                        ("pubkey", pubkey.to_string(), String),
                                    );
                                }
                                true
                            }
                            AccountsIndexScanResult::KeepInMemory => true,
                            AccountsIndexScanResult::OnlyKeepInMemoryIfDirty => false,
                        };
                    }
                    None => {
                        avoid_callback_result.unwrap_or_else(|| callback(pubkey, None));
                    }
                }
                (cache, ())
            };

            match filter {
                ScanFilter::All => {
                    // SAFETY: The caller must ensure that if `provide_entry_in_callback` is true, and
                    // if it's possible for `callback` to clone the entry Arc, then it must also add
                    // the entry to the in-mem cache if the entry is made dirty.
                    lock.as_ref()
                        .unwrap()
                        .get_internal_inner(pubkey, internal_callback);
                }
                ScanFilter::OnlyAbnormal
                | ScanFilter::OnlyAbnormalWithVerify
                | ScanFilter::OnlyAbnormalTest => {
                    let found =
                        lock.as_ref()
                            .unwrap()
                            .get_only_in_mem(pubkey, false, |mut entry| {
                                if entry.is_some() && matches!(filter, ScanFilter::OnlyAbnormalTest)
                                {
                                    let local_entry = entry.unwrap();
                                    if local_entry.ref_count() == 1
                                        && local_entry.slot_list_lock_read_len() == 1
                                    {
                                        // Account was found in memory, but is a single ref single slot account
                                        // For testing purposes, return None as this can be treated like
                                        // a normal account that was flushed to storage.
                                        entry = None;
                                    }
                                }
                                internal_callback(entry);
                                entry.is_some()
                            });
                    if !found && matches!(filter, ScanFilter::OnlyAbnormalWithVerify) {
                        lock.as_ref().unwrap().get_internal_inner(pubkey, |entry| {
                            assert!(entry.is_some(), "{pubkey}, entry: {entry:?}");
                            let entry = entry.unwrap();
                            assert_eq!(entry.ref_count(), 1, "{pubkey}");
                            assert_eq!(entry.slot_list_lock_read_len(), 1, "{pubkey}");
                            (false, ())
                        });
                    }
                }
            }
        });
    }

    // Get the maximum root <= `max_allowed_root` from the given `slot_list`
    fn get_newest_root_in_slot_list(
        alive_roots: &RollingBitField,
        slot_list: &[SlotListItem<T>],
        max_allowed_root_inclusive: Option<Slot>,
    ) -> Slot {
        slot_list
            .iter()
            .map(|(slot, _)| slot)
            .filter(|slot| max_allowed_root_inclusive.is_none_or(|max_root| **slot <= max_root))
            .filter(|slot| alive_roots.contains(slot))
            .max()
            .copied()
            .unwrap_or(0)
    }

    fn update_spl_token_secondary_indexes<G: spl_generic_token::token::GenericTokenAccount>(
        &self,
        token_id: &Pubkey,
        pubkey: &Pubkey,
        account_owner: &Pubkey,
        account_data: &[u8],
        account_indexes: &AccountSecondaryIndexes,
    ) {
        if *account_owner == *token_id {
            if account_indexes.contains(&AccountIndex::SplTokenOwner) {
                if let Some(owner_key) = G::unpack_account_owner(account_data) {
                    if account_indexes.include_key(owner_key) {
                        self.spl_token_owner_index.insert(owner_key, pubkey);
                    }
                }
            }

            if account_indexes.contains(&AccountIndex::SplTokenMint) {
                if let Some(mint_key) = G::unpack_account_mint(account_data) {
                    if account_indexes.include_key(mint_key) {
                        self.spl_token_mint_index.insert(mint_key, pubkey);
                    }
                }
            }
        }
    }

    pub fn get_index_key_size(&self, index: &AccountIndex, index_key: &Pubkey) -> Option<usize> {
        match index {
            AccountIndex::ProgramId => self.program_id_index.index.get(index_key).map(|x| x.len()),
            AccountIndex::SplTokenOwner => self
                .spl_token_owner_index
                .index
                .get(index_key)
                .map(|x| x.len()),
            AccountIndex::SplTokenMint => self
                .spl_token_mint_index
                .index
                .get(index_key)
                .map(|x| x.len()),
        }
    }

    /// log any secondary index counts, if non-zero
    pub(crate) fn log_secondary_indexes(&self) {
        if !self.program_id_index.index.is_empty() {
            info!("secondary index: {:?}", AccountIndex::ProgramId);
            self.program_id_index.log_contents();
        }
        if !self.spl_token_mint_index.index.is_empty() {
            info!("secondary index: {:?}", AccountIndex::SplTokenMint);
            self.spl_token_mint_index.log_contents();
        }
        if !self.spl_token_owner_index.index.is_empty() {
            info!("secondary index: {:?}", AccountIndex::SplTokenOwner);
            self.spl_token_owner_index.log_contents();
        }
    }

    pub(crate) fn update_secondary_indexes(
        &self,
        pubkey: &Pubkey,
        account: &impl ReadableAccount,
        account_indexes: &AccountSecondaryIndexes,
    ) {
        if account_indexes.is_empty() {
            return;
        }

        let account_owner = account.owner();
        let account_data = account.data();

        if account_indexes.contains(&AccountIndex::ProgramId)
            && account_indexes.include_key(account_owner)
        {
            self.program_id_index.insert(account_owner, pubkey);
        }
        // Note because of the below check below on the account data length, when an
        // account hits zero lamports and is reset to AccountSharedData::Default, then we skip
        // the below updates to the secondary indexes.
        //
        // Skipping means not updating secondary index to mark the account as missing.
        // This doesn't introduce false positives during a scan because the caller to scan
        // provides the ancestors to check. So even if a zero-lamport account is not yet
        // removed from the secondary index, the scan function will:
        // 1) consult the primary index via `get(&pubkey, Some(ancestors), max_root)`
        // and find the zero-lamport version
        // 2) When the fetch from storage occurs, it will return AccountSharedData::Default
        // (as persisted tombstone for snapshots). This will then ultimately be
        // filtered out by post-scan filters, like in `get_filtered_spl_token_accounts_by_owner()`.

        self.update_spl_token_secondary_indexes::<spl_generic_token::token::Account>(
            &spl_generic_token::token::id(),
            pubkey,
            account_owner,
            account_data,
            account_indexes,
        );
        self.update_spl_token_secondary_indexes::<spl_generic_token::token_2022::Account>(
            &spl_generic_token::token_2022::id(),
            pubkey,
            account_owner,
            account_data,
            account_indexes,
        );
    }

    pub(crate) fn get_bin(&self, pubkey: &Pubkey) -> &InMemAccountsIndex<T, U> {
        &self.account_maps[self.bin_calculator.bin_from_pubkey(pubkey)]
    }

    pub fn bins(&self) -> usize {
        self.account_maps.len()
    }

    // Same functionally to upsert, but:
    // 1. operates on a batch of items
    // 2. holds the write lock for the duration of adding the items
    // Can save time when inserting lots of new keys.
    // But, does NOT update secondary index
    // This is designed to be called at startup time.
    // returns (insertion_time_us, InsertNewIfMissingIntoPrimaryIndexInfo)
    pub(crate) fn insert_new_if_missing_into_primary_index(
        &self,
        slot: Slot,
        mut items: Vec<(Pubkey, T)>,
    ) -> (u64, InsertNewIfMissingIntoPrimaryIndexInfo) {
        let mut insert_time = Measure::start("insert_into_primary_index");

        let use_disk = self.storage.storage.is_disk_index_enabled();

        let mut count = 0;

        // accumulated stats after inserting pubkeys into the index
        let mut num_did_not_exist = 0;
        let mut num_existed_in_mem = 0;
        let mut num_existed_on_disk = 0;

        // offset bin processing in the 'binned' array by a random amount.
        // This results in calls to insert_new_entry_if_missing_with_lock from different threads starting at different bins to avoid
        // lock contention.
        let bins = self.bins();
        let random_bin_offset = rng().random_range(0..bins);
        let bin_calc = self.bin_calculator;
        items.sort_unstable_by(|(pubkey_a, _), (pubkey_b, _)| {
            ((bin_calc.bin_from_pubkey(pubkey_a) + random_bin_offset) % bins)
                .cmp(&((bin_calc.bin_from_pubkey(pubkey_b) + random_bin_offset) % bins))
                .then_with(|| pubkey_a.cmp(pubkey_b))
        });

        let storage = self.storage.storage.as_ref();
        while !items.is_empty() {
            let mut start_index = items.len() - 1;
            let mut last_pubkey = &items[start_index].0;
            let pubkey_bin = bin_calc.bin_from_pubkey(last_pubkey);
            // Find the smallest index with the same pubkey bin
            while start_index > 0 {
                let next = start_index - 1;
                let next_pubkey = &items[next].0;
                assert_ne!(
                    next_pubkey, last_pubkey,
                    "Accounts may only be stored once per slot: {slot}"
                );
                if bin_calc.bin_from_pubkey(next_pubkey) != pubkey_bin {
                    break;
                }
                start_index = next;
                last_pubkey = next_pubkey;
            }

            let r_account_maps = self.account_maps[pubkey_bin].as_ref();
            // count only considers non-duplicate accounts
            count += items.len() - start_index;

            let items = items.drain(start_index..);
            if use_disk {
                r_account_maps.startup_insert_only(slot, items);
            } else {
                // not using disk buckets, so just write to in-mem
                // this is no longer the default case
                let mut duplicates_from_in_memory = vec![];
                items.for_each(|(pubkey, account_info)| {
                    let new_entry =
                        PreAllocatedAccountMapEntry::new(slot, account_info, storage, use_disk);
                    match r_account_maps.insert_new_entry_if_missing_with_lock(pubkey, new_entry) {
                        InsertNewEntryResults::DidNotExist => {
                            num_did_not_exist += 1;
                        }
                        InsertNewEntryResults::Existed {
                            other_slot,
                            location,
                        } => {
                            if let Some(other_slot) = other_slot {
                                duplicates_from_in_memory.push((other_slot, pubkey));
                            }
                            duplicates_from_in_memory.push((slot, pubkey));

                            match location {
                                ExistedLocation::InMem => {
                                    num_existed_in_mem += 1;
                                }
                                ExistedLocation::OnDisk => {
                                    num_existed_on_disk += 1;
                                }
                            }
                        }
                    }
                });

                r_account_maps
                    .startup_update_duplicates_from_in_memory_only(duplicates_from_in_memory);
            }
        }
        insert_time.stop();

        (
            insert_time.as_us(),
            InsertNewIfMissingIntoPrimaryIndexInfo {
                count,
                num_did_not_exist,
                num_existed_in_mem,
                num_existed_on_disk,
            },
        )
    }

    /// use Vec<> because the internal vecs are already allocated per bin
    pub(crate) fn populate_and_retrieve_duplicate_keys_from_startup(
        &self,
        f: impl Fn(Vec<(Slot, Pubkey)>) + Sync + Send,
    ) {
        (0..self.bins())
            .into_par_iter()
            .map(|pubkey_bin| {
                let r_account_maps = &self.account_maps[pubkey_bin];
                if self.storage.storage.is_disk_index_enabled() {
                    r_account_maps.populate_and_retrieve_duplicate_keys_from_startup()
                } else {
                    r_account_maps.startup_take_duplicates_from_in_memory_only()
                }
            })
            .for_each(f);
    }

    /// Updates the given pubkey at the given slot with the new account information.
    /// on return, the index's previous account info may be returned in 'reclaims' depending on 'previous_slot_entry_was_cached'
    pub fn upsert(
        &self,
        new_slot: Slot,
        old_slot: Slot,
        pubkey: &Pubkey,
        account: &impl ReadableAccount,
        account_indexes: &AccountSecondaryIndexes,
        account_info: T,
        reclaims: &mut ReclaimsSlotList<T>,
        reclaim: UpsertReclaim,
    ) {
        // vast majority of updates are to item already in accounts index, so store as raw to avoid unnecessary allocations
        let store_raw = true;

        // We don't atomically update both primary index and secondary index together.
        // This certainly creates a small time window with inconsistent state across the two indexes.
        // However, this is acceptable because:
        //
        //  - A strict consistent view at any given moment of time is not necessary, because the only
        //  use case for the secondary index is `scan`, and `scans` are only supported/require consistency
        //  on frozen banks, and this inconsistency is only possible on working banks.
        //
        //  - The secondary index is never consulted as primary source of truth for gets/stores.
        //  So, what the accounts_index sees alone is sufficient as a source of truth for other non-scan
        //  account operations.
        let new_item = PreAllocatedAccountMapEntry::new(
            new_slot,
            account_info,
            &self.storage.storage,
            store_raw,
        );
        let map = self.get_bin(pubkey);

        map.upsert(pubkey, new_item, Some(old_slot), reclaims, reclaim);
        self.update_secondary_indexes(pubkey, account, account_indexes);
    }

    pub fn ref_count_from_storage(&self, pubkey: &Pubkey) -> RefCount {
        let map = self.get_bin(pubkey);
        map.get_internal_inner(pubkey, |entry| {
            (
                false,
                entry.map(|entry| entry.ref_count()).unwrap_or_default(),
            )
        })
    }

    fn purge_secondary_indexes_by_inner_key(
        &self,
        inner_key: &Pubkey,
        account_indexes: &AccountSecondaryIndexes,
    ) {
        if account_indexes.contains(&AccountIndex::ProgramId) {
            self.program_id_index.remove_by_inner_key(inner_key);
        }

        if account_indexes.contains(&AccountIndex::SplTokenOwner) {
            self.spl_token_owner_index.remove_by_inner_key(inner_key);
        }

        if account_indexes.contains(&AccountIndex::SplTokenMint) {
            self.spl_token_mint_index.remove_by_inner_key(inner_key);
        }
    }

    /// Returns true if the slot list was completely purged (is empty at the end).
    fn purge_older_root_entries(
        &self,
        slot_list: &mut SlotListWriteGuard<T>,
        reclaims: &mut ReclaimsSlotList<T>,
        max_clean_root_inclusive: Option<Slot>,
    ) -> bool {
        if slot_list.len() <= 1 {
            self.purge_older_root_entries_one_slot_list
                .fetch_add(1, Ordering::Relaxed);
        }
        let newest_root_in_slot_list;
        let max_clean_root_inclusive = {
            let roots_tracker = &self.roots_tracker.read().unwrap();
            newest_root_in_slot_list = Self::get_newest_root_in_slot_list(
                &roots_tracker.alive_roots,
                slot_list,
                max_clean_root_inclusive,
            );
            max_clean_root_inclusive.unwrap_or_else(|| roots_tracker.alive_roots.max_inclusive())
        };

        slot_list.retain_and_count(|(slot, value)| {
            let should_purge = Self::can_purge_older_entries(
                // Note that we have a root that is inclusive here.
                // Calling a function that expects 'exclusive'
                // This is expected behavior for this call.
                max_clean_root_inclusive,
                newest_root_in_slot_list,
                *slot,
            ) && !value.is_cached();
            if should_purge {
                reclaims.push((*slot, *value));
            }
            !should_purge
        }) == 0
    }

    /// return true if pubkey was removed from the accounts index
    ///  or does not exist in the accounts index
    /// This means it should NOT be unref'd later.
    #[must_use]
    pub fn clean_rooted_entries(
        &self,
        pubkey: &Pubkey,
        reclaims: &mut ReclaimsSlotList<T>,
        max_clean_root_inclusive: Option<Slot>,
    ) -> bool {
        let mut is_slot_list_empty = false;
        let missing_in_accounts_index = self
            .slot_list_mut(pubkey, |mut slot_list| {
                is_slot_list_empty = self.purge_older_root_entries(
                    &mut slot_list,
                    reclaims,
                    max_clean_root_inclusive,
                );
            })
            .is_none();

        let mut removed = false;
        // If the slot list is empty, remove the pubkey from `account_maps`. Make sure to grab the
        // lock and double check the slot list is still empty, because another writer could have
        // locked and inserted the pubkey in-between when `is_slot_list_empty=true` and the call to
        // remove() below.
        if is_slot_list_empty {
            let w_maps = self.get_bin(pubkey);
            removed = w_maps.remove_if_slot_list_empty(*pubkey);
        }
        removed || missing_in_accounts_index
    }

    /// Clean the slot list by removing all slot_list items older than the max_slot
    /// Decrease the reference count of the entry by the number of removed accounts.
    /// Returns the slot and account_info of the remaining entry in the slot list
    /// Note: This must only be called on startup, and reclaims
    /// must be reclaimed.
    fn clean_and_unref_slot_list_on_startup(
        &self,
        entry: &AccountMapEntry<T>,
        reclaims: &mut ReclaimsSlotList<T>,
    ) -> (u64, T) {
        let mut slot_list = entry.slot_list_write_lock();
        let max_slot = slot_list
            .iter()
            .map(|(slot, _account)| *slot)
            .max()
            .expect("Slot list has entries");

        let mut reclaim_count = 0;

        slot_list.retain_and_count(|(slot, value)| {
            // keep the newest entry, and reclaim all others
            if *slot < max_slot {
                assert!(!value.is_cached(), "Unsafe to reclaim cached entries");
                reclaims.push((*slot, *value));
                reclaim_count += 1;
                false
            } else {
                true
            }
        });

        // Unref
        entry.unref_by_count(reclaim_count);
        assert_eq!(
            entry.ref_count(),
            1,
            "ref count should be one after cleaning all entries"
        );

        entry.set_dirty(true);

        // Return the last entry in the slot list, which is the only one
        *slot_list
            .last()
            .expect("Slot list should have at least one entry after cleaning")
    }

    /// Cleans and unrefs all older rooted entries for each pubkey in the accounts index.
    /// Calls passed in callback on the remaining slot entry
    /// All pubkeys must be from a single bin
    pub fn clean_and_unref_rooted_entries_by_bin(
        &self,
        pubkeys_by_bin: &[Pubkey],
    ) -> ReclaimsSlotList<T> {
        let mut reclaims = ReclaimsSlotList::new();

        let map = match pubkeys_by_bin.first() {
            Some(pubkey) => self.get_bin(pubkey),
            None => return reclaims, // no pubkeys to process, return
        };

        for pubkey in pubkeys_by_bin {
            map.get_internal_inner(pubkey, |entry| {
                let entry = entry.expect("Expected entry to exist in accounts index");
                self.clean_and_unref_slot_list_on_startup(entry, &mut reclaims);
                (false, ())
            });
        }
        reclaims
    }

    /// When can an entry be purged?
    ///
    /// If we get a slot update where slot != newest_root_in_slot_list for an account where slot <
    /// max_clean_root_exclusive, then we know it's safe to delete because:
    ///
    /// a) If slot < newest_root_in_slot_list, then we know the update is outdated by a later rooted
    /// update, namely the one in newest_root_in_slot_list
    ///
    /// b) If slot > newest_root_in_slot_list, then because slot < max_clean_root_exclusive and we know there are
    /// no roots in the slot list between newest_root_in_slot_list and max_clean_root_exclusive, (otherwise there
    /// would be a bigger newest_root_in_slot_list, which is a contradiction), then we know slot must be
    /// an unrooted slot less than max_clean_root_exclusive and thus safe to clean as well.
    fn can_purge_older_entries(
        max_clean_root_exclusive: Slot,
        newest_root_in_slot_list: Slot,
        slot: Slot,
    ) -> bool {
        slot < max_clean_root_exclusive && slot != newest_root_in_slot_list
    }

    /// Given a list of slots, return a new list of only the slots that are rooted
    pub fn get_rooted_from_list<'a>(&self, slots: impl Iterator<Item = &'a Slot>) -> Vec<Slot> {
        let roots_tracker = self.roots_tracker.read().unwrap();
        slots
            .filter_map(|s| {
                if roots_tracker.alive_roots.contains(s) {
                    Some(*s)
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn is_alive_root(&self, slot: Slot) -> bool {
        self.roots_tracker
            .read()
            .unwrap()
            .alive_roots
            .contains(&slot)
    }

    pub fn add_root(&self, slot: Slot) {
        self.roots_added.fetch_add(1, Ordering::Relaxed);
        let mut w_roots_tracker = self.roots_tracker.write().unwrap();
        // `AccountsDb::flush_accounts_cache()` relies on roots being added in order
        assert!(
            slot >= w_roots_tracker.alive_roots.max_inclusive(),
            "Roots must be added in order: {} < {}",
            slot,
            w_roots_tracker.alive_roots.max_inclusive()
        );
        // 'slot' is a root, so it is both 'root' and 'original'
        w_roots_tracker.alive_roots.insert(slot);
    }

    pub fn max_root_inclusive(&self) -> Slot {
        self.roots_tracker
            .read()
            .unwrap()
            .alive_roots
            .max_inclusive()
    }

    /// Remove the slot when the storage for the slot is freed
    /// Accounts no longer reference this slot.
    /// return true if slot was a root
    pub fn clean_dead_slot(&self, slot: Slot) -> bool {
        let mut w_roots_tracker = self.roots_tracker.write().unwrap();
        if !w_roots_tracker.alive_roots.remove(&slot) {
            false
        } else {
            drop(w_roots_tracker);
            self.roots_removed.fetch_add(1, Ordering::Relaxed);
            true
        }
    }

    pub(crate) fn update_roots_stats(&self, stats: &mut AccountsIndexRootsStats) {
        let roots_tracker = self.roots_tracker.read().unwrap();
        stats.roots_len = Some(roots_tracker.alive_roots.len());
        stats.roots_range = Some(roots_tracker.alive_roots.range_width());
    }

    pub fn all_alive_roots(&self) -> Vec<Slot> {
        let tracker = self.roots_tracker.read().unwrap();
        tracker.alive_roots.get_all()
    }

    // These functions/fields are only usable from a dev context (i.e. tests and benches)
    #[cfg(feature = "dev-context-only-utils")]
    // filter any rooted entries and return them along with a bool that indicates
    // if this account has no more entries. Note this does not update the secondary
    // indexes!
    pub fn purge_roots(&self, pubkey: &Pubkey) -> (SlotList<T>, bool) {
        self.slot_list_mut(pubkey, |mut slot_list| {
            let reclaims = self.get_rooted_entries(&slot_list, None);
            let is_empty = slot_list.retain_and_count(|(slot, _)| !self.is_alive_root(*slot)) == 0;
            (reclaims, is_empty)
        })
        .unwrap()
    }
}

/// modes the system can be in
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Startup {
    /// not startup, but steady state execution
    Normal,
    /// startup (not steady state execution)
    /// requesting 'startup'-like behavior where in-mem acct idx items are flushed asap
    Startup,
}

#[cfg(test)]
pub mod tests {
    use {
        super::{bucket_map_holder::BucketMapHolder, *},
        crate::accounts_index::account_map_entry::AccountMapEntryMeta,
        solana_account::AccountSharedData,
        solana_pubkey::PUBKEY_BYTES,
        spl_generic_token::{spl_token_ids, token::SPL_TOKEN_ACCOUNT_OWNER_OFFSET},
        std::ops::{
            Bound::{Excluded, Included, Unbounded},
            RangeInclusive,
        },
        test_case::test_matrix,
    };

    pub enum SecondaryIndexTypes<'a> {
        RwLock(&'a SecondaryIndex<RwLockSecondaryIndexEntry>),
    }

    pub fn spl_token_mint_index_enabled() -> AccountSecondaryIndexes {
        let mut account_indexes = HashSet::new();
        account_indexes.insert(AccountIndex::SplTokenMint);
        AccountSecondaryIndexes {
            indexes: account_indexes,
            keys: None,
        }
    }

    pub fn spl_token_owner_index_enabled() -> AccountSecondaryIndexes {
        let mut account_indexes = HashSet::new();
        account_indexes.insert(AccountIndex::SplTokenOwner);
        AccountSecondaryIndexes {
            indexes: account_indexes,
            keys: None,
        }
    }

    fn create_spl_token_mint_secondary_index_state() -> (usize, usize, AccountSecondaryIndexes) {
        {
            // Check that we're actually testing the correct variant
            let index = AccountsIndex::<bool, bool>::default_for_tests();
            let _type_check = SecondaryIndexTypes::RwLock(&index.spl_token_mint_index);
        }

        (0, PUBKEY_BYTES, spl_token_mint_index_enabled())
    }

    fn create_spl_token_owner_secondary_index_state() -> (usize, usize, AccountSecondaryIndexes) {
        {
            // Check that we're actually testing the correct variant
            let index = AccountsIndex::<bool, bool>::default_for_tests();
            let _type_check = SecondaryIndexTypes::RwLock(&index.spl_token_owner_index);
        }

        (
            SPL_TOKEN_ACCOUNT_OWNER_OFFSET,
            SPL_TOKEN_ACCOUNT_OWNER_OFFSET + PUBKEY_BYTES,
            spl_token_owner_index_enabled(),
        )
    }

    #[test]
    fn test_get_empty() {
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let ancestors = Ancestors::default();
        let key = &key;
        assert!(!index.contains_with(key, Some(&ancestors), None));
        assert!(!index.contains_with(key, None, None));

        let mut num = 0;
        index
            .scan_accounts(
                &ancestors,
                0,
                |_pubkey, _index| num += 1,
                &ScanConfig::default(),
            )
            .expect("scan should succeed");
        assert_eq!(num, 0);
    }

    #[test]
    fn test_secondary_index_include_exclude() {
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();
        let mut index = AccountSecondaryIndexes::default();

        assert!(!index.contains(&AccountIndex::ProgramId));
        index.indexes.insert(AccountIndex::ProgramId);
        assert!(index.contains(&AccountIndex::ProgramId));
        assert!(index.include_key(&pk1));
        assert!(index.include_key(&pk2));

        let exclude = false;
        index.keys = Some(AccountSecondaryIndexesIncludeExclude {
            keys: [pk1].iter().cloned().collect::<HashSet<_>>(),
            exclude,
        });
        assert!(index.include_key(&pk1));
        assert!(!index.include_key(&pk2));

        let exclude = true;
        index.keys = Some(AccountSecondaryIndexesIncludeExclude {
            keys: [pk1].iter().cloned().collect::<HashSet<_>>(),
            exclude,
        });
        assert!(!index.include_key(&pk1));
        assert!(index.include_key(&pk2));

        let exclude = true;
        index.keys = Some(AccountSecondaryIndexesIncludeExclude {
            keys: [pk1, pk2].iter().cloned().collect::<HashSet<_>>(),
            exclude,
        });
        assert!(!index.include_key(&pk1));
        assert!(!index.include_key(&pk2));

        let exclude = false;
        index.keys = Some(AccountSecondaryIndexesIncludeExclude {
            keys: [pk1, pk2].iter().cloned().collect::<HashSet<_>>(),
            exclude,
        });
        assert!(index.include_key(&pk1));
        assert!(index.include_key(&pk2));
    }

    const UPSERT_RECLAIM_TEST_DEFAULT: UpsertReclaim = UpsertReclaim::PopulateReclaims;

    #[test]
    fn test_insert_no_ancestors() {
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let mut gc = ReclaimsSlotList::new();
        index.upsert(
            0,
            0,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            true,
            &mut gc,
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        assert!(gc.is_empty());

        let ancestors = Ancestors::default();
        assert!(!index.contains_with(&key, Some(&ancestors), None));
        assert!(!index.contains_with(&key, None, None));

        let mut num = 0;
        index
            .scan_accounts(
                &ancestors,
                0,
                |_pubkey, _index| num += 1,
                &ScanConfig::default(),
            )
            .expect("scan should succeed");
        assert_eq!(num, 0);
    }

    type AccountInfoTest = f64;

    impl IndexValue for AccountInfoTest {}
    impl DiskIndexValue for AccountInfoTest {}
    impl IsCached for AccountInfoTest {
        fn is_cached(&self) -> bool {
            true
        }
    }

    impl IsZeroLamport for AccountInfoTest {
        fn is_zero_lamport(&self) -> bool {
            true
        }
    }

    #[test]
    #[should_panic(expected = "Accounts may only be stored once per slot:")]
    fn test_insert_duplicates() {
        let key = solana_pubkey::new_rand();
        let pubkey = &key;
        let slot = 0;
        let mut ancestors = Ancestors::default();
        ancestors.insert(slot, 0);

        let account_info = true;
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let account_info2: bool = !account_info;
        let items = vec![(*pubkey, account_info), (*pubkey, account_info2)];
        index.set_startup(Startup::Startup);
        let (_, _result) = index.insert_new_if_missing_into_primary_index(slot, items);
    }

    #[test]
    fn test_insert_new_with_lock_no_ancestors() {
        let key = solana_pubkey::new_rand();
        let pubkey = &key;
        let slot = 0;

        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let account_info = true;
        let items = vec![(*pubkey, account_info)];
        index.set_startup(Startup::Startup);
        let expected_len = items.len();
        let (_, result) = index.insert_new_if_missing_into_primary_index(slot, items);
        assert_eq!(result.count, expected_len);
        index.set_startup(Startup::Normal);

        let mut ancestors = Ancestors::default();
        assert!(!index.contains_with(pubkey, Some(&ancestors), None));
        assert!(!index.contains_with(pubkey, None, None));

        let mut num = 0;
        index
            .scan_accounts(
                &ancestors,
                0,
                |_pubkey, _index| num += 1,
                &ScanConfig::default(),
            )
            .expect("scan should succeed");
        assert_eq!(num, 0);
        ancestors.insert(slot, 0);
        assert!(index.contains_with(pubkey, Some(&ancestors), None));
        assert_eq!(index.ref_count_from_storage(pubkey), 1);
        index
            .scan_accounts(
                &ancestors,
                0,
                |_pubkey, _index| num += 1,
                &ScanConfig::default(),
            )
            .expect("scan should succeed");
        assert_eq!(num, 1);

        // not zero lamports
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let account_info = false;
        let items = vec![(*pubkey, account_info)];
        index.set_startup(Startup::Startup);
        let expected_len = items.len();
        let (_, result) = index.insert_new_if_missing_into_primary_index(slot, items);
        assert_eq!(result.count, expected_len);
        index.set_startup(Startup::Normal);

        let mut ancestors = Ancestors::default();
        assert!(!index.contains_with(pubkey, Some(&ancestors), None));
        assert!(!index.contains_with(pubkey, None, None));

        let mut num = 0;
        index
            .scan_accounts(
                &ancestors,
                0,
                |_pubkey, _index| num += 1,
                &ScanConfig::default(),
            )
            .expect("scan should succeed");
        assert_eq!(num, 0);
        ancestors.insert(slot, 0);
        assert!(index.contains_with(pubkey, Some(&ancestors), None));
        assert_eq!(index.ref_count_from_storage(pubkey), 1);
        index
            .scan_accounts(
                &ancestors,
                0,
                |_pubkey, _index| num += 1,
                &ScanConfig::default(),
            )
            .expect("scan should succeed");
        assert_eq!(num, 1);
    }

    fn get_pre_allocated<T: IndexValue>(
        slot: Slot,
        account_info: T,
        storage: &Arc<BucketMapHolder<T, T>>,
        store_raw: bool,
        to_raw_first: bool,
    ) -> PreAllocatedAccountMapEntry<T> {
        let entry = PreAllocatedAccountMapEntry::new(slot, account_info, storage, store_raw);

        if to_raw_first {
            // convert to raw
            let (slot2, account_info2) = entry.into();
            // recreate using extracted raw
            PreAllocatedAccountMapEntry::new(slot2, account_info2, storage, store_raw)
        } else {
            entry
        }
    }

    #[test]
    fn test_clean_and_unref_rooted_entries_by_bin_empty() {
        let index: AccountsIndex<bool, bool> = AccountsIndex::<bool, bool>::default_for_tests();
        let pubkeys_by_bin: Vec<Pubkey> = vec![];

        let reclaims = index.clean_and_unref_rooted_entries_by_bin(&pubkeys_by_bin);

        assert!(reclaims.is_empty());
    }

    #[test]
    fn test_clean_and_unref_rooted_entries_by_bin_single_entry() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let pubkey = solana_pubkey::new_rand();
        let slot = 0;
        let account_info = true;

        let mut gc = ReclaimsSlotList::new();
        index.upsert(
            slot,
            slot,
            &pubkey,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            account_info,
            &mut gc,
            UPSERT_RECLAIM_TEST_DEFAULT,
        );

        assert!(gc.is_empty());

        let reclaims = index.clean_and_unref_rooted_entries_by_bin(&[pubkey]);

        assert_eq!(reclaims.len(), 0);
    }

    #[test]
    fn test_clean_and_unref_rooted_entries_by_bin_with_reclaim() {
        let index = AccountsIndex::<u64, u64>::default_for_tests();
        let pubkey = solana_pubkey::new_rand();
        let slot1 = 0;
        let slot2 = 1;
        let account_info1 = 0;
        let account_info2 = 1;

        let mut gc = ReclaimsSlotList::new();
        for (slot, account_info) in [(slot1, account_info1), (slot2, account_info2)] {
            index.upsert(
                slot,
                slot,
                &pubkey,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                account_info,
                &mut gc,
                UpsertReclaim::IgnoreReclaims,
            );
        }

        assert!(gc.is_empty());

        let reclaims = index.clean_and_unref_rooted_entries_by_bin(&[pubkey]);
        assert_eq!(reclaims, ReclaimsSlotList::from([(slot1, account_info1)]));
    }

    #[test]
    fn test_clean_and_unref_rooted_entries_by_bin_multiple_pubkeys() {
        let index: AccountsIndex<bool, bool> = AccountsIndex::<bool, bool>::default_for_tests();
        let bin_index = 0;
        let mut pubkeys = Vec::new();
        let mut expected_reclaims = ReclaimsSlotList::new();
        let mut gc = ReclaimsSlotList::new();

        while pubkeys.len() < 10 {
            let new_pubkey = solana_pubkey::new_rand();

            // Ensure the pubkey is in the desired bin
            if index.bin_calculator.bin_from_pubkey(&new_pubkey) == bin_index {
                pubkeys.push(new_pubkey);
            }
        }

        for (i, pubkey) in pubkeys.iter().enumerate() {
            let num_inserts: u64 = i as u64 % 4 + 1;

            for slot in 0..num_inserts {
                if slot > 0 {
                    expected_reclaims.push((slot - 1, true));
                }
                index.upsert(
                    slot,
                    slot,
                    pubkey,
                    &AccountSharedData::default(),
                    &AccountSecondaryIndexes::default(),
                    true,
                    &mut gc,
                    UpsertReclaim::IgnoreReclaims,
                );
            }
        }

        assert!(gc.is_empty());

        let mut reclaims = index.clean_and_unref_rooted_entries_by_bin(&pubkeys);
        reclaims.sort_unstable();
        expected_reclaims.sort_unstable();

        assert!(!reclaims.is_empty());
        assert_eq!(reclaims, expected_reclaims);
    }

    #[test]
    fn test_new_entry() {
        for store_raw in [false, true] {
            for to_raw_first in [false, true] {
                let slot = 0;
                // account_info type that IS cached
                let account_info = AccountInfoTest::default();
                let index = AccountsIndex::default_for_tests();

                let new_entry = get_pre_allocated(
                    slot,
                    account_info,
                    &index.storage.storage,
                    store_raw,
                    to_raw_first,
                )
                .into_account_map_entry(&index.storage.storage);
                assert_eq!(new_entry.ref_count(), 0);
                assert_eq!(new_entry.slot_list_lock_read_len(), 1);
                assert_eq!(
                    new_entry.slot_list_read_lock().to_vec(),
                    vec![(slot, account_info)]
                );

                // account_info type that is NOT cached
                let account_info = true;
                let index = AccountsIndex::default_for_tests();

                let new_entry = get_pre_allocated(
                    slot,
                    account_info,
                    &index.storage.storage,
                    store_raw,
                    to_raw_first,
                )
                .into_account_map_entry(&index.storage.storage);
                assert_eq!(new_entry.ref_count(), 1);
                assert_eq!(new_entry.slot_list_lock_read_len(), 1);
                assert_eq!(
                    new_entry.slot_list_read_lock().to_vec(),
                    vec![(slot, account_info)]
                );
            }
        }
    }

    #[test]
    fn test_batch_insert() {
        let slot0 = 0;
        let key0 = solana_pubkey::new_rand();
        let key1 = solana_pubkey::new_rand();

        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let account_infos = [true, false];

        index.set_startup(Startup::Startup);
        let items = vec![(key0, account_infos[0]), (key1, account_infos[1])];
        let expected_len = items.len();
        let (_, result) = index.insert_new_if_missing_into_primary_index(slot0, items);
        assert_eq!(result.count, expected_len);
        index.set_startup(Startup::Normal);

        for (i, key) in [key0, key1].iter().enumerate() {
            index.get_and_then(key, |entry| {
                assert_eq!(entry.unwrap().ref_count(), 1);
                assert_eq!(
                    entry.unwrap().slot_list_read_lock().as_ref(),
                    &[(slot0, account_infos[i])],
                );
                (false, ())
            });
        }
    }

    fn test_new_entry_code_paths_helper<T: IndexValue>(
        account_infos: [T; 2],
        is_cached: bool,
        upsert_method: Option<UpsertReclaim>,
        use_disk: bool,
    ) {
        if is_cached && upsert_method.is_none() {
            // This is an illegal combination when we are using queued lazy inserts.
            // Cached items don't ever leave the in-mem cache.
            // But the queued lazy insert code relies on there being nothing in the in-mem cache.
            return;
        }

        let slot0 = 0;
        let slot1 = 1;
        let key = solana_pubkey::new_rand();

        let mut config = ACCOUNTS_INDEX_CONFIG_FOR_TESTING;
        config.index_limit = if use_disk {
            IndexLimit::Minimal
        } else {
            IndexLimit::InMemOnly
        };
        let index = AccountsIndex::<T, T>::new(&config, Arc::default());
        let mut gc = ReclaimsSlotList::new();

        match upsert_method {
            Some(upsert_method) => {
                // insert first entry for pubkey. This will use new_entry_after_update and not call update.
                index.upsert(
                    slot0,
                    slot0,
                    &key,
                    &AccountSharedData::default(),
                    &AccountSecondaryIndexes::default(),
                    account_infos[0],
                    &mut gc,
                    upsert_method,
                );
            }
            None => {
                let items = vec![(key, account_infos[0])];
                index.set_startup(Startup::Startup);
                let expected_len = items.len();
                let (_, result) = index.insert_new_if_missing_into_primary_index(slot0, items);
                assert_eq!(result.count, expected_len);
                index.set_startup(Startup::Normal);
            }
        }
        assert!(gc.is_empty());

        // verify the added entry matches expected
        index.get_and_then(&key, |entry| {
            let entry = entry.unwrap();
            let slot_list = entry.slot_list_read_lock();
            assert_eq!(entry.ref_count(), RefCount::from(!is_cached));
            assert_eq!(slot_list.as_ref(), &[(slot0, account_infos[0])]);
            let new_entry = PreAllocatedAccountMapEntry::new(
                slot0,
                account_infos[0],
                &index.storage.storage,
                false,
            )
            .into_account_map_entry(&index.storage.storage);
            assert_eq!(slot_list.as_ref(), new_entry.slot_list_read_lock().as_ref(),);
            (false, ())
        });

        match upsert_method {
            Some(upsert_method) => {
                // insert second entry for pubkey. This will use update and NOT use new_entry_after_update.
                index.upsert(
                    slot1,
                    slot1,
                    &key,
                    &AccountSharedData::default(),
                    &AccountSecondaryIndexes::default(),
                    account_infos[1],
                    &mut gc,
                    upsert_method,
                );
            }
            None => {
                // this has the effect of aging out everything in the in-mem cache
                for _ in 0..5 {
                    index.set_startup(Startup::Startup);
                    index.set_startup(Startup::Normal);
                }

                let items = vec![(key, account_infos[1])];
                index.set_startup(Startup::Startup);
                let expected_len = items.len();
                let (_, result) = index.insert_new_if_missing_into_primary_index(slot1, items);
                assert_eq!(result.count, expected_len);
                index.set_startup(Startup::Normal);
            }
        }

        // There should be reclaims if entries are uncached and old slots are being reclaimed
        let should_have_reclaims =
            upsert_method == Some(UpsertReclaim::ReclaimOldSlots) && !is_cached;

        if should_have_reclaims {
            assert!(!gc.is_empty());
            assert_eq!(gc.len(), 1);
            assert_eq!(gc[0], (slot0, account_infos[0]));
        } else {
            assert!(gc.is_empty());
        }

        index.populate_and_retrieve_duplicate_keys_from_startup(|_slot_keys| {});

        let last_item = index.get_and_then(&key, |entry| {
            let entry = entry.unwrap();
            let slot_list = entry.slot_list_read_lock();

            if should_have_reclaims {
                assert_eq!(entry.ref_count(), 1);
                assert_eq!(slot_list.as_ref(), &[(slot1, account_infos[1])],);
            } else {
                assert_eq!(entry.ref_count(), if is_cached { 0 } else { 2 });
                assert_eq!(
                    slot_list.as_ref(),
                    &[(slot0, account_infos[0]), (slot1, account_infos[1])],
                );
            }
            (false, *slot_list.last().unwrap())
        });

        let new_entry = PreAllocatedAccountMapEntry::new(
            slot1,
            account_infos[1],
            &index.storage.storage,
            false,
        );

        assert_eq!(last_item, new_entry.into());
    }

    #[test_matrix(
        [false, true],
        [None, Some(UpsertReclaim::PopulateReclaims), Some(UpsertReclaim::ReclaimOldSlots)],
        [true, false]
    )]
    fn test_new_entry_and_update_code_paths(
        use_disk: bool,
        upsert_method: Option<UpsertReclaim>,
        is_cached: bool,
    ) {
        if is_cached {
            // account_info type that IS cached
            test_new_entry_code_paths_helper([1.0, 2.0], true, upsert_method, use_disk);
        } else {
            // account_info type that is NOT cached
            test_new_entry_code_paths_helper([1, 2], false, upsert_method, use_disk);
        }
    }

    #[test]
    fn test_insert_with_lock_no_ancestors() {
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let slot = 0;
        let account_info = true;

        let new_entry =
            PreAllocatedAccountMapEntry::new(slot, account_info, &index.storage.storage, false);
        assert_eq!(0, account_maps_stats_len(&index));

        assert_eq!(0, account_maps_stats_len(&index));
        let r_account_maps = index.get_bin(&key);
        r_account_maps.upsert(
            &key,
            new_entry,
            None,
            &mut ReclaimsSlotList::default(),
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        assert_eq!(1, account_maps_stats_len(&index));

        let mut ancestors = Ancestors::default();
        assert!(!index.contains_with(&key, Some(&ancestors), None));
        assert!(!index.contains_with(&key, None, None));
        index.get_and_then(&key, |entry| {
            let (stored_slot, value) = entry.unwrap().slot_list_read_lock()[0];
            assert_eq!(stored_slot, slot);
            assert_eq!(value, account_info);
            (false, ())
        });

        let mut num = 0;
        index
            .scan_accounts(
                &ancestors,
                0,
                |_pubkey, _index| num += 1,
                &ScanConfig::default(),
            )
            .expect("scan should succeed");
        assert_eq!(num, 0);
        ancestors.insert(slot, 0);
        assert!(index.contains_with(&key, Some(&ancestors), None));
        index
            .scan_accounts(
                &ancestors,
                0,
                |_pubkey, _index| num += 1,
                &ScanConfig::default(),
            )
            .expect("scan should succeed");
        assert_eq!(num, 1);
    }

    #[test]
    fn test_insert_wrong_ancestors() {
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let mut gc = ReclaimsSlotList::new();
        index.upsert(
            0,
            0,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            true,
            &mut gc,
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        assert!(gc.is_empty());

        let ancestors = vec![(1, 1)].into_iter().collect();
        assert!(!index.contains_with(&key, Some(&ancestors), None));

        let mut num = 0;
        index
            .scan_accounts(
                &ancestors,
                0,
                |_pubkey, _index| num += 1,
                &ScanConfig::default(),
            )
            .expect("scan should succeed");
        assert_eq!(num, 0);
    }
    #[test]
    fn test_insert_ignore_reclaims() {
        {
            // non-cached
            let key = solana_pubkey::new_rand();
            let index = AccountsIndex::<u64, u64>::default_for_tests();
            let mut reclaims = ReclaimsSlotList::new();
            let slot = 0;
            let value = 1;
            assert!(!value.is_cached());
            index.upsert(
                slot,
                slot,
                &key,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                value,
                &mut reclaims,
                UpsertReclaim::PopulateReclaims,
            );
            assert!(reclaims.is_empty());
            index.upsert(
                slot,
                slot,
                &key,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                value,
                &mut reclaims,
                UpsertReclaim::PopulateReclaims,
            );
            // reclaimed
            assert!(!reclaims.is_empty());
            reclaims.clear();
            index.upsert(
                slot,
                slot,
                &key,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                value,
                &mut reclaims,
                // since IgnoreReclaims, we should expect reclaims to be empty
                UpsertReclaim::IgnoreReclaims,
            );
            // reclaims is ignored
            assert!(reclaims.is_empty());
        }
        {
            // cached
            let key = solana_pubkey::new_rand();
            let index = AccountsIndex::<AccountInfoTest, AccountInfoTest>::default_for_tests();
            let mut reclaims = ReclaimsSlotList::new();
            let slot = 0;
            let value = 1.0;
            assert!(value.is_cached());
            index.upsert(
                slot,
                slot,
                &key,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                value,
                &mut reclaims,
                UpsertReclaim::PopulateReclaims,
            );
            assert!(reclaims.is_empty());
            index.upsert(
                slot,
                slot,
                &key,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                value,
                &mut reclaims,
                UpsertReclaim::PopulateReclaims,
            );
            // No reclaims, since the entry replaced was cached
            assert!(reclaims.is_empty());
            index.upsert(
                slot,
                slot,
                &key,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                value,
                &mut reclaims,
                // since IgnoreReclaims, we should expect reclaims to be empty
                UpsertReclaim::IgnoreReclaims,
            );
            // reclaims is ignored
            assert!(reclaims.is_empty());
        }
    }

    #[test]
    fn test_insert_with_ancestors() {
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let mut gc = ReclaimsSlotList::new();
        index.upsert(
            0,
            0,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            true,
            &mut gc,
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        assert!(gc.is_empty());

        let ancestors = vec![(0, 0)].into_iter().collect();
        index
            .get_with_and_then(
                &key,
                Some(&ancestors),
                None,
                false,
                |(slot, account_info)| {
                    assert_eq!(slot, 0);
                    assert!(account_info);
                },
            )
            .unwrap();

        let mut num = 0;
        let mut found_key = false;
        index
            .scan_accounts(
                &ancestors,
                0,
                |pubkey, _index| {
                    if pubkey == &key {
                        found_key = true
                    };
                    num += 1
                },
                &ScanConfig::default(),
            )
            .expect("scan should succeed");

        assert_eq!(num, 1);
        assert!(found_key);
    }

    fn setup_accounts_index_keys(num_pubkeys: usize) -> (AccountsIndex<bool, bool>, Vec<Pubkey>) {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let root_slot = 0;

        let mut pubkeys: Vec<Pubkey> = std::iter::repeat_with(|| {
            let new_pubkey = solana_pubkey::new_rand();
            index.upsert(
                root_slot,
                root_slot,
                &new_pubkey,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                true,
                &mut ReclaimsSlotList::new(),
                UPSERT_RECLAIM_TEST_DEFAULT,
            );
            new_pubkey
        })
        .take(num_pubkeys.saturating_sub(1))
        .collect();

        if num_pubkeys != 0 {
            pubkeys.push(Pubkey::default());
            index.upsert(
                root_slot,
                root_slot,
                &Pubkey::default(),
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                true,
                &mut ReclaimsSlotList::new(),
                UPSERT_RECLAIM_TEST_DEFAULT,
            );
        }

        index.add_root(root_slot);

        (index, pubkeys)
    }

    fn run_test_scan_accounts(num_pubkeys: usize) {
        let (index, _) = setup_accounts_index_keys(num_pubkeys);

        let mut scanned_keys = HashSet::new();
        index
            .scan_accounts(
                &Ancestors::default(),
                0,
                |pubkey, _index| {
                    scanned_keys.insert(*pubkey);
                },
                &ScanConfig::default(),
            )
            .expect("scan should succeed");
        assert_eq!(scanned_keys.len(), num_pubkeys);
    }

    #[test]
    fn test_scan_accounts() {
        run_test_scan_accounts(0);
        run_test_scan_accounts(1);
        run_test_scan_accounts(ITER_BATCH_SIZE * 10);
        run_test_scan_accounts(ITER_BATCH_SIZE * 10 - 1);
        run_test_scan_accounts(ITER_BATCH_SIZE * 10 + 1);
    }

    #[test]
    fn test_is_alive_root() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        assert!(!index.is_alive_root(0));
        index.add_root(0);
        assert!(index.is_alive_root(0));
    }

    #[test]
    fn test_insert_with_root() {
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let mut gc = ReclaimsSlotList::new();
        index.upsert(
            0,
            0,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            true,
            &mut gc,
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        assert!(gc.is_empty());

        index.add_root(0);
        index
            .get_with_and_then(&key, None, None, false, |(slot, account_info)| {
                assert_eq!(slot, 0);
                assert!(account_info);
            })
            .unwrap();
    }

    #[test]
    fn test_clean_first() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        index.add_root(0);
        index.add_root(1);
        index.clean_dead_slot(0);
        assert!(index.is_alive_root(1));
        assert!(!index.is_alive_root(0));
    }

    #[test]
    fn test_clean_last() {
        //this behavior might be undefined, clean up should only occur on older slots
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        index.add_root(0);
        index.add_root(1);
        index.clean_dead_slot(1);
        assert!(!index.is_alive_root(1));
        assert!(index.is_alive_root(0));
    }

    #[test]
    fn test_update_last_wins() {
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<u64, u64>::default_for_tests();
        let ancestors = vec![(0, 0)].into_iter().collect();
        let mut gc = ReclaimsSlotList::new();
        index.upsert(
            0,
            0,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            1,
            &mut gc,
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        assert!(gc.is_empty());
        index
            .get_with_and_then(
                &key,
                Some(&ancestors),
                None,
                false,
                |(slot, account_info)| {
                    assert_eq!(slot, 0);
                    assert_eq!(account_info, 1);
                },
            )
            .unwrap();

        let mut gc = ReclaimsSlotList::new();
        index.upsert(
            0,
            0,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            0,
            &mut gc,
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        assert_eq!(gc, ReclaimsSlotList::from([(0, 1)]));
        index
            .get_with_and_then(
                &key,
                Some(&ancestors),
                None,
                false,
                |(slot, account_info)| {
                    assert_eq!(slot, 0);
                    assert_eq!(account_info, 0);
                },
            )
            .unwrap();
    }

    #[test]
    fn test_update_new_slot() {
        agave_logger::setup();
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let ancestors = vec![(0, 0)].into_iter().collect();
        let mut gc = ReclaimsSlotList::new();
        index.upsert(
            0,
            0,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            true,
            &mut gc,
            UpsertReclaim::PopulateReclaims,
        );
        assert!(gc.is_empty());
        index.upsert(
            1,
            1,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            false,
            &mut gc,
            UpsertReclaim::PopulateReclaims,
        );
        assert!(gc.is_empty());
        index
            .get_with_and_then(
                &key,
                Some(&ancestors),
                None,
                false,
                |(slot, account_info)| {
                    assert_eq!(slot, 0);
                    assert!(account_info);
                },
            )
            .unwrap();
        let ancestors = vec![(1, 0)].into_iter().collect();
        index
            .get_with_and_then(
                &key,
                Some(&ancestors),
                None,
                false,
                |(slot, account_info)| {
                    assert_eq!(slot, 1);
                    assert!(!account_info);
                },
            )
            .unwrap();
    }

    #[test]
    fn test_update_gc_purged_slot() {
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let mut gc = ReclaimsSlotList::new();
        index.upsert(
            0,
            0,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            true,
            &mut gc,
            UpsertReclaim::PopulateReclaims,
        );
        assert!(gc.is_empty());
        index.upsert(
            1,
            1,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            false,
            &mut gc,
            UpsertReclaim::PopulateReclaims,
        );
        index.upsert(
            2,
            2,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            true,
            &mut gc,
            UpsertReclaim::PopulateReclaims,
        );
        index.upsert(
            3,
            3,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            true,
            &mut gc,
            UpsertReclaim::PopulateReclaims,
        );
        index.add_root(0);
        index.add_root(1);
        index.add_root(3);
        index.upsert(
            4,
            4,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            true,
            &mut gc,
            UpsertReclaim::PopulateReclaims,
        );

        // Updating index should not purge older roots, only purges
        // previous updates within the same slot
        assert_eq!(gc, ReclaimsSlotList::new());
        index
            .get_with_and_then(&key, None, None, false, |(slot, account_info)| {
                assert_eq!(slot, 3);
                assert!(account_info);
            })
            .unwrap();

        let mut num = 0;
        let mut found_key = false;
        index
            .scan_accounts(
                &Ancestors::default(),
                0,
                |pubkey, index| {
                    if pubkey == &key {
                        found_key = true;
                        assert_eq!(index, (&true, 3));
                    };
                    num += 1
                },
                &ScanConfig::default(),
            )
            .expect("scan should succeed");
        assert_eq!(num, 1);
        assert!(found_key);
    }

    #[test]
    fn test_upsert_reclaims() {
        let key = solana_pubkey::new_rand();
        let index =
            AccountsIndex::<CacheableIndexValueTest, CacheableIndexValueTest>::default_for_tests();
        let mut reclaims = ReclaimsSlotList::new();
        index.upsert(
            0,
            0,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            CacheableIndexValueTest(true),
            &mut reclaims,
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        // No reclaims should be returned on the first item
        assert!(reclaims.is_empty());

        index.upsert(
            0,
            0,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            CacheableIndexValueTest(false),
            &mut reclaims,
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        // Cached item should not be reclaimed
        assert!(reclaims.is_empty());

        // Slot list should only have a single entry
        let slot_list_len = index.get_and_then(&key, |entry| {
            (false, entry.unwrap().slot_list_lock_read_len())
        });
        assert_eq!(slot_list_len, 1);

        index.upsert(
            0,
            0,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            CacheableIndexValueTest(false),
            &mut reclaims,
            UPSERT_RECLAIM_TEST_DEFAULT,
        );

        // Uncached item should be returned as reclaim
        assert!(!reclaims.is_empty());

        // Slot list should only have a single entry
        let slot_list_len = index.get_and_then(&key, |entry| {
            (false, entry.unwrap().slot_list_lock_read_len())
        });
        assert_eq!(slot_list_len, 1);
    }

    fn account_maps_stats_len<T: IndexValue>(index: &AccountsIndex<T, T>) -> usize {
        index.storage.storage.stats.total_count()
    }

    #[test]
    fn test_purge() {
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<u64, u64>::default_for_tests();
        let mut gc = ReclaimsSlotList::new();
        assert_eq!(0, account_maps_stats_len(&index));
        index.upsert(
            1,
            1,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            12,
            &mut gc,
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        assert_eq!(1, account_maps_stats_len(&index));

        index.upsert(
            1,
            1,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            10,
            &mut gc,
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        assert_eq!(1, account_maps_stats_len(&index));

        let purges = index.purge_roots(&key);
        assert_eq!(purges, (SlotList::new(), false));
        index.add_root(1);

        let purges = index.purge_roots(&key);
        assert_eq!(purges, (SlotList::from([(1, 10)]), true));

        assert_eq!(1, account_maps_stats_len(&index));
        index.upsert(
            1,
            1,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            9,
            &mut gc,
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        assert_eq!(1, account_maps_stats_len(&index));
    }

    #[test]
    fn test_latest_slot() {
        let slot_slice = vec![(0, true), (5, true), (3, true), (7, true)];
        let index = AccountsIndex::<bool, bool>::default_for_tests();

        // No ancestors, no root, should return None
        assert!(index.latest_slot(None, &slot_slice, None).is_none());

        // Given a root, should return the root
        index.add_root(5);
        assert_eq!(index.latest_slot(None, &slot_slice, None).unwrap(), 1);

        // Given a max_root == root, should still return the root
        assert_eq!(index.latest_slot(None, &slot_slice, Some(5)).unwrap(), 1);

        // Given a max_root < root, should filter out the root
        assert!(index.latest_slot(None, &slot_slice, Some(4)).is_none());

        // Given a max_root, should filter out roots < max_root, but specified
        // ancestors should not be affected
        let ancestors = vec![(3, 1), (7, 1)].into_iter().collect();
        assert_eq!(
            index
                .latest_slot(Some(&ancestors), &slot_slice, Some(4))
                .unwrap(),
            3
        );
        assert_eq!(
            index
                .latest_slot(Some(&ancestors), &slot_slice, Some(7))
                .unwrap(),
            3
        );

        // Given no max_root, should just return the greatest ancestor or root
        assert_eq!(
            index
                .latest_slot(Some(&ancestors), &slot_slice, None)
                .unwrap(),
            3
        );
    }

    fn make_empty_token_account_data() -> Vec<u8> {
        const SPL_TOKEN_INITIALIZED_OFFSET: usize = 108;
        let mut data = vec![0; spl_generic_token::token::Account::get_packed_len()];
        data[SPL_TOKEN_INITIALIZED_OFFSET] = 1;
        data
    }

    fn run_test_purge_exact_secondary_index<
        SecondaryIndexEntryType: SecondaryIndexEntry + Default + Sync + Send,
    >(
        index: &AccountsIndex<bool, bool>,
        secondary_index: &SecondaryIndex<SecondaryIndexEntryType>,
        key_start: usize,
        key_end: usize,
        secondary_indexes: &AccountSecondaryIndexes,
    ) {
        // No roots, should be no reclaims
        let slots = vec![1, 2, 5, 9];
        let index_key = Pubkey::new_unique();
        let account_key = Pubkey::new_unique();

        let mut account_data = make_empty_token_account_data();
        account_data[key_start..key_end].clone_from_slice(&(index_key.to_bytes()));

        // Insert slots into secondary index
        for slot in &slots {
            index.upsert(
                *slot,
                *slot,
                &account_key,
                // Make sure these accounts are added to secondary index
                &AccountSharedData::create_from_existing_shared_data(
                    0,
                    Arc::new(account_data.to_vec()),
                    spl_generic_token::token::id(),
                    false,
                    0,
                ),
                secondary_indexes,
                true,
                &mut ReclaimsSlotList::new(),
                UPSERT_RECLAIM_TEST_DEFAULT,
            );
        }

        // Only one top level index entry exists
        assert_eq!(secondary_index.index.get(&index_key).unwrap().len(), 1);

        // In the reverse index, one account maps across multiple slots
        // to the same top level key
        assert_eq!(
            secondary_index
                .reverse_index
                .get(&account_key)
                .unwrap()
                .value()
                .read()
                .unwrap()
                .len(),
            1
        );

        index.purge_exact(
            &account_key,
            slots.into_iter().collect::<HashSet<Slot>>(),
            &mut ReclaimsSlotList::new(),
        );

        let _ = index.handle_dead_keys(&[account_key], secondary_indexes);
        assert!(secondary_index.index.is_empty());
        assert!(secondary_index.reverse_index.is_empty());
    }

    #[test]
    fn test_reclaim_older_items_in_slot_list() {
        agave_logger::setup();
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<u64, u64>::default_for_tests();
        let mut gc = ReclaimsSlotList::new();
        let reclaim_slot = 5;
        let account_value = 50;

        // Insert multiple older items into the slot list
        for slot in 0..reclaim_slot {
            index.upsert(
                slot,
                slot,
                &key,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                slot,
                &mut gc,
                UpsertReclaim::IgnoreReclaims,
            );
        }
        let slot_list_len = index.get_and_then(&key, |entry| {
            (false, entry.unwrap().slot_list_lock_read_len())
        });
        assert_eq!(slot_list_len, reclaim_slot as usize);

        // Insert an item newer than the one that we will reclaim old slots on
        index.upsert(
            reclaim_slot + 1,
            reclaim_slot + 1,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            account_value + 1,
            &mut gc,
            UpsertReclaim::IgnoreReclaims,
        );
        let slot_list_len = index.get_and_then(&key, |entry| {
            (false, entry.unwrap().slot_list_lock_read_len())
        });
        assert_eq!(slot_list_len, (reclaim_slot + 1) as usize);

        // Reclaim all older slots
        index.upsert(
            reclaim_slot,
            reclaim_slot,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            account_value,
            &mut gc,
            UpsertReclaim::ReclaimOldSlots,
        );

        // Verify that older items are reclaimed
        assert_eq!(gc.len(), reclaim_slot as usize);
        for (slot, value) in gc.iter() {
            assert!(*slot < reclaim_slot);
            assert_eq!(*value, *slot);
        }

        // Verify that the item added is in in the slot list
        let ancestors = vec![(reclaim_slot, 0)].into_iter().collect();
        index
            .get_with_and_then(
                &key,
                Some(&ancestors),
                None,
                false,
                |(slot, account_info)| {
                    assert_eq!(slot, reclaim_slot);
                    assert_eq!(account_info, account_value);
                },
            )
            .unwrap();

        // Verify that the newer item remains in the slot list
        let ancestors = vec![((reclaim_slot + 1), 0)].into_iter().collect();
        index
            .get_with_and_then(
                &key,
                Some(&ancestors),
                None,
                false,
                |(slot, account_info)| {
                    assert_eq!(slot, reclaim_slot + 1);
                    assert_eq!(account_info, account_value + 1);
                },
            )
            .unwrap();
    }

    #[test]
    fn test_reclaim_do_not_reclaim_cached_other_slot() {
        agave_logger::setup();
        let key = solana_pubkey::new_rand();
        let index =
            AccountsIndex::<CacheableIndexValueTest, CacheableIndexValueTest>::default_for_tests();
        let mut gc = ReclaimsSlotList::new();

        // Insert an uncached account at slot 0 and an cached account at slot 1
        index.upsert(
            0,
            0,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            CacheableIndexValueTest(false),
            &mut gc,
            UpsertReclaim::IgnoreReclaims,
        );

        index.upsert(
            1,
            1,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            CacheableIndexValueTest(true),
            &mut gc,
            UpsertReclaim::IgnoreReclaims,
        );

        // Now insert a cached account at slot 2
        index.upsert(
            2,
            2,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            CacheableIndexValueTest(true),
            &mut gc,
            UpsertReclaim::IgnoreReclaims,
        );

        // Replace the cached account at slot 2 with a uncached account
        index.upsert(
            2,
            2,
            &key,
            &AccountSharedData::default(),
            &AccountSecondaryIndexes::default(),
            CacheableIndexValueTest(false),
            &mut gc,
            UpsertReclaim::ReclaimOldSlots,
        );

        // Verify that the slot list is length two and consists of the cached account at slot 1
        // and the uncached account at slot 2
        index.get_and_then(&key, |entry| {
            let entry = entry.unwrap();
            assert_eq!(entry.slot_list_lock_read_len(), 2);
            assert_eq!(
                entry.slot_list_read_lock()[0],
                PreAllocatedAccountMapEntry::new(
                    1,
                    CacheableIndexValueTest(true),
                    &index.storage.storage,
                    false
                )
                .into()
            );
            assert_eq!(
                entry.slot_list_read_lock()[1],
                PreAllocatedAccountMapEntry::new(
                    2,
                    CacheableIndexValueTest(false),
                    &index.storage.storage,
                    false
                )
                .into()
            );
            // Verify that the uncached account at slot 0 was reclaimed
            assert_eq!(gc.len(), 1);
            assert_eq!(gc[0], (0, CacheableIndexValueTest(false)));
            (false, ())
        });
    }

    #[test]
    fn test_purge_exact_spl_token_mint_secondary_index() {
        let (key_start, key_end, secondary_indexes) = create_spl_token_mint_secondary_index_state();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        run_test_purge_exact_secondary_index(
            &index,
            &index.spl_token_mint_index,
            key_start,
            key_end,
            &secondary_indexes,
        );
    }

    #[test]
    fn test_purge_exact_spl_token_owner_secondary_index() {
        let (key_start, key_end, secondary_indexes) =
            create_spl_token_owner_secondary_index_state();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        run_test_purge_exact_secondary_index(
            &index,
            &index.spl_token_owner_index,
            key_start,
            key_end,
            &secondary_indexes,
        );
    }

    #[test]
    fn test_purge_older_root_entries() {
        // No roots, should be no reclaims
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let entry = AccountMapEntry::new(
            SlotList::from_iter([(1, true), (2, true), (5, true), (9, true)]),
            1,
            AccountMapEntryMeta::default(),
        );
        let mut slot_list = entry.slot_list_write_lock();
        let mut reclaims = ReclaimsSlotList::new();
        assert!(!index.purge_older_root_entries(&mut slot_list, &mut reclaims, None));
        assert!(reclaims.is_empty());
        assert_eq!(
            slot_list.clone_list(),
            SlotList::from_iter([(1, true), (2, true), (5, true), (9, true)])
        );

        // Add a later root, earlier slots should be reclaimed
        slot_list.assign([(1, true), (2, true), (5, true), (9, true)]);
        index.add_root(1);
        // Note 2 is not a root
        index.add_root(5);
        reclaims = ReclaimsSlotList::new();
        assert!(!index.purge_older_root_entries(&mut slot_list, &mut reclaims, None));
        assert_eq!(reclaims, ReclaimsSlotList::from([(1, true), (2, true)]));
        assert_eq!(
            slot_list.clone_list(),
            SlotList::from_iter([(5, true), (9, true)])
        );
        // Add a later root that is not in the list, should not affect the outcome
        slot_list.assign([(1 as Slot, true), (2, true), (5, true), (9, true)]);
        index.add_root(6);
        reclaims = ReclaimsSlotList::new();
        assert!(!index.purge_older_root_entries(&mut slot_list, &mut reclaims, None));
        assert_eq!(reclaims, ReclaimsSlotList::from([(1, true), (2, true)]));
        assert_eq!(
            slot_list.clone_list(),
            SlotList::from_iter([(5, true), (9, true)])
        );

        // Pass a max root >= than any root in the slot list, should not affect
        // outcome
        slot_list.assign([(1, true), (2, true), (5, true), (9, true)]);
        reclaims = ReclaimsSlotList::new();
        assert!(!index.purge_older_root_entries(&mut slot_list, &mut reclaims, Some(6)));
        assert_eq!(reclaims, ReclaimsSlotList::from([(1, true), (2, true)]));
        assert_eq!(
            slot_list.clone_list(),
            SlotList::from_iter([(5, true), (9, true)])
        );

        // Pass a max root, earlier slots should be reclaimed
        slot_list.assign([(1, true), (2, true), (5, true), (9, true)]);
        reclaims = ReclaimsSlotList::new();
        assert!(!index.purge_older_root_entries(&mut slot_list, &mut reclaims, Some(5)));
        assert_eq!(reclaims, ReclaimsSlotList::from([(1, true), (2, true)]));
        assert_eq!(
            slot_list.clone_list(),
            SlotList::from_iter([(5, true), (9, true)])
        );

        // Pass a max root 2. This means the latest root < 2 is 1 because 2 is not a root
        // so nothing will be purged
        slot_list.assign([(1, true), (2, true), (5, true), (9, true)]);
        reclaims = ReclaimsSlotList::new();
        assert!(!index.purge_older_root_entries(&mut slot_list, &mut reclaims, Some(2)));
        assert!(reclaims.is_empty());
        assert_eq!(
            slot_list.clone_list(),
            SlotList::from_iter([(1, true), (2, true), (5, true), (9, true)])
        );

        // Pass a max root 1. This means the latest root < 3 is 1 because 2 is not a root
        // so nothing will be purged
        slot_list.assign([(1, true), (2, true), (5, true), (9, true)]);
        reclaims = ReclaimsSlotList::new();
        assert!(!index.purge_older_root_entries(&mut slot_list, &mut reclaims, Some(1)));
        assert!(reclaims.is_empty());
        assert_eq!(
            slot_list.clone_list(),
            SlotList::from_iter([(1, true), (2, true), (5, true), (9, true)])
        );

        // Pass a max root that doesn't exist in the list but is greater than
        // some of the roots in the list, shouldn't return those smaller roots
        slot_list.assign([(1, true), (2, true), (5, true), (9, true)]);
        reclaims = ReclaimsSlotList::new();
        assert!(!index.purge_older_root_entries(&mut slot_list, &mut reclaims, Some(7)));
        assert_eq!(reclaims, ReclaimsSlotList::from([(1, true), (2, true)]));
        assert_eq!(
            slot_list.clone_list(),
            SlotList::from_iter([(5, true), (9, true)])
        );
    }

    fn check_secondary_index_mapping_correct<SecondaryIndexEntryType>(
        secondary_index: &SecondaryIndex<SecondaryIndexEntryType>,
        secondary_index_keys: &[Pubkey],
        account_key: &Pubkey,
    ) where
        SecondaryIndexEntryType: SecondaryIndexEntry + Default + Sync + Send,
    {
        // Check secondary index has unique mapping from secondary index key
        // to the account key and slot
        for secondary_index_key in secondary_index_keys {
            assert_eq!(secondary_index.index.len(), secondary_index_keys.len());
            let account_key_map = secondary_index.get(secondary_index_key);
            assert_eq!(account_key_map.len(), 1);
            assert_eq!(account_key_map, vec![*account_key]);
        }
        // Check reverse index contains all of the `secondary_index_keys`
        let secondary_index_key_map = secondary_index.reverse_index.get(account_key).unwrap();
        assert_eq!(
            &*secondary_index_key_map.value().read().unwrap(),
            secondary_index_keys
        );
    }

    fn run_test_spl_token_secondary_indexes<
        SecondaryIndexEntryType: SecondaryIndexEntry + Default + Sync + Send,
    >(
        token_id: &Pubkey,
        index: &AccountsIndex<bool, bool>,
        secondary_index: &SecondaryIndex<SecondaryIndexEntryType>,
        key_start: usize,
        key_end: usize,
        secondary_indexes: &AccountSecondaryIndexes,
    ) {
        let mut secondary_indexes = secondary_indexes.clone();
        let account_key = Pubkey::new_unique();
        let index_key = Pubkey::new_unique();
        let mut account_data = make_empty_token_account_data();
        account_data[key_start..key_end].clone_from_slice(&(index_key.to_bytes()));

        // Wrong program id
        index.upsert(
            0,
            0,
            &account_key,
            &AccountSharedData::create_from_existing_shared_data(
                0,
                Arc::new(account_data.to_vec()),
                Pubkey::default(),
                false,
                0,
            ),
            &secondary_indexes,
            true,
            &mut ReclaimsSlotList::new(),
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        assert!(secondary_index.index.is_empty());
        assert!(secondary_index.reverse_index.is_empty());

        // Wrong account data size
        index.upsert(
            0,
            0,
            &account_key,
            &AccountSharedData::create_from_existing_shared_data(
                0,
                Arc::new(account_data[1..].to_vec()),
                *token_id,
                false,
                0,
            ),
            &secondary_indexes,
            true,
            &mut ReclaimsSlotList::new(),
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        assert!(secondary_index.index.is_empty());
        assert!(secondary_index.reverse_index.is_empty());

        secondary_indexes.keys = None;

        // Just right. Inserting the same index multiple times should be ok
        for _ in 0..2 {
            index.update_secondary_indexes(
                &account_key,
                &AccountSharedData::create_from_existing_shared_data(
                    0,
                    Arc::new(account_data.to_vec()),
                    *token_id,
                    false,
                    0,
                ),
                &secondary_indexes,
            );
            check_secondary_index_mapping_correct(secondary_index, &[index_key], &account_key);
        }

        // included
        assert!(!secondary_index.index.is_empty());
        assert!(!secondary_index.reverse_index.is_empty());

        secondary_indexes.keys = Some(AccountSecondaryIndexesIncludeExclude {
            keys: [index_key].iter().cloned().collect::<HashSet<_>>(),
            exclude: false,
        });
        secondary_index.index.clear();
        secondary_index.reverse_index.clear();
        index.update_secondary_indexes(
            &account_key,
            &AccountSharedData::create_from_existing_shared_data(
                0,
                Arc::new(account_data.to_vec()),
                *token_id,
                false,
                0,
            ),
            &secondary_indexes,
        );
        assert!(!secondary_index.index.is_empty());
        assert!(!secondary_index.reverse_index.is_empty());
        check_secondary_index_mapping_correct(secondary_index, &[index_key], &account_key);

        // not-excluded
        secondary_indexes.keys = Some(AccountSecondaryIndexesIncludeExclude {
            keys: [].iter().cloned().collect::<HashSet<_>>(),
            exclude: true,
        });
        secondary_index.index.clear();
        secondary_index.reverse_index.clear();
        index.update_secondary_indexes(
            &account_key,
            &AccountSharedData::create_from_existing_shared_data(
                0,
                Arc::new(account_data.to_vec()),
                *token_id,
                false,
                0,
            ),
            &secondary_indexes,
        );
        assert!(!secondary_index.index.is_empty());
        assert!(!secondary_index.reverse_index.is_empty());
        check_secondary_index_mapping_correct(secondary_index, &[index_key], &account_key);

        secondary_indexes.keys = None;

        index.slot_list_mut(&account_key, |mut slot_list| slot_list.clear());

        // Everything should be deleted
        let _ = index.handle_dead_keys(&[account_key], &secondary_indexes);
        assert!(secondary_index.index.is_empty());
        assert!(secondary_index.reverse_index.is_empty());
    }

    #[test]
    fn test_spl_token_mint_secondary_index() {
        let (key_start, key_end, secondary_indexes) = create_spl_token_mint_secondary_index_state();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        for token_id in &spl_token_ids() {
            run_test_spl_token_secondary_indexes(
                token_id,
                &index,
                &index.spl_token_mint_index,
                key_start,
                key_end,
                &secondary_indexes,
            );
        }
    }

    #[test]
    fn test_spl_token_owner_secondary_index() {
        let (key_start, key_end, secondary_indexes) =
            create_spl_token_owner_secondary_index_state();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        for token_id in &spl_token_ids() {
            run_test_spl_token_secondary_indexes(
                token_id,
                &index,
                &index.spl_token_owner_index,
                key_start,
                key_end,
                &secondary_indexes,
            );
        }
    }

    fn run_test_secondary_indexes_same_slot_and_forks<
        SecondaryIndexEntryType: SecondaryIndexEntry + Default + Sync + Send,
    >(
        token_id: &Pubkey,
        index: &AccountsIndex<bool, bool>,
        secondary_index: &SecondaryIndex<SecondaryIndexEntryType>,
        index_key_start: usize,
        index_key_end: usize,
        secondary_indexes: &AccountSecondaryIndexes,
    ) {
        let account_key = Pubkey::new_unique();
        let secondary_key1 = Pubkey::new_unique();
        let secondary_key2 = Pubkey::new_unique();
        let slot = 1;
        let mut account_data1 = make_empty_token_account_data();
        account_data1[index_key_start..index_key_end]
            .clone_from_slice(&(secondary_key1.to_bytes()));
        let mut account_data2 = make_empty_token_account_data();
        account_data2[index_key_start..index_key_end]
            .clone_from_slice(&(secondary_key2.to_bytes()));

        // First write one mint index
        index.upsert(
            slot,
            slot,
            &account_key,
            &AccountSharedData::create_from_existing_shared_data(
                0,
                Arc::new(account_data1.to_vec()),
                *token_id,
                false,
                0,
            ),
            secondary_indexes,
            true,
            &mut ReclaimsSlotList::new(),
            UPSERT_RECLAIM_TEST_DEFAULT,
        );

        // Now write a different mint index for the same account
        index.upsert(
            slot,
            slot,
            &account_key,
            &AccountSharedData::create_from_existing_shared_data(
                0,
                Arc::new(account_data2.to_vec()),
                *token_id,
                false,
                0,
            ),
            secondary_indexes,
            true,
            &mut ReclaimsSlotList::new(),
            UPSERT_RECLAIM_TEST_DEFAULT,
        );

        // Both pubkeys will now be present in the index
        check_secondary_index_mapping_correct(
            secondary_index,
            &[secondary_key1, secondary_key2],
            &account_key,
        );

        // If a later slot also introduces secondary_key1, then it should still exist in the index
        let later_slot = slot + 1;
        index.upsert(
            later_slot,
            later_slot,
            &account_key,
            &AccountSharedData::create_from_existing_shared_data(
                0,
                Arc::new(account_data1.to_vec()),
                *token_id,
                false,
                0,
            ),
            secondary_indexes,
            true,
            &mut ReclaimsSlotList::new(),
            UPSERT_RECLAIM_TEST_DEFAULT,
        );
        assert_eq!(secondary_index.get(&secondary_key1), vec![account_key]);

        // If we set a root at `later_slot`, and clean, then even though the account with secondary_key1
        // was outdated by the update in the later slot, the primary account key is still alive,
        // so both secondary keys will still be kept alive.
        index.add_root(later_slot);
        index.slot_list_mut(&account_key, |mut slot_list| {
            index.purge_older_root_entries(&mut slot_list, &mut ReclaimsSlotList::new(), None)
        });

        check_secondary_index_mapping_correct(
            secondary_index,
            &[secondary_key1, secondary_key2],
            &account_key,
        );

        // Removing the remaining entry for this pubkey in the index should mark the
        // pubkey as dead and finally remove all the secondary indexes
        let mut reclaims = ReclaimsSlotList::new();
        index.purge_exact(&account_key, later_slot, &mut reclaims);
        let _ = index.handle_dead_keys(&[account_key], secondary_indexes);
        assert!(secondary_index.index.is_empty());
        assert!(secondary_index.reverse_index.is_empty());
    }

    #[test]
    fn test_spl_token_mint_secondary_index_same_slot_and_forks() {
        let (key_start, key_end, account_index) = create_spl_token_mint_secondary_index_state();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        for token_id in &spl_token_ids() {
            run_test_secondary_indexes_same_slot_and_forks(
                token_id,
                &index,
                &index.spl_token_mint_index,
                key_start,
                key_end,
                &account_index,
            );
        }
    }

    #[test]
    fn test_rwlock_secondary_index_same_slot_and_forks() {
        let (key_start, key_end, account_index) = create_spl_token_owner_secondary_index_state();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        for token_id in &spl_token_ids() {
            run_test_secondary_indexes_same_slot_and_forks(
                token_id,
                &index,
                &index.spl_token_owner_index,
                key_start,
                key_end,
                &account_index,
            );
        }
    }

    impl IndexValue for bool {}
    impl IndexValue for u64 {}
    impl DiskIndexValue for bool {}
    impl DiskIndexValue for u64 {}
    impl IsCached for bool {
        fn is_cached(&self) -> bool {
            false
        }
    }
    impl IsCached for u64 {
        fn is_cached(&self) -> bool {
            false
        }
    }
    impl IsZeroLamport for bool {
        fn is_zero_lamport(&self) -> bool {
            false
        }
    }

    impl IsZeroLamport for u64 {
        fn is_zero_lamport(&self) -> bool {
            false
        }
    }

    /// Type that supports caching for tests. Used to test upsert behaviour
    /// when the slot list has mixed cached and uncached items.
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    struct CacheableIndexValueTest(bool);
    impl IndexValue for CacheableIndexValueTest {}
    impl DiskIndexValue for CacheableIndexValueTest {}
    impl IsCached for CacheableIndexValueTest {
        fn is_cached(&self) -> bool {
            // Return self value as whether the item is cached or not
            self.0
        }
    }
    impl IsZeroLamport for CacheableIndexValueTest {
        fn is_zero_lamport(&self) -> bool {
            false
        }
    }

    #[test]
    fn test_get_newest_root_in_slot_list() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let return_0 = 0;
        let slot1 = 1;
        let slot2 = 2;
        let slot99 = 99;

        // no roots, so always 0
        {
            let roots_tracker = &index.roots_tracker.read().unwrap();
            let slot_list = Vec::<(Slot, bool)>::default();
            assert_eq!(
                return_0,
                AccountsIndex::<bool, bool>::get_newest_root_in_slot_list(
                    &roots_tracker.alive_roots,
                    &slot_list,
                    Some(slot1),
                )
            );
            assert_eq!(
                return_0,
                AccountsIndex::<bool, bool>::get_newest_root_in_slot_list(
                    &roots_tracker.alive_roots,
                    &slot_list,
                    Some(slot2),
                )
            );
            assert_eq!(
                return_0,
                AccountsIndex::<bool, bool>::get_newest_root_in_slot_list(
                    &roots_tracker.alive_roots,
                    &slot_list,
                    Some(slot99),
                )
            );
        }

        index.add_root(slot2);

        {
            let roots_tracker = &index.roots_tracker.read().unwrap();
            let slot_list = vec![(slot2, true)];
            assert_eq!(
                slot2,
                AccountsIndex::<bool, bool>::get_newest_root_in_slot_list(
                    &roots_tracker.alive_roots,
                    &slot_list,
                    Some(slot2),
                )
            );
            // no newest root
            assert_eq!(
                return_0,
                AccountsIndex::<bool, bool>::get_newest_root_in_slot_list(
                    &roots_tracker.alive_roots,
                    &slot_list,
                    Some(slot1),
                )
            );
            assert_eq!(
                slot2,
                AccountsIndex::<bool, bool>::get_newest_root_in_slot_list(
                    &roots_tracker.alive_roots,
                    &slot_list,
                    Some(slot99),
                )
            );
        }
    }

    impl<T: IndexValue> AccountsIndex<T, T> {
        fn upsert_simple_test(&self, key: &Pubkey, slot: Slot, value: T) {
            let mut gc = ReclaimsSlotList::new();

            // It is invalid to reclaim older slots if the slot being upserted
            // is unrooted
            let reclaim_method = if self.is_alive_root(slot) {
                UPSERT_RECLAIM_TEST_DEFAULT
            } else {
                UpsertReclaim::IgnoreReclaims
            };

            self.upsert(
                slot,
                slot,
                key,
                &AccountSharedData::default(),
                &AccountSecondaryIndexes::default(),
                value,
                &mut gc,
                reclaim_method,
            );
            assert!(gc.is_empty());
        }

        pub fn clear_roots(&self) {
            self.roots_tracker.write().unwrap().alive_roots.clear()
        }
    }

    #[test]
    fn test_unref() {
        let value = true;
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let slot1 = 1;

        index.upsert_simple_test(&key, slot1, value);

        index.get_and_then(&key, |entry| {
            let entry = entry.unwrap();
            // check refcount BEFORE the unref
            assert_eq!(entry.ref_count(), 1);
            // first time, ref count was at 1, we can unref once. Unref should return 1.
            assert_eq!(entry.unref(), 1);
            // check refcount AFTER the unref
            assert_eq!(entry.ref_count(), 0);
            (false, ())
        });
    }

    #[test]
    #[should_panic(expected = "decremented ref count below zero")]
    fn test_illegal_unref() {
        let value = true;
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let slot1 = 1;

        index.upsert_simple_test(&key, slot1, value);

        index.get_and_then(&key, |entry| {
            let entry = entry.unwrap();
            // make ref count be zero
            assert_eq!(entry.unref(), 1);
            assert_eq!(entry.ref_count(), 0);

            // unref when already at zero should panic
            entry.unref();
            (false, ())
        });
    }

    #[test]
    fn test_clean_rooted_entries_return() {
        agave_logger::setup();
        let value = true;
        let key = solana_pubkey::new_rand();
        let key_unknown = solana_pubkey::new_rand();
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        let slot1 = 1;

        let mut gc = ReclaimsSlotList::new();
        // return true if we don't know anything about 'key_unknown'
        // the item did not exist in the accounts index at all, so index is up to date
        assert!(index.clean_rooted_entries(&key_unknown, &mut gc, None));

        index.upsert_simple_test(&key, slot1, value);

        let slot2 = 2;
        // none for max root because we don't want to delete the entry yet
        assert!(!index.clean_rooted_entries(&key, &mut gc, None));
        // this is because of inclusive vs exclusive in the call to can_purge_older_entries
        assert!(!index.clean_rooted_entries(&key, &mut gc, Some(slot1)));
        // this will delete the entry because it is <= max_root_inclusive and NOT a root
        // note this has to be slot2 because of inclusive vs exclusive in the call to can_purge_older_entries
        {
            let mut gc = ReclaimsSlotList::new();
            assert!(index.clean_rooted_entries(&key, &mut gc, Some(slot2)));
            assert_eq!(gc, ReclaimsSlotList::from([(slot1, value)]));
        }

        // re-add it
        index.upsert_simple_test(&key, slot1, value);

        index.add_root(slot1);
        assert!(!index.clean_rooted_entries(&key, &mut gc, Some(slot2)));
        index.upsert_simple_test(&key, slot2, value);

        index.get_and_then(&key, |entry| {
            let account_map_entry = entry.unwrap();
            let slot_list = account_map_entry.slot_list_read_lock();
            assert_eq!(2, slot_list.len());
            assert_eq!(&[(slot1, value), (slot2, value)], slot_list.as_ref());
            (false, ())
        });
        assert!(!index.clean_rooted_entries(&key, &mut gc, Some(slot2)));
        assert_eq!(
            2,
            index.get_and_then(&key, |entry| (
                false,
                entry.unwrap().slot_list_lock_read_len()
            ))
        );
        assert!(gc.is_empty());
        {
            {
                let roots_tracker = &index.roots_tracker.read().unwrap();
                let slot_list = SlotList::from([(slot2, value)]);
                assert_eq!(
                    0,
                    AccountsIndex::<bool, bool>::get_newest_root_in_slot_list(
                        &roots_tracker.alive_roots,
                        &slot_list,
                        None,
                    )
                );
            }
            index.add_root(slot2);
            {
                let roots_tracker = &index.roots_tracker.read().unwrap();
                let slot_list = SlotList::from([(slot2, value)]);
                assert_eq!(
                    slot2,
                    AccountsIndex::<bool, bool>::get_newest_root_in_slot_list(
                        &roots_tracker.alive_roots,
                        &slot_list,
                        None,
                    )
                );
                assert_eq!(
                    0,
                    AccountsIndex::<bool, bool>::get_newest_root_in_slot_list(
                        &roots_tracker.alive_roots,
                        &slot_list,
                        Some(0),
                    )
                );
            }
        }

        assert!(gc.is_empty());
        assert!(!index.clean_rooted_entries(&key, &mut gc, Some(slot2)));
        assert_eq!(gc, ReclaimsSlotList::from([(slot1, value)]));
        gc.clear();
        index.clean_dead_slot(slot2);
        let slot3 = 3;
        assert!(index.clean_rooted_entries(&key, &mut gc, Some(slot3)));
        assert_eq!(gc, ReclaimsSlotList::from([(slot2, value)]));
    }

    #[test]
    fn test_handle_dead_keys_return() {
        let key = solana_pubkey::new_rand();
        let index = AccountsIndex::<bool, bool>::default_for_tests();

        assert_eq!(
            index.handle_dead_keys(&[key], &AccountSecondaryIndexes::default()),
            vec![key].into_iter().collect::<HashSet<_>>()
        );
    }

    #[test]
    fn test_start_end_bin() {
        let index = AccountsIndex::<bool, bool>::default_for_tests();
        assert_eq!(index.bins(), BINS_FOR_TESTING);

        let range = (Unbounded::<Pubkey>, Unbounded);
        let (start, end) = index.bin_start_end_inclusive(&range);
        assert_eq!(start, 0); // no range, so 0
        assert_eq!(end, BINS_FOR_TESTING - 1); // no range, so last bin

        let key = Pubkey::from([0; 32]);
        let range = RangeInclusive::new(key, key);
        let (start, end) = index.bin_start_end_inclusive(&range);
        assert_eq!(start, 0); // start at pubkey 0, so 0
        assert_eq!(end, 0); // end at pubkey 0, so 0

        let range = (Included(key), Excluded(key));
        let (start, end) = index.bin_start_end_inclusive(&range);
        assert_eq!(start, 0); // start at pubkey 0, so 0
        assert_eq!(end, 0); // end at pubkey 0, so 0

        let range = (Excluded(key), Excluded(key));
        let (start, end) = index.bin_start_end_inclusive(&range);
        assert_eq!(start, 0); // start at pubkey 0, so 0
        assert_eq!(end, 0); // end at pubkey 0, so 0

        let key = Pubkey::from([0xff; 32]);
        let range = RangeInclusive::new(key, key);
        let (start, end) = index.bin_start_end_inclusive(&range);
        let bins = index.bins();
        assert_eq!(start, bins - 1); // start at highest possible pubkey, so bins - 1
        assert_eq!(end, bins - 1);

        let range = (Included(key), Excluded(key));
        let (start, end) = index.bin_start_end_inclusive(&range);
        assert_eq!(start, bins - 1); // start at highest possible pubkey, so bins - 1
        assert_eq!(end, bins - 1);

        let range = (Excluded(key), Excluded(key));
        let (start, end) = index.bin_start_end_inclusive(&range);
        assert_eq!(start, bins); // Exclude the highest possible pubkey, so start should be "bins"
        assert_eq!(end, bins - 1); // End should be the last bin index
    }

    #[test]
    #[should_panic(expected = "bins.is_power_of_two()")]
    #[allow(clippy::field_reassign_with_default)]
    fn test_illegal_bins() {
        let mut config = AccountsIndexConfig::default();
        config.bins = Some(3);
        AccountsIndex::<bool, bool>::new(&config, Arc::default());
    }

    #[test]
    fn test_scan_config() {
        for scan_order in [ScanOrder::Sorted, ScanOrder::Unsorted] {
            let config = ScanConfig::new(scan_order);
            assert_eq!(config.scan_order, scan_order);
            assert!(config.abort.is_none()); // not allocated
            assert!(!config.is_aborted());
            config.abort(); // has no effect
            assert!(!config.is_aborted());
        }

        let config = ScanConfig::new(ScanOrder::Sorted);
        assert_eq!(config.scan_order, ScanOrder::Sorted);
        assert!(config.abort.is_none());

        let config = ScanConfig::default();
        assert_eq!(config.scan_order, ScanOrder::Unsorted);
        assert!(config.abort.is_none());

        let config = config.recreate_with_abort();
        assert!(config.abort.is_some());
        assert!(!config.is_aborted());
        config.abort();
        assert!(config.is_aborted());

        let config = config.recreate_with_abort();
        assert!(config.is_aborted());
    }
}
