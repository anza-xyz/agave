//! Persistent accounts are stored at this path location:
//!  `<path>/<pid>/data/`
//!
//! The persistent store would allow for this mode of operation:
//!  - Concurrent single thread append with many concurrent readers.
//!
//! The underlying memory is memory mapped to a file. The accounts would be
//! stored across multiple files and the mappings of file and offset of a
//! particular account would be stored in a shared index. This will allow for
//! concurrent commits without blocking reads, which will sequentially write
//! to memory, ssd or disk, and should be as fast as the hardware allow for.
//! The only required in memory data structure with a write lock is the index,
//! which should be fast to update.
//!
//! [`AppendVec`]'s only store accounts for single slots.  To bootstrap the
//! index from a persistent store of [`AppendVec`]'s, the entries include
//! a "write_version".  A single global atomic `AccountsDb::write_version`
//! tracks the number of commits to the entire data store. So the latest
//! commit for each slot entry would be indexed.

mod accounts_db_config;
mod geyser_plugin_utils;
pub(crate) mod stats;
pub(crate) mod tests;

pub use accounts_db_config::{
    ACCOUNTS_DB_CONFIG_FOR_BENCHMARKS, ACCOUNTS_DB_CONFIG_FOR_TESTING, AccountsDbConfig,
};
#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::{
        account_info::{AccountInfo, Offset, StorageLocation},
        account_storage::{
            AccountStorage, AccountStoragesOrderer, ShrinkInProgress,
            stored_account_info::{StoredAccountInfo, StoredAccountInfoWithoutData},
        },
        account_storage_entry::AccountStorageEntry,
        accounts_cache::{AccountsCache, CachedAccount, SlotCache},
        accounts_db::stats::{
            AccountsStats, CleanAccountsStats, FlushStats, LoadAccountsStats,
            ObsoleteAccountsStats, PurgeStats, ShrinkAncientStats, ShrinkStats, ShrinkStatsSub,
            StoreAccountsForFlushStats, StoreAccountsForShrinkStats, StoreAccountsForSquashStats,
            StoreAccountsUnfrozenStats, WriteAccountsToCacheStats,
        },
        accounts_file::AccountsFileProvider,
        accounts_hash::{AccountLtHash, AccountsLtHash, ZERO_LAMPORT_ACCOUNT_LT_HASH},
        accounts_index::{
            AccountSecondaryIndexes, AccountsIndex, IndexKey, ReclaimsSlotList,
            ReclaimsWithNewestSlot, ScanFilter, Startup, UpsertReclaim,
            in_mem_accounts_index::StartupStats,
        },
        accounts_scan::{ScanConfig, ScanError, ScanGuard, ScanResult, ScanTracker},
        accounts_update_notifier_interface::{AccountForGeyser, AccountsUpdateNotifier},
        active_stats::{ActiveStatItem, ActiveStats},
        ancestors::Ancestors,
        append_vec::{self, AppendVec},
        contains::Contains,
        is_zero_lamport::IsZeroLamport,
        partitioned_rewards::PartitionedEpochRewardsConfig,
        read_only_accounts_cache::ReadOnlyAccountsCache,
        storable_accounts::{StorableAccounts, StorableAccountsBySlot},
        u64_align,
        utils::{self, create_account_shared_data},
    },
    agave_fs::buffered_reader::RequiredLenBufFileRead,
    ahash::{HashMapExt as _, HashSetExt as _},
    bv::BitVec,
    dashmap::DashMap,
    log::*,
    rand::{Rng, rng},
    rayon::{ThreadPool, prelude::*},
    seqlock::SeqLock,
    solana_account::{Account, AccountSharedData, ReadableAccount},
    solana_clock::{BankId, Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_lattice_hash::{
        batch,
        lt_hash::{LtHash, SingleLtHashUpdater},
    },
    solana_measure::{measure::Measure, measure_us},
    solana_nohash_hasher::{BuildNoHashHasher, IntMap, IntSet},
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    solana_rayon_threadlimit::get_thread_count,
    std::{
        borrow::Cow,
        boxed::Box,
        collections::{BTreeSet, HashSet, VecDeque},
        io, iter, mem,
        num::Saturating,
        ops::RangeBounds,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex, RwLock,
            atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        },
        thread,
        time::{Duration, Instant},
    },
    tempfile::TempDir,
};

// when the accounts write cache exceeds this many bytes, we will flush it
// this can be specified on the command line, too (--accounts-db-write-cache-limit)
const WRITE_CACHE_LIMIT_BYTES_DEFAULT: u64 = 15_000_000_000;
const SCAN_SLOT_PAR_ITER_THRESHOLD: usize = 4000;

const DEFAULT_NUM_DIRS: u32 = 4;

// This value reflects recommended memory lock limit documented in the validator's
// setup instructions at https://docs.anza.xyz/operations/guides/validator-start allowing use of
// several io_uring instances with fixed buffers for large disk IO operations.
pub const TOTAL_IO_URING_BUFFERS_SIZE_LIMIT: usize = 2_000_000_000;

// When getting accounts for shrinking from the index, this is the # of accounts to lookup per thread.
// This allows us to split up accounts index accesses across multiple threads.
const SHRINK_COLLECT_CHUNK_SIZE: usize = 50;

/// The number of shrink candidate slots that is small enough so that
/// additional storages from ancient slots can be added to the
/// candidates for shrinking.
const SHRINK_INSERT_ANCIENT_THRESHOLD: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanAccountStorageData {
    /// callback for accounts in storage will not include `data`
    // Note, currently only used in tests, but do not remove.
    #[cfg_attr(not(test), allow(dead_code))]
    NoData,
    /// return data (&[u8]) for each account.
    /// This can be expensive to get and is not necessary for many scan operations.
    DataRefForStorage,
}

#[derive(Default, Debug)]
/// hold alive accounts
/// alive means in the accounts index
pub(crate) struct AliveAccounts<'a> {
    /// slot the accounts are currently stored in
    pub(crate) slot: Slot,
    pub(crate) accounts: Vec<&'a AccountFromStorage>,
    pub(crate) bytes: usize,
}

/// separate alive accounts by whether a newer duplicate of the account exists
#[derive(Debug)]
pub(crate) struct AliveAccountsSeparated<'a> {
    /// can be packed into any slot
    pub(crate) no_duplicates: AliveAccounts<'a>,
    /// can only be packed into a slot >= this one
    pub(crate) newest_duplicate: AliveAccounts<'a>,
    /// must stay in this slot
    pub(crate) not_newest_duplicate: AliveAccounts<'a>,
}

pub(crate) trait ShrinkCollector<'a>: Sync + Send {
    fn with_capacity(capacity: usize, slot: Slot) -> Self;
    fn collect(&mut self, other: Self);
    fn add(&mut self, account: &'a AccountFromStorage, slot_list: &[(Slot, AccountInfo)]);
    fn len(&self) -> usize;
    fn alive_bytes(&self) -> usize;
    fn alive_accounts(&self) -> &Vec<&'a AccountFromStorage>;
}

impl<'a> ShrinkCollector<'a> for AliveAccounts<'a> {
    fn collect(&mut self, mut other: Self) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.accounts.append(&mut other.accounts);
    }
    fn with_capacity(capacity: usize, slot: Slot) -> Self {
        Self {
            accounts: Vec::with_capacity(capacity),
            bytes: 0,
            slot,
        }
    }
    fn add(&mut self, account: &'a AccountFromStorage, _slot_list: &[(Slot, AccountInfo)]) {
        self.accounts.push(account);
        self.bytes = self.bytes.saturating_add(account.stored_size());
    }
    fn len(&self) -> usize {
        self.accounts.len()
    }
    fn alive_bytes(&self) -> usize {
        self.bytes
    }
    fn alive_accounts(&self) -> &Vec<&'a AccountFromStorage> {
        &self.accounts
    }
}

impl<'a> ShrinkCollector<'a> for AliveAccountsSeparated<'a> {
    fn collect(&mut self, other: Self) {
        self.no_duplicates.collect(other.no_duplicates);
        self.newest_duplicate.collect(other.newest_duplicate);
        self.not_newest_duplicate
            .collect(other.not_newest_duplicate);
    }
    fn with_capacity(capacity: usize, slot: Slot) -> Self {
        Self {
            no_duplicates: AliveAccounts::with_capacity(capacity, slot),
            newest_duplicate: AliveAccounts::with_capacity(0, slot),
            not_newest_duplicate: AliveAccounts::with_capacity(0, slot),
        }
    }
    fn add(&mut self, account: &'a AccountFromStorage, slot_list: &[(Slot, AccountInfo)]) {
        assert!(!slot_list.is_empty());
        let slot = self.no_duplicates.slot;
        let other = if slot_list.len() == 1 {
            &mut self.no_duplicates
        } else if !slot_list
            .iter()
            .any(|(slot_list_slot, _info)| slot_list_slot > &slot)
        {
            // this entry is alive but is newer than any other slot in the index
            &mut self.newest_duplicate
        } else {
            // This entry is alive but is older than at least one other slot in the index.
            // We would expect clean to get rid of the entry for THIS slot at some point, but clean hasn't done that yet.
            &mut self.not_newest_duplicate
        };
        other.add(account, slot_list);
    }
    fn len(&self) -> usize {
        self.no_duplicates
            .len()
            .saturating_add(self.not_newest_duplicate.len())
            .saturating_add(self.newest_duplicate.len())
    }
    fn alive_bytes(&self) -> usize {
        self.no_duplicates
            .alive_bytes()
            .saturating_add(self.not_newest_duplicate.alive_bytes())
            .saturating_add(self.newest_duplicate.alive_bytes())
    }
    fn alive_accounts(&self) -> &Vec<&'a AccountFromStorage> {
        unimplemented!("illegal use");
    }
}

#[derive(Debug)]
pub(crate) struct ShrinkCollect<T> {
    pub(crate) slot: Slot,
    pub(crate) written_bytes: u64,
    pub(crate) alive_accounts: T,
    /// Tombstones carried forward into the new storage because they are not yet purgeable.
    pub(crate) tombstones_to_carry_forward: Vec<AccountFromStorage>,
    /// total size in storage of all accounts in `tombstones_to_carry_forward`
    pub(crate) tombstones_total_bytes: usize,
    /// total size in storage of all alive accounts
    pub(crate) alive_total_bytes: usize,
    pub(crate) total_starting_accounts: usize,
}

struct LoadAccountsIndexForShrink<T> {
    /// all alive accounts
    alive_accounts: T,
}

/// reference an account found during scanning a storage.
#[derive(Debug, PartialEq, Copy, Clone)]
pub struct AccountFromStorage {
    pub index_info: AccountInfo,
    pub data_len: u64,
    pub pubkey: Pubkey,
}

impl IsZeroLamport for AccountFromStorage {
    fn is_zero_lamport(&self) -> bool {
        self.index_info.is_zero_lamport()
    }
}

impl AccountFromStorage {
    pub fn pubkey(&self) -> &Pubkey {
        &self.pubkey
    }
    pub fn stored_size(&self) -> usize {
        AppendVec::calculate_stored_size(self.data_len as usize)
    }
    pub fn data_len(&self) -> usize {
        self.data_len as usize
    }
    #[cfg(test)]
    pub(crate) fn new(offset: Offset, account: &StoredAccountInfoWithoutData) -> Self {
        // the id is irrelevant in this account info. This structure is only used DURING shrink operations.
        // In those cases, there is only 1 append vec id per slot when we read the accounts.
        // Any value of storage id in account info works fine when we want the 'normal' storage.
        let storage_id = 0;
        AccountFromStorage {
            index_info: AccountInfo::new(
                StorageLocation::AccountsFile(storage_id, offset),
                account.is_zero_lamport(),
            ),
            pubkey: *account.pubkey(),
            data_len: account.data_len as u64,
        }
    }
}

pub struct GetUniqueAccountsResult {
    pub stored_accounts: Vec<AccountFromStorage>,
    pub written_bytes: u64,
}

pub struct AccountsAddRootTiming {
    pub cache_us: u64,
}

/// Slots older the "number of slots in an epoch minus this number"
/// than max root are treated as ancient and subject to packing.
/// |  older  |<-          slots in an epoch          ->| max root
/// |  older  |<-    offset   ->|                       |
/// |          ancient          |        modern         |
///
/// If this is negative, this many slots older than the number of
/// slots in epoch are still treated as modern (ie. non-ancient).
/// |  older  |<- abs(offset) ->|<- slots in an epoch ->| max root
/// | ancient |                 modern                  |
///
/// Note that another constant DEFAULT_MAX_ANCIENT_STORAGES sets a
/// threshold for combining ancient storages so that their overall
/// number is under a certain limit, whereas this constant establishes
/// the distance from the max root slot beyond which storages holding
/// the account data for the slots are considered ancient by the
/// shrinking algorithm.
const ANCIENT_APPEND_VEC_DEFAULT_OFFSET: Option<i64> = Some(100_000);
/// The smallest size of ideal ancient storage.
/// The setting can be overridden on the command line
/// with --accounts-db-ancient-ideal-storage-size option.
const DEFAULT_ANCIENT_STORAGE_IDEAL_SIZE: u64 = 100_000;
/// Default value for the number of ancient storages the ancient slot
/// combining should converge to.
pub const DEFAULT_MAX_ANCIENT_STORAGES: usize = 100_000;

#[cfg(not(test))]
const ABSURD_CONSECUTIVE_FAILED_ITERATIONS: usize = 100;

#[derive(Debug, Clone, Copy)]
pub enum AccountShrinkThreshold {
    /// Measure the total space sparseness across all candidates
    /// And select the candidates by using the top sparse account storage entries to shrink.
    /// The value is the overall shrink threshold measured as ratio of the total live bytes
    /// over the total bytes.
    TotalSpace { shrink_ratio: f64 },
    /// Use the following option to shrink all stores whose alive ratio is below
    /// the specified threshold.
    IndividualStore { shrink_ratio: f64 },
}
pub const DEFAULT_ACCOUNTS_SHRINK_OPTIMIZE_TOTAL_SPACE: bool = true;
pub const DEFAULT_ACCOUNTS_SHRINK_RATIO: f64 = 0.80;
// The default extra account space in percentage from the ideal target
const DEFAULT_ACCOUNTS_SHRINK_THRESHOLD_OPTION: AccountShrinkThreshold =
    AccountShrinkThreshold::TotalSpace {
        shrink_ratio: DEFAULT_ACCOUNTS_SHRINK_RATIO,
    };

impl Default for AccountShrinkThreshold {
    fn default() -> AccountShrinkThreshold {
        DEFAULT_ACCOUNTS_SHRINK_THRESHOLD_OPTION
    }
}

pub enum ScanStorageResult<R, B> {
    Cached(Vec<R>),
    Stored(B),
}

#[derive(Debug)]
pub struct IndexGenerationInfo {
    pub accounts_data_len: u64,
    /// The accounts lt hash calculated during index generation.
    /// Will be used when verifying accounts, after rebuilding a Bank.
    pub calculated_accounts_lt_hash: AccountsLtHash,
    /// The capitalization, in lamports, calculated during index generation.
    pub calculated_capitalization: u64,
}

/// Accumulator for the values produced while generating the index
#[derive(Debug)]
struct IndexGenerationAccumulator {
    insert_time_us: u64,
    num_accounts: u64,
    accounts_data_len: u64,
    all_accounts_are_zero_lamports_slots: u64,
    /// List of slots with only zero lamports accounts and indices into `storages` used in `generate_index`
    slots_with_only_zero_lamport_accounts: Vec<(Slot, usize)>,
    storage_info: StorageSizeAndCountList,
    /// Number of accounts in this slot that didn't already exist in the index
    num_did_not_exist: u64,
    /// Number of accounts in this slot that already existed, and were in-mem
    num_existed_in_mem: u64,
    /// Number of accounts in this slot that already existed, and were on-disk
    num_existed_on_disk: u64,
    /// The accounts lt hash for the set of accounts processed using this accumulator
    lt_hash_acc: batch::Accumulator,
    /// The capitalization for the set of accounts processed using this accumulator.
    /// Needs to be u128 as it may temporarily overflow u64 due to
    /// all duplicates being summed before being removed.
    capitalization: u128,
    /// The number of accounts in this slot that were skipped when generating the index as they
    /// were already marked obsolete in the account storage entry
    num_obsolete_accounts_skipped: u64,
    /// The number of zero-lamport pubkeys found in this slot
    num_zero_lamport_pubkeys: u64,
    slot_arena: IndexGenerationSlotArena,
}
impl IndexGenerationAccumulator {
    fn with_slots_capacity(num_slots: usize) -> Self {
        Self {
            insert_time_us: 0,
            num_accounts: 0,
            accounts_data_len: 0,
            all_accounts_are_zero_lamports_slots: 0,
            slots_with_only_zero_lamport_accounts: Vec::new(),
            storage_info: Vec::with_capacity(num_slots),
            num_did_not_exist: 0,
            num_existed_in_mem: 0,
            num_existed_on_disk: 0,
            lt_hash_acc: batch::Accumulator::new(),
            capitalization: 0,
            num_obsolete_accounts_skipped: 0,
            num_zero_lamport_pubkeys: 0,
            slot_arena: IndexGenerationSlotArena::default(),
        }
    }
    fn accumulate(&mut self, mut other: Self) {
        self.insert_time_us += other.insert_time_us;
        self.num_accounts += other.num_accounts;
        self.accounts_data_len += other.accounts_data_len;
        self.all_accounts_are_zero_lamports_slots += other.all_accounts_are_zero_lamports_slots;
        self.slots_with_only_zero_lamport_accounts
            .append(&mut other.slots_with_only_zero_lamport_accounts);
        self.num_did_not_exist += other.num_did_not_exist;
        self.num_existed_in_mem += other.num_existed_in_mem;
        self.num_existed_on_disk += other.num_existed_on_disk;
        self.lt_hash_acc.mix_in(&other.lt_hash_acc.into_lt_hash());
        self.capitalization = self
            .capitalization
            .checked_add(other.capitalization)
            .expect("capitalization cannot overflow");
        self.num_obsolete_accounts_skipped += other.num_obsolete_accounts_skipped;
        self.num_zero_lamport_pubkeys += other.num_zero_lamport_pubkeys;
        self.storage_info.append(&mut other.storage_info);
    }
}

/// Auxiliary state populated and emptied per slot within `generate_index_for_slot`
///
/// Holds allocated memory across run of index generation thread for performance.
#[derive(Debug, Default)]
struct IndexGenerationSlotArena {
    keyed_account_infos: Vec<(Pubkey, AccountInfo)>,
}

impl IndexGenerationSlotArena {
    /// Makes sure no actual items are stored in the allocated data structures
    fn ensure_empty(&mut self) {
        assert!(self.keyed_account_infos.is_empty(), "should be drained");
    }
}

/// The lt hash of old/duplicate accounts
///
/// Accumulation of all the duplicate accounts found during index generation.
/// These accounts need to have their lt hashes mixed *out*.
/// This is the final value, that when applied to all the storages at startup,
/// will produce the correct accounts lt hash.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DuplicatesLtHash(pub LtHash);

impl Default for DuplicatesLtHash {
    fn default() -> Self {
        Self(LtHash::identity())
    }
}

#[derive(Default, Debug)]
struct GenerateIndexTimings {
    pub total_time_us: u64,
    pub index_time: u64,
    pub insertion_time_us: u64,
    pub storage_size_storages_us: u64,
    pub index_flush_us: u64,
    pub total_including_duplicates: u64,
    pub visit_duplicate_accounts_time_us: u64,
    pub total_duplicate_slot_keys: u64,
    pub total_num_unique_duplicate_keys: u64,
    pub num_duplicate_accounts: u64,
    pub populate_duplicate_keys_us: u64,
    pub total_slots: u64,
    pub all_accounts_are_zero_lamports_slots: u64,
    pub mark_obsolete_accounts_us: u64,
    pub num_obsolete_accounts_marked: u64,
    pub num_slots_removed_as_obsolete: u64,
    pub num_obsolete_accounts_skipped: u64,
    pub num_zero_lamport_pubkeys: u64,
}

#[derive(Default, Debug, PartialEq, Eq)]
struct StorageSizeAndCount {
    /// total size stored, including both alive and dead bytes
    pub stored_size: usize,
    /// number of accounts in the storage including both alive and dead accounts
    pub count: usize,
}
type StorageSizeAndCountList = Vec<(AccountsFileId, StorageSizeAndCount)>;

impl GenerateIndexTimings {
    pub fn report(&self, startup_stats: &StartupStats) {
        datapoint_info!(
            "generate_index",
            ("overall_us", self.total_time_us, i64),
            ("index_time_us", self.index_time, i64),
            // we cannot accurately measure index insertion time because of many threads and lock contention
            ("insertion_time_us", self.insertion_time_us, i64),
            (
                "storage_size_storages_us",
                self.storage_size_storages_us,
                i64
            ),
            ("index_flush_us", self.index_flush_us, i64),
            (
                "total_items_including_duplicates",
                self.total_including_duplicates,
                i64
            ),
            (
                "visit_duplicate_accounts_us",
                self.visit_duplicate_accounts_time_us,
                i64
            ),
            (
                "total_duplicate_slot_keys",
                self.total_duplicate_slot_keys,
                i64
            ),
            (
                "total_num_unique_duplicate_keys",
                self.total_num_unique_duplicate_keys,
                i64
            ),
            ("num_duplicate_accounts", self.num_duplicate_accounts, i64),
            (
                "populate_duplicate_keys_us",
                self.populate_duplicate_keys_us,
                i64
            ),
            ("total_slots", self.total_slots, i64),
            (
                "copy_data_us",
                startup_stats.copy_data_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "all_accounts_are_zero_lamports_slots",
                self.all_accounts_are_zero_lamports_slots,
                i64
            ),
            (
                "mark_obsolete_accounts_us",
                self.mark_obsolete_accounts_us,
                i64
            ),
            (
                "num_obsolete_accounts_marked",
                self.num_obsolete_accounts_marked,
                i64
            ),
            (
                "num_slots_removed_as_obsolete",
                self.num_slots_removed_as_obsolete,
                i64
            ),
            (
                "num_obsolete_accounts_skipped",
                self.num_obsolete_accounts_skipped,
                i64
            ),
            (
                "num_zero_lamport_pubkeys",
                self.num_zero_lamport_pubkeys,
                i64
            ),
        );
    }
}

impl IsZeroLamport for AccountSharedData {
    fn is_zero_lamport(&self) -> bool {
        self.lamports() == 0
    }
}

impl IsZeroLamport for Account {
    fn is_zero_lamport(&self) -> bool {
        self.lamports() == 0
    }
}

/// An offset into the AccountsDb::storage vector
pub type AtomicAccountsFileId = AtomicU32;
pub type AccountsFileId = u32;

type SlotOffsets = IntMap<Slot, IntSet<Offset>>;
type ShrinkCandidates = IntSet<Slot>;

// Some hints for applicability of additional sanity checks for the do_load fast-path;
// Slower fallback code path will be taken if the fast path has failed over the retry
// threshold, regardless of these hints. Also, load cannot fail not-deterministically
// even under very rare circumstances, unlike previously did allow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadHint {
    // Caller hints that it's loading transactions for a block which is
    // descended from the current root, and at the tip of its fork.
    // Thereby, further this assumes AccountIndex::max_root should not increase
    // during this load, meaning there should be no squash.
    // Overall, this enables us to assert!() strictly while running the fast-path for
    // account loading, while maintaining the determinism of account loading and resultant
    // transaction execution thereof.
    FixedMaxRoot,
    // Caller can't hint the above safety assumption. Generally RPC and miscellaneous
    // other call-site falls into this category. The likelihood of slower path is slightly
    // increased as well.
    Unspecified,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PopulateReadCache {
    /// If the account is found in storage, populate the read cache with the loaded account
    True,
    /// Do not populate the read cache with loaded accounts
    False,
}

#[derive(Debug)]
pub enum LoadedAccountAccessor {
    // StoredAccountInfo can't be held directly here due to its lifetime dependency on
    // AccountStorageEntry
    Stored(Option<(Arc<AccountStorageEntry>, Offset)>),
}

impl LoadedAccountAccessor {
    fn check_and_get_loaded_account_shared_data(
        &mut self,
        load_filter: Option<impl Fn(u64, &Pubkey, usize) -> bool>,
    ) -> Option<AccountSharedData> {
        // all of these following .expect() and .unwrap() are like serious logic errors,
        // ideal for representing this as rust type system....

        match self {
            LoadedAccountAccessor::Stored(Some((maybe_storage_entry, offset))) => {
                // If we do find the storage entry, we can guarantee that the storage entry is
                // safe to read from because we grabbed a reference to the storage entry while it
                // was still in the storage map. This means even if the storage entry is removed
                // from the storage map after we grabbed the storage entry, the recycler should not
                // reset the storage entry until we drop the reference to the storage entry.

                // If there's a load filter, read only the account metadata first.
                // This way we don't read the whole account (including its data)
                // from disk, only to discard it later.
                let should_load_account = load_filter.is_none_or(|load_filter| {
                    maybe_storage_entry
                        .accounts
                        .get_stored_account_without_data_callback(*offset, |account| {
                            load_filter(account.lamports, account.owner, account.data_len)
                        })
                        .expect(
                            "If a storage entry was found in the storage map, it must not have \
                             been reset yet",
                        )
                });

                should_load_account.then(|| {
                    maybe_storage_entry
                        .accounts
                        .get_account_shared_data(*offset)
                        .expect(
                            "If a storage entry was found in the storage map, it must not have \
                             been reset yet",
                        )
                })
            }

            // It is safe ("""safe""") to skip consulting `load_filter` here because this
            // branch immediately and invariably panics.
            LoadedAccountAccessor::Stored(None) => {
                let account = self
                    .check_and_get_loaded_account(|loaded_account| loaded_account.take_account());
                unreachable!("{account:?}");
            }
        }
    }

    fn check_and_get_loaded_account<T>(
        &mut self,
        callback: impl for<'local> FnMut(LoadedAccount<'local>) -> T,
    ) -> T {
        // all of these following .expect() and .unwrap() are like serious logic errors,
        // ideal for representing this as rust type system....

        match self {
            LoadedAccountAccessor::Stored(None) => {
                panic!(
                    "Should have already been taken care of when creating this \
                     LoadedAccountAccessor"
                );
            }
            LoadedAccountAccessor::Stored(Some(_maybe_storage_entry)) => {
                // If we do find the storage entry, we can guarantee that the storage entry is
                // safe to read from because we grabbed a reference to the storage entry while it
                // was still in the storage map. This means even if the storage entry is removed
                // from the storage map after we grabbed the storage entry, the recycler should not
                // reset the storage entry until we drop the reference to the storage entry.
                self.get_loaded_account(callback).expect(
                    "If a storage entry was found in the storage map, it must not have been reset \
                     yet",
                )
            }
        }
    }

    fn get_loaded_account<T>(
        &mut self,
        mut callback: impl for<'local> FnMut(LoadedAccount<'local>) -> T,
    ) -> Option<T> {
        match self {
            LoadedAccountAccessor::Stored(maybe_storage_entry) => {
                // storage entry may not be present if slot was cleaned up in
                // between reading the accounts index and calling this function to
                // get account meta from the storage entry here
                maybe_storage_entry
                    .as_ref()
                    .and_then(|(storage_entry, offset)| {
                        storage_entry
                            .accounts
                            .get_stored_account_callback(*offset, |account| {
                                callback(LoadedAccount::Stored(account))
                            })
                    })
            }
        }
    }
}

pub enum LoadedAccount<'a> {
    Stored(StoredAccountInfo<'a>),
    Cached(Cow<'a, Arc<CachedAccount>>),
}

impl LoadedAccount<'_> {
    pub fn pubkey(&self) -> &Pubkey {
        match self {
            LoadedAccount::Stored(stored_account) => stored_account.pubkey(),
            LoadedAccount::Cached(cached_account) => cached_account.pubkey(),
        }
    }

    pub fn take_account(&self) -> AccountSharedData {
        match self {
            LoadedAccount::Stored(stored_account) => create_account_shared_data(stored_account),
            LoadedAccount::Cached(cached_account) => match cached_account {
                Cow::Owned(cached_account) => cached_account.account.clone(),
                Cow::Borrowed(cached_account) => cached_account.account.clone(),
            },
        }
    }

    pub fn is_cached(&self) -> bool {
        match self {
            LoadedAccount::Stored(_) => false,
            LoadedAccount::Cached(_) => true,
        }
    }

    /// data_len can be calculated without having access to `&data` in future implementations
    pub fn data_len(&self) -> usize {
        self.data().len()
    }
}

impl ReadableAccount for LoadedAccount<'_> {
    fn lamports(&self) -> u64 {
        match self {
            LoadedAccount::Stored(stored_account) => stored_account.lamports(),
            LoadedAccount::Cached(cached_account) => cached_account.account.lamports(),
        }
    }
    fn data(&self) -> &[u8] {
        match self {
            LoadedAccount::Stored(stored_account) => stored_account.data(),
            LoadedAccount::Cached(cached_account) => cached_account.account.data(),
        }
    }
    fn owner(&self) -> &Pubkey {
        match self {
            LoadedAccount::Stored(stored_account) => stored_account.owner(),
            LoadedAccount::Cached(cached_account) => cached_account.account.owner(),
        }
    }
    fn executable(&self) -> bool {
        match self {
            LoadedAccount::Stored(stored_account) => stored_account.executable(),
            LoadedAccount::Cached(cached_account) => cached_account.account.executable(),
        }
    }
    fn rent_epoch(&self) -> Epoch {
        match self {
            LoadedAccount::Stored(stored_account) => stored_account.rent_epoch(),
            LoadedAccount::Cached(cached_account) => cached_account.account.rent_epoch(),
        }
    }
}

#[derive(Default)]
struct CleanKeyTimings {
    collect_delta_keys_us: u64,
    zero_lamport_single_ref_slots_added_to_shrink_count: u64,
    zero_lamport_sweep_us: u64,
}

pub fn get_temp_accounts_paths(count: u32) -> io::Result<(Vec<TempDir>, Vec<PathBuf>)> {
    let temp_dirs: io::Result<Vec<TempDir>> = (0..count).map(|_| TempDir::new()).collect();
    let temp_dirs = temp_dirs?;

    let paths: io::Result<Vec<_>> = temp_dirs
        .iter()
        .map(|temp_dir| {
            utils::create_accounts_run_and_snapshot_dirs(temp_dir)
                .map(|(run_dir, _snapshot_dir)| run_dir)
        })
        .collect();
    let paths = paths?;
    Ok((temp_dirs, paths))
}

/// One accounts index bin's worth of pubkeys that are candidates for cleaning
type CleaningCandidatesBin = HashSet<Pubkey, PubkeyHasherBuilder>;
/// This is the return type of AccountsDb::construct_candidate_clean_keys.
/// It's a collection of pubkeys that are candidates for cleaning
type CleaningCandidates = Box<[RwLock<CleaningCandidatesBin>]>;
type AccountInfoAccountsIndex = AccountsIndex<AccountInfo, AccountInfo>;

// This structure handles the load/store of the accounts
#[derive(Debug)]
pub struct AccountsDb {
    /// Keeps tracks of index into AppendVec on a per slot basis
    pub accounts_index: AccountInfoAccountsIndex,

    /// Some(offset) iff we want to squash old append vecs together into 'ancient append vecs'
    /// Some(offset) means for slots up to (max_slot - (slots_per_epoch - 'offset')), put them in ancient append vecs
    pub ancient_append_vec_offset: Option<i64>,
    pub ancient_storage_ideal_size: u64,
    pub max_ancient_storages: usize,
    /// true iff we want to skip the initial hash calculation on startup
    pub skip_initial_hash_calc: bool,

    pub storage: AccountStorage,

    pub accounts_cache: AccountsCache,

    write_cache_limit_bytes: Option<u64>,

    read_only_accounts_cache: ReadOnlyAccountsCache,

    /// distribute the accounts across storage lists
    pub next_id: AtomicAccountsFileId,

    /// Set of shrinkable stores organized by map of slot to storage id
    pub shrink_candidate_slots: Mutex<ShrinkCandidates>,

    pub write_version: AtomicU64,

    /// Set of storage paths to pick from
    pub paths: Vec<PathBuf>,

    /// directory for bank hash details files
    bank_hash_details_dir: PathBuf,

    /// Directory of paths this accounts_db needs to hold/remove
    #[allow(dead_code)]
    pub temp_paths: Option<Vec<TempDir>>,

    /// Thread pool for foreground tasks, e.g. transaction processing
    pub thread_pool_foreground: ThreadPool,
    /// Thread pool for background tasks, e.g. AccountsBackgroundService and flush/clean/shrink
    pub thread_pool_background: ThreadPool,

    pub stats: AccountsStats,

    /// Stats for loading accounts during transaction processing
    load_account_stats: LoadAccountsStats,

    /// Stats from storing accounts unfrozen
    store_accounts_unfrozen_stats: StoreAccountsUnfrozenStats,

    clean_accounts_stats: CleanAccountsStats,

    // Stats for purges called outside of clean_accounts()
    external_purge_slots_stats: PurgeStats,

    pub shrink_stats: ShrinkStats,

    pub(crate) shrink_ancient_stats: ShrinkAncientStats,

    pub scan_tracker: ScanTracker,

    pub account_indexes: AccountSecondaryIndexes,

    /// Set of unique keys per slot which is used
    /// to drive clean_accounts
    /// Populated when flushing the accounts write cache
    uncleaned_pubkeys: DashMap<Slot, Vec<Pubkey>, BuildNoHashHasher<Slot>>,

    /// Pubkeys of the slots purged from the write cache, one entry per purged slot, waiting to be
    /// cleaned
    pubkeys_removed_from_cache: Mutex<Vec<Vec<Pubkey>>>,

    #[cfg(test)]
    load_delay: u64,

    #[cfg(test)]
    load_limit: AtomicU64,

    /// true if drop_callback is attached to the bank.
    is_bank_drop_callback_enabled: AtomicBool,

    shrink_ratio: AccountShrinkThreshold,

    /// Set by `set_latest_full_snapshot_slot` when the snapshot advances. Read and cleared by
    /// clean
    latest_full_snapshot_slot_advanced_since_clean: AtomicBool,

    /// GeyserPlugin accounts update notifier
    accounts_update_notifier: Option<AccountsUpdateNotifier>,

    pub(crate) active_stats: ActiveStats,

    /// debug feature to scan every storage and verify the index matches
    verify_index: bool,

    /// storage format to use for new storages
    accounts_file_provider: AccountsFileProvider,

    /// index scan filtering for shrinking
    scan_filter_for_shrinking: ScanFilter,

    /// this will live here until the feature for partitioned epoch rewards is activated.
    /// At that point, this and other code can be deleted.
    pub partitioned_epoch_rewards_config: PartitionedEpochRewardsConfig,

    /// The latest full snapshot slot dictates how to handle zero lamport accounts
    /// Note, this is None if we're told to *not* take snapshots
    latest_full_snapshot_slot: SeqLock<Option<Slot>>,

    /// The full snapshot slot we last swept for zero-lamport-single-ref shrink
    /// eligibility.
    last_swept_full_snapshot_slot: AtomicU64,

    /// These are the ancient storages that could be valuable to
    /// shrink, sorted by amount of dead bytes.  The elements
    /// are sorted from the largest dead bytes to the smallest.
    /// Members are Slot and capacity. If capacity is smaller, then
    /// that means the storage was already shrunk.
    pub(crate) best_ancient_slots_to_shrink: RwLock<VecDeque<(Slot, u64)>>,

    /// The largest slot that has been added as a root via `add_root`.
    max_root: AtomicU64,
}

pub fn quarter_thread_count() -> usize {
    std::cmp::max(2, num_cpus::get() / 4)
}

pub fn default_num_foreground_threads() -> usize {
    get_thread_count()
}

impl AccountsDb {
    // The default high and low watermark sizes for the accounts read cache.
    // If the cache size exceeds MAX_SIZE_HI, it'll evict entries until the size is <= MAX_SIZE_LO.
    //
    // These default values were chosen empirically to minimize evictions on mainnet-beta.
    // As of 2025-08-15 on mainnet-beta, the read cache size's steady state is around 2.5 GB,
    // and add a bit more to buffer future growth.
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    const DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_LO: usize = 3_000_000_000;
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    const DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_HI: usize = 3_100_000_000;

    // See AccountsDbConfig::read_cache_evict_sample_size.
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    const DEFAULT_READ_ONLY_CACHE_EVICT_SAMPLE_SIZE: usize = 8;

    /// Number of read-only cache shards. Using 2^16 (65 536) shards keeps the
    /// count a power of two and roughly matches the number of cached accounts
    /// we observe on mainnet-beta. The average load is still ~1 account per
    /// shard (collisions are common), but compared with the default
    /// `num_cpus * 4` shards - where we saw hot shards carrying ~200
    /// accounts - this dramatically lowers contention.
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    const DEFAULT_READ_ONLY_CACHE_NUM_SHARDS: usize = 65536;

    pub fn new_with_config(
        paths: Vec<PathBuf>,
        accounts_db_config: AccountsDbConfig,
        accounts_update_notifier: Option<AccountsUpdateNotifier>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let accounts_index_config = accounts_db_config.index.unwrap_or_default();
        let accounts_index = AccountsIndex::new(&accounts_index_config, exit);

        let (paths, temp_paths) = if paths.is_empty() {
            // Create a temporary set of accounts directories, used primarily
            // for testing
            let (temp_dirs, temp_paths) = get_temp_accounts_paths(DEFAULT_NUM_DIRS).unwrap();
            (temp_paths, Some(temp_dirs))
        } else {
            (paths, None)
        };

        let read_cache_size = accounts_db_config.read_cache_limit_bytes.unwrap_or((
            Self::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_LO,
            Self::DEFAULT_MAX_READ_ONLY_CACHE_DATA_SIZE_HI,
        ));
        let read_cache_evict_sample_size = accounts_db_config
            .read_cache_evict_sample_size
            .unwrap_or(Self::DEFAULT_READ_ONLY_CACHE_EVICT_SAMPLE_SIZE);
        let read_cache_num_shards = accounts_db_config
            .read_cache_num_shards
            .unwrap_or(Self::DEFAULT_READ_ONLY_CACHE_NUM_SHARDS);

        // Increase the stack for foreground threads
        // rayon needs a lot of stack
        const ACCOUNTS_STACK_SIZE: usize = 8 * 1024 * 1024;
        let num_foreground_threads = accounts_db_config
            .num_foreground_threads
            .map(Into::into)
            .unwrap_or_else(default_num_foreground_threads);
        let thread_pool_foreground = rayon::ThreadPoolBuilder::new()
            .num_threads(num_foreground_threads)
            .thread_name(|i| format!("solAcctsDbFg{i:02}"))
            .stack_size(ACCOUNTS_STACK_SIZE)
            .build()
            .expect("new rayon threadpool");

        let num_background_threads = accounts_db_config
            .num_background_threads
            .map(Into::into)
            .unwrap_or_else(quarter_thread_count);
        let thread_pool_background = rayon::ThreadPoolBuilder::new()
            .thread_name(|i| format!("solAcctsDbBg{i:02}"))
            .num_threads(num_background_threads)
            .build()
            .expect("new rayon threadpool");

        let new = Self {
            accounts_index,
            paths,
            bank_hash_details_dir: accounts_db_config.bank_hash_details_dir,
            temp_paths,
            skip_initial_hash_calc: accounts_db_config.skip_initial_hash_calc,
            ancient_append_vec_offset: accounts_db_config
                .ancient_append_vec_offset
                .or(ANCIENT_APPEND_VEC_DEFAULT_OFFSET),
            ancient_storage_ideal_size: accounts_db_config
                .ancient_storage_ideal_size
                .unwrap_or(DEFAULT_ANCIENT_STORAGE_IDEAL_SIZE),
            max_ancient_storages: accounts_db_config
                .max_ancient_storages
                .unwrap_or(DEFAULT_MAX_ANCIENT_STORAGES),
            scan_tracker: ScanTracker::default(),
            account_indexes: accounts_db_config.account_indexes.unwrap_or_default(),
            shrink_ratio: accounts_db_config.shrink_ratio,
            accounts_update_notifier,
            read_only_accounts_cache: ReadOnlyAccountsCache::new(
                read_cache_size.0,
                read_cache_size.1,
                read_cache_evict_sample_size,
                read_cache_num_shards,
            ),
            write_cache_limit_bytes: accounts_db_config.write_cache_limit_bytes,
            partitioned_epoch_rewards_config: accounts_db_config.partitioned_epoch_rewards_config,
            verify_index: accounts_db_config.verify_index,
            scan_filter_for_shrinking: accounts_db_config.scan_filter_for_shrinking,
            thread_pool_foreground,
            thread_pool_background,
            active_stats: ActiveStats::default(),
            storage: AccountStorage::default(),
            accounts_cache: AccountsCache::default(),
            uncleaned_pubkeys: DashMap::default(),
            pubkeys_removed_from_cache: Mutex::default(),
            next_id: AtomicAccountsFileId::new(0),
            shrink_candidate_slots: Mutex::new(ShrinkCandidates::default()),
            write_version: AtomicU64::new(0),
            external_purge_slots_stats: PurgeStats::default(),
            clean_accounts_stats: CleanAccountsStats::default(),
            shrink_stats: ShrinkStats::default(),
            shrink_ancient_stats: ShrinkAncientStats::default(),
            stats: AccountsStats::default(),
            load_account_stats: LoadAccountsStats::default(),
            store_accounts_unfrozen_stats: StoreAccountsUnfrozenStats::default(),
            #[cfg(test)]
            load_delay: u64::default(),
            #[cfg(test)]
            load_limit: AtomicU64::default(),
            is_bank_drop_callback_enabled: AtomicBool::default(),
            latest_full_snapshot_slot_advanced_since_clean: AtomicBool::default(),
            accounts_file_provider: accounts_db_config.accounts_file_provider,
            latest_full_snapshot_slot: SeqLock::new(None),
            last_swept_full_snapshot_slot: AtomicU64::new(0),
            best_ancient_slots_to_shrink: RwLock::default(),
            max_root: AtomicU64::new(0),
        };

        {
            for path in new.paths.iter() {
                std::fs::create_dir_all(path).expect("Create directory failed.");
            }
        }
        new
    }

    pub fn bank_hash_details_dir(&self) -> &Path {
        &self.bank_hash_details_dir
    }

    /// Returns true if there is an accounts update notifier.
    pub fn has_accounts_update_notifier(&self) -> bool {
        self.accounts_update_notifier.is_some()
    }

    fn next_id(&self) -> AccountsFileId {
        let next_id = self.next_id.fetch_add(1, Ordering::AcqRel);
        assert!(
            next_id != AccountsFileId::MAX,
            "We've run out of storage ids!"
        );
        next_id
    }

    /// While scanning cleaning candidates obtain slots that can be
    /// reclaimed for each pubkey. If the pubkey's entry was removed from the accounts
    /// index, its secondary index entries are purged with it.
    fn collect_reclaims(
        &self,
        pubkey: &Pubkey,
        max_clean_root_inclusive: Option<Slot>,
    ) -> ReclaimsWithNewestSlot<AccountInfo> {
        let mut clean_rooted = Measure::start("clean_old_root-ms");
        let mut reclaims = ReclaimsWithNewestSlot::new();
        let removed_from_index = self.accounts_index.clean_rooted_entries(
            pubkey,
            &mut reclaims,
            max_clean_root_inclusive,
        );
        clean_rooted.stop();
        if removed_from_index {
            self.clean_accounts_stats
                .num_accounts_removed_from_index
                .fetch_add(1, Ordering::Relaxed);
            self.purge_secondary_indexes_for_dead_keys(iter::once(pubkey));
        }
        self.clean_accounts_stats
            .clean_old_root_us
            .fetch_add(clean_rooted.as_us(), Ordering::Relaxed);
        reclaims
    }

    /// Reclaim older states of accounts older than max_clean_root_inclusive for AccountsDb bloat mitigation.
    ///
    /// The reclaimed accounts were already removed from the slot list when the
    /// reclaims were collected
    fn clean_accounts_older_than_root(&self, reclaims: &ReclaimsWithNewestSlot<AccountInfo>) {
        if reclaims.is_empty() {
            return;
        }
        let (_, reclaim_us) = measure_us!({
            // Each reclaim is marked obsolete at the slot of its account's newest surviving
            // entry. A reclaim carrying its own slot is the newest entry itself, already
            // removed from the index, and is created as a tombstone instead
            self.thread_pool_background.install(|| {
                reclaims
                    .par_iter()
                    .for_each(|(reclaimed_item, newest_slot)| {
                        self.handle_reclaims(
                            iter::once(reclaimed_item),
                            &self.clean_accounts_stats.purge_stats,
                            MarkAccountsObsolete::Yes(*newest_slot),
                        );
                    });
            });
        });
        self.clean_accounts_stats
            .clean_old_root_reclaim_us
            .fetch_add(reclaim_us, Ordering::Relaxed);
    }

    /// Purges each key in `removed_keys` from the enabled secondary indexes, unless the key is
    /// still alive in the write cache. `removed_keys` must be keys that are not present in the
    /// primary index
    ///
    /// The cache check is all-or-nothing per key: a key kept because it is cache-live retains all
    /// of its secondary entries, including stale ones from its dead rooted versions (e.g. an old
    /// mint after the account is re-created with a new one). Scans tolerate stale entries by
    /// post-filtering against account data, and they are removed the next time the key dies while
    /// not cache-resident.
    ///
    /// Cache writes populate the secondary indexes but not the primary index, so a key that is gone
    /// from the primary index can still be alive in the write cache and must keep its secondary
    /// entries.
    ///
    /// Clean calls this for the keys whose last primary index entry it removed, and for the keys
    /// that `purge_slots_from_cache` removed from the cache and deferred to
    /// `handle_pubkeys_removed_from_cache`. Flush calls it for a zero-lamport account that leaves
    /// the cache without a primary index entry, either skipped or stored as a tombstone and deleted
    /// from the index. Callers never run concurrently with each other: clean and flush both run on
    /// the ABS thread and the snapshot minimizer runs standalone, so the only concurrent writer is
    /// replay. This is tricky due to the races that need to be considered:
    /// 1) Removed from the cache then re-added to the cache by replay
    /// - This is protected by re-checking the cache in the closure passed to purge. Since purge
    ///   holds the secondary index's reverse-index lock when it re-checks cache presence, and a
    ///   cache store writes the cache before inserting into the secondary index under that same
    ///   lock, either the re-check sees the cache write and the entry is not removed, or the
    ///   removal wins and the store's later insert re-adds it.
    /// 2) The same key is handled twice, e.g. flush purges a tombstoned key that clean then purges
    ///    again from the deferred list
    /// - Since the cache removal and the index removal are both done before the removal from the
    ///   secondary index, the worst case is a double removal (both paths remove the same secondary
    ///   index entry). This is safe since the secondary index removal is idempotent.
    /// 3) Removed from the primary index, but still present in the cache
    /// - This is protected by checking the cache presence in the closure. If the pubkey is still
    ///   present in the cache, the secondary index entry is not removed. It is purged when the key
    ///   later leaves the cache: by flush if the key is skipped or tombstoned, otherwise by the
    ///   deferred handling in clean.
    /// 4) A deferred key is re-added to the cache, rooted, and flushed to storage before clean
    ///    handles it
    /// - The deferred keys are passed through `handle_dead_keys` first, which yields a key only
    ///   when its slot list is absent or empty, checked under the key's index entry lock. A key
    ///   that regained a primary index entry in the deferral window is not yielded, and flush
    ///   cannot add one while clean is running since both run on the ABS thread.
    fn purge_secondary_indexes_for_dead_keys<'a>(
        &self,
        removed_keys: impl IntoIterator<Item = &'a Pubkey>,
    ) {
        if self.account_indexes.is_empty() {
            return;
        }
        for key in removed_keys {
            // Purging secondary entries for a key that is still alive in the primary index
            // would leave a live account invisible to secondary-index scans
            debug_assert!(
                !self.accounts_index.contains(key),
                "key removed from the primary index must not be present: {key}"
            );
            self.accounts_index.purge_secondary_indexes_by_inner_key_if(
                key,
                &self.account_indexes,
                || !self.accounts_cache.contains_pubkey(key),
            );
        }
    }

    #[must_use]
    pub fn purge_keys_exact<C>(
        &self,
        pubkey_to_slot_set: impl IntoIterator<Item = (Pubkey, C)>,
    ) -> ReclaimsSlotList<AccountInfo>
    where
        C: for<'a> Contains<'a, Slot>,
    {
        let mut reclaims = ReclaimsSlotList::new();
        let mut dead_keys = Vec::new();

        let mut purge_exact_count = 0;
        let (_, purge_exact_us) =
            measure_us!(for (pubkey, slots_set) in pubkey_to_slot_set.into_iter() {
                purge_exact_count += 1;
                let is_empty = self
                    .accounts_index
                    .purge_exact(&pubkey, slots_set, &mut reclaims);
                if is_empty {
                    dead_keys.push(pubkey);
                }
            });

        let (_, handle_dead_keys_us) = measure_us!({
            let removed_keys = self.accounts_index.handle_dead_keys(&dead_keys);
            self.purge_secondary_indexes_for_dead_keys(&removed_keys);
        });

        self.stats
            .purge_exact_count
            .fetch_add(purge_exact_count, Ordering::Relaxed);
        self.stats
            .handle_dead_keys_us
            .fetch_add(handle_dead_keys_us, Ordering::Relaxed);
        self.stats
            .purge_exact_us
            .fetch_add(purge_exact_us, Ordering::Relaxed);
        reclaims
    }

    fn max_clean_root(&self, proposed_clean_root: Option<Slot>) -> Option<Slot> {
        match (
            self.scan_tracker.min_ongoing_scan_root(),
            proposed_clean_root,
        ) {
            (None, None) => None,
            (Some(min_scan_root), None) => Some(min_scan_root),
            (None, Some(proposed_clean_root)) => Some(proposed_clean_root),
            (Some(min_scan_root), Some(proposed_clean_root)) => {
                Some(std::cmp::min(min_scan_root, proposed_clean_root))
            }
        }
    }

    /// get the oldest slot that is within one epoch of the highest known root.
    /// The slot will have been offset by `self.ancient_append_vec_offset`
    fn get_oldest_non_ancient_slot(&self, epoch_schedule: &EpochSchedule) -> Slot {
        self.get_oldest_non_ancient_slot_from_slot(epoch_schedule, self.max_root())
    }

    /// get the oldest slot that is within one epoch of `max_root`.
    /// The slot will have been offset by `self.ancient_append_vec_offset`
    fn get_oldest_non_ancient_slot_from_slot(
        &self,
        epoch_schedule: &EpochSchedule,
        max_root: Slot,
    ) -> Slot {
        let mut result = max_root;
        if let Some(offset) = self.ancient_append_vec_offset {
            result = Self::apply_offset_to_slot(result, offset);
        }
        result = Self::apply_offset_to_slot(
            result,
            -((epoch_schedule.slots_per_epoch as i64).saturating_sub(1)),
        );
        result.min(max_root)
    }

    /// Collect all the uncleaned slots, up to a max slot
    ///
    /// Search through the uncleaned Pubkeys and return all the slots, up to a maximum slot.
    fn collect_uncleaned_slots_up_to_slot(&self, max_slot_inclusive: Option<Slot>) -> Vec<Slot> {
        self.uncleaned_pubkeys
            .iter()
            .filter_map(|entry| {
                let slot = *entry.key();
                max_slot_inclusive
                    .is_none_or(|max_slot_inclusive| slot <= max_slot_inclusive)
                    .then_some(slot)
            })
            .collect()
    }

    /// For each slot in the list of uncleaned slots, up to a maximum
    /// slot, remove it from the `uncleaned_pubkeys` and move all the
    /// pubkeys to `candidates` for cleaning.
    fn remove_uncleaned_slots_up_to_slot_and_move_pubkeys(
        &self,
        max_slot_inclusive: Option<Slot>,
        candidates: &[RwLock<CleaningCandidatesBin>],
    ) {
        let uncleaned_slots = self.collect_uncleaned_slots_up_to_slot(max_slot_inclusive);
        for uncleaned_slot in uncleaned_slots.into_iter() {
            if let Some((_removed_slot, mut removed_pubkeys)) =
                self.uncleaned_pubkeys.remove(&uncleaned_slot)
            {
                // Sort all keys by bin index so that we can insert
                // them in `candidates` more efficiently.
                removed_pubkeys.sort_unstable_by_key(|pubkey| {
                    self.accounts_index.bin_calculator.bin_from_pubkey(pubkey)
                });
                if let Some(first_removed_pubkey) = removed_pubkeys.first() {
                    let mut prev_bin = self
                        .accounts_index
                        .bin_calculator
                        .bin_from_pubkey(first_removed_pubkey);
                    let mut candidates_bin = candidates[prev_bin].write().unwrap();
                    for removed_pubkey in removed_pubkeys {
                        let curr_bin = self
                            .accounts_index
                            .bin_calculator
                            .bin_from_pubkey(&removed_pubkey);
                        if curr_bin != prev_bin {
                            candidates_bin = candidates[curr_bin].write().unwrap();
                            prev_bin = curr_bin;
                        }
                        candidates_bin.insert(removed_pubkey);
                    }
                }
            }
        }
    }

    /// Construct a list of candidates for cleaning from:
    /// - uncleaned_pubkeys -- the delta set of updated pubkeys in rooted slots from the last clean
    fn construct_candidate_clean_keys(
        &self,
        max_clean_root_inclusive: Option<Slot>,
        timings: &mut CleanKeyTimings,
    ) -> CleaningCandidates {
        let num_bins = self.accounts_index.bins();
        let candidates: CleaningCandidates =
            std::iter::repeat_with(|| RwLock::new(CleaningCandidatesBin::default()))
                .take(num_bins)
                .collect();

        let mut collect_delta_keys = Measure::start("key_create");
        self.remove_uncleaned_slots_up_to_slot_and_move_pubkeys(
            max_clean_root_inclusive,
            &candidates,
        );
        collect_delta_keys.stop();
        timings.collect_delta_keys_us += collect_delta_keys.as_us();

        // Cleaning up zero lamport accounts is gated by a full snapshot because they need to be
        // retained for incremental snapshots. Once a full snapshot occurs, sweep the newly-covered
        // slots for tombstone-only storages to purge and newly shrinkable storages.
        if self
            .latest_full_snapshot_slot_advanced_since_clean
            .swap(false, Ordering::Acquire)
            && let Some(latest_full_snapshot_slot) = self.latest_full_snapshot_slot()
        {
            let last_swept_full_snapshot_slot =
                self.last_swept_full_snapshot_slot.load(Ordering::Relaxed);
            let (added_to_shrink_count, sweep_us) = measure_us!(self.sweep_slots_after_snapshot(
                last_swept_full_snapshot_slot,
                latest_full_snapshot_slot
            ));
            timings.zero_lamport_single_ref_slots_added_to_shrink_count += added_to_shrink_count;
            timings.zero_lamport_sweep_us += sweep_us;
        }

        candidates
    }

    /// Loop through slots in `[last_swept_full_snapshot_slot + 1, latest_full_snapshot_slot]` and
    /// re-examine each storage now that a full snapshot has advanced past its slot:
    /// 1) if it holds only tombstones, purge it directly; or
    /// 2) if its dead zero-lamport accounts made it shrinkable, add it to the shrink candidates.
    ///
    /// Advances `last_swept_full_snapshot_slot` to `latest_full_snapshot_slot` on completion.
    ///
    /// Returns the count of storages that were added to the shrink candidates set.
    fn sweep_slots_after_snapshot(
        &self,
        last_swept_full_snapshot_slot: Slot,
        latest_full_snapshot_slot: Slot,
    ) -> u64 {
        let start = last_swept_full_snapshot_slot.saturating_add(1);

        let mut added_to_shrink_count = 0;
        {
            // Held for the scan. Safe because the only paths that take this lock in production
            // validator code run in earlier/later phases of the same AccountsBackgroundService
            // iteration, never concurrently with clean_accounts.
            let mut shrink_candidates = self.shrink_candidate_slots.lock().unwrap();
            for slot in start..=latest_full_snapshot_slot {
                if let Some(store) = self.storage.get_slot_storage_entry(slot) {
                    if store.has_only_tombstones() {
                        // Now just contains tombstones and no live index entries: purge
                        self.purge_dead_slots_from_storage(
                            iter::once(&slot),
                            &self.clean_accounts_stats.purge_stats,
                        );
                    } else if self.is_shrinking_productive(&store)
                        && self.is_candidate_for_shrink(&store)
                        && shrink_candidates.insert(slot)
                    {
                        added_to_shrink_count += 1;
                    }
                }
            }
        }

        self.last_swept_full_snapshot_slot
            .store(latest_full_snapshot_slot, Ordering::Relaxed);
        added_to_shrink_count
    }

    /// called with cli argument to verify the index is correct for all accounts
    /// this is very slow
    /// this function will call Rayon par_iter, so you will want to have thread pool installed if
    /// you want to call this without consuming all the cores on the CPU.
    fn verify_index(&self, max_slot_inclusive: Option<Slot>) {
        info!("verifying index as of slot: {max_slot_inclusive:?}");
        let pubkey_slot_lists = DashMap::<Pubkey, Vec<Slot>, PubkeyHasherBuilder>::default();
        let mut storages = self.storage.all_storages();
        // Flush is not running while we verify, so storages are stable. With no slot bound we
        // verify every storage; otherwise we drop storages newer than the bound.
        if let Some(max_slot_inclusive) = max_slot_inclusive {
            storages.retain(|s| s.slot() <= max_slot_inclusive);
        }
        // populate
        storages.par_iter().for_each_init(
            || Box::new(append_vec::new_scan_accounts_reader()),
            |reader, storage| {
                let slot = storage.slot();
                storage
                    .scan_accounts(reader.as_mut(), |_offset, account| {
                        let pk = account.pubkey();
                        match pubkey_slot_lists.entry(*pk) {
                            dashmap::mapref::entry::Entry::Occupied(mut occupied_entry) => {
                                if !occupied_entry.get().iter().any(|s| s == &slot) {
                                    occupied_entry.get_mut().push(slot);
                                }
                            }
                            dashmap::mapref::entry::Entry::Vacant(vacant_entry) => {
                                vacant_entry.insert(vec![slot]);
                            }
                        }
                    })
                    .expect("must scan accounts storage");
            },
        );
        let total = pubkey_slot_lists.len();
        if total == 0 {
            return;
        }
        let failed = AtomicBool::default();
        let threads = rayon::current_num_threads();
        let per_batch = total.div_ceil(threads);
        (0..=threads).into_par_iter().for_each(|attempt| {
            pubkey_slot_lists
                .iter()
                .skip(attempt * per_batch)
                .take(per_batch)
                .for_each(|entry| {
                    let mut storage_slots = entry.value().clone();
                    storage_slots.sort_unstable();
                    self.accounts_index
                        .get_and_then(entry.key(), |index_entry| {
                            let Some(index_entry) = index_entry else {
                                failed.store(true, Ordering::Relaxed);
                                error!(
                                    "verify_index: {} has no index entry, storages: \
                                     {storage_slots:?}",
                                    entry.key(),
                                );
                                return (false, ());
                            };
                            let slot_list = index_entry.slot_list_read_lock();
                            // Slots newer than `max_slot_inclusive` are in the index but were
                            // excluded from the storage scan, so exclude them from the comparison
                            // too.
                            let mut index_slots = slot_list
                                .iter()
                                .map(|(slot, _)| *slot)
                                .filter(|slot| {
                                    max_slot_inclusive.is_none_or(|max_slot_inclusive| {
                                        *slot <= max_slot_inclusive
                                    })
                                })
                                .collect::<Vec<_>>();
                            index_slots.sort_unstable();

                            if index_slots != storage_slots {
                                failed.store(true, Ordering::Relaxed);
                                error!(
                                    "verify_index: {} index slot list does not match storages: \
                                     index: {index_slots:?}, storages: {storage_slots:?}, slot \
                                     list: {:?}",
                                    entry.key(),
                                    slot_list,
                                );
                            }
                            (false, ())
                        });
                });
        });
        if failed.load(Ordering::Relaxed) {
            panic!("verify_index failed");
        }
    }

    // Purge zero lamport accounts and older rooted account states as garbage
    // collection
    // Only remove those accounts where the entire rooted history of the account
    // can be purged because there are no live append vecs in the ancestors
    pub fn clean_accounts(&self, max_clean_root_inclusive: Option<Slot>, is_startup: bool) {
        if self.verify_index {
            //at startup use all cores to verify the index
            if is_startup {
                self.verify_index(max_clean_root_inclusive);
            } else {
                // otherwise, use the background thread pool
                self.thread_pool_background
                    .install(|| self.verify_index(max_clean_root_inclusive));
            }
        }

        let _guard = self.active_stats.activate(ActiveStatItem::Clean);

        let purges_old_accounts_count = AtomicU64::default();

        let mut measure_all = Measure::start("clean_accounts");
        let max_clean_root_inclusive = self.max_clean_root(max_clean_root_inclusive);

        self.report_store_stats();

        // purge_slots_from_cache delays handling of the pubkeys it removes from the cache
        // so that the purge path never modifies the accounts index. Handle them here
        let (_, handle_pubkeys_removed_from_cache_us) =
            measure_us!(self.handle_pubkeys_removed_from_cache());

        let active_guard = self
            .active_stats
            .activate(ActiveStatItem::CleanConstructCandidates);
        let mut measure_construct_candidates = Measure::start("construct_candidates");
        let mut key_timings = CleanKeyTimings::default();
        let candidates =
            self.construct_candidate_clean_keys(max_clean_root_inclusive, &mut key_timings);
        measure_construct_candidates.stop();
        drop(active_guard);

        let num_candidates = candidates.iter().map(|x| x.read().unwrap().len()).sum();
        let found_not_zero_accum = AtomicU64::new(0);
        let not_found_on_fork_accum = AtomicU64::new(0);
        let missing_accum = AtomicU64::new(0);
        let useful_accum = AtomicU64::new(0);
        let reclaims = ReclaimsWithNewestSlot::with_capacity(num_candidates);
        let reclaims = Mutex::new(reclaims);
        // parallel scan the index.
        let do_clean_scan = || {
            candidates.par_iter().for_each(|candidates_bin| {
                let mut found_not_zero = 0;
                let mut not_found_on_fork = 0;
                let mut missing = 0;
                let mut useful = 0;
                let mut purges_old_accounts_local = 0;
                // Take the bin so its allocation is freed by this thread once the bin is
                // scanned, rather than serially after every bin completes.
                let candidates_bin = mem::take(&mut *candidates_bin.write().unwrap());
                for candidate_pubkey in candidates_bin {
                    let mut should_collect_reclaims = false;
                    self.accounts_index.scan(
                        iter::once(&candidate_pubkey),
                        |_candidate_pubkey, slot_list| {
                            let mut useless = true;
                            if let Some(slot_list) = slot_list {
                                // find the highest rooted slot in the slot list
                                let index_in_slot_list = self.accounts_index.latest_slot(
                                    None,
                                    slot_list,
                                    max_clean_root_inclusive,
                                );

                                match index_in_slot_list {
                                    Some(index_in_slot_list) => {
                                        // found info relative to max_clean_root
                                        let (slot, account_info) = &slot_list[index_in_slot_list];
                                        if account_info.is_zero_lamport() {
                                            useless = false;
                                            // The latest one is zero lamports. We may be able to purge it.
                                            // Even if the slot list length is 1, this may be
                                            // reclaimable as it is a zero lamport account
                                            should_collect_reclaims = true;
                                        } else {
                                            found_not_zero += 1;
                                        }

                                        // If this candidate has multiple rooted slot list entries,
                                        // we should reclaim the older ones.
                                        if slot_list.len() > 1
                                            && *slot
                                                <= max_clean_root_inclusive.unwrap_or(Slot::MAX)
                                        {
                                            should_collect_reclaims = true;
                                            purges_old_accounts_local += 1;
                                            useless = false;
                                        }
                                    }
                                    None => {
                                        // This pubkey is in the index but not in a root slot, so clean
                                        // it up by adding it to the to-be-purged list.
                                        //
                                        // Also, this pubkey must have been touched by some slot since
                                        // it was in the dirty list, so we assume that the slot it was
                                        // touched in must be unrooted.
                                        not_found_on_fork += 1;
                                        should_collect_reclaims = true;
                                        purges_old_accounts_local += 1;
                                        useless = false;
                                    }
                                }
                            } else {
                                missing += 1;
                            }
                            if !useless {
                                useful += 1;
                            }
                        },
                        ScanFilter::All,
                    );
                    if should_collect_reclaims {
                        let reclaims_new =
                            self.collect_reclaims(&candidate_pubkey, max_clean_root_inclusive);
                        if !reclaims_new.is_empty() {
                            reclaims.lock().unwrap().extend(reclaims_new);
                        }
                    }
                }
                found_not_zero_accum.fetch_add(found_not_zero, Ordering::Relaxed);
                not_found_on_fork_accum.fetch_add(not_found_on_fork, Ordering::Relaxed);
                missing_accum.fetch_add(missing, Ordering::Relaxed);
                useful_accum.fetch_add(useful, Ordering::Relaxed);
                purges_old_accounts_count.fetch_add(purges_old_accounts_local, Ordering::Relaxed);
            });
        };
        let active_guard = self
            .active_stats
            .activate(ActiveStatItem::CleanScanCandidates);
        let mut accounts_scan = Measure::start("accounts_scan");
        if is_startup {
            do_clean_scan();
        } else {
            self.thread_pool_background.install(do_clean_scan);
        }
        accounts_scan.stop();
        drop(active_guard);

        let reclaims = reclaims.into_inner().unwrap();

        let active_guard = self.active_stats.activate(ActiveStatItem::CleanOldAccounts);
        let mut clean_old_rooted = Measure::start("clean_old_roots");
        self.clean_accounts_older_than_root(&reclaims);
        clean_old_rooted.stop();
        drop(active_guard);

        measure_all.stop();

        self.clean_accounts_stats.report();
        datapoint_info!(
            "clean_accounts",
            ("max_clean_root", max_clean_root_inclusive, Option<i64>),
            ("total_us", measure_all.as_us(), i64),
            (
                "collect_delta_keys_us",
                key_timings.collect_delta_keys_us,
                i64
            ),
            ("construct_candidates_us", measure_construct_candidates.as_us(), i64),
            (
                "handle_pubkeys_removed_from_cache_us",
                handle_pubkeys_removed_from_cache_us,
                i64
            ),
            ("accounts_scan", accounts_scan.as_us(), i64),
            ("clean_old_rooted", clean_old_rooted.as_us(), i64),
            (
                "zero_lamport_single_ref_slots_added_to_shrink_count",
                key_timings.zero_lamport_single_ref_slots_added_to_shrink_count,
                i64
            ),
            ("zero_lamport_sweep_us", key_timings.zero_lamport_sweep_us, i64),
            ("useful_keys", useful_accum.load(Ordering::Relaxed), i64),
            ("total_keys_count", num_candidates, i64),
            (
                "scan_found_not_zero",
                found_not_zero_accum.load(Ordering::Relaxed),
                i64
            ),
            (
                "scan_not_found_on_fork",
                not_found_on_fork_accum.load(Ordering::Relaxed),
                i64
            ),
            ("scan_missing", missing_accum.load(Ordering::Relaxed), i64),
            (
                "get_account_sizes_us",
                self.clean_accounts_stats
                    .get_account_sizes_us
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "slots_cleaned",
                self.clean_accounts_stats
                    .slots_cleaned
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "num_accounts_removed_from_index",
                self.clean_accounts_stats
                    .num_accounts_removed_from_index
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "clean_old_root_us",
                self.clean_accounts_stats
                    .clean_old_root_us
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "clean_old_root_reclaim_us",
                self.clean_accounts_stats
                    .clean_old_root_reclaim_us
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "remove_dead_accounts_remove_us",
                self.clean_accounts_stats
                    .remove_dead_accounts_remove_us
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "remove_dead_accounts_shrink_us",
                self.clean_accounts_stats
                    .remove_dead_accounts_shrink_us
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "purge_older_root_entries_one_slot_list",
                self.accounts_index
                    .purge_older_root_entries_one_slot_list
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "active_scans",
                self.scan_tracker.active_scans.load(Ordering::Relaxed),
                i64
            ),
            (
                "max_distance_to_min_scan_slot",
                self.scan_tracker.max_distance_to_min_scan_slot
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "purges_old_accounts_count",
                purges_old_accounts_count.load(Ordering::Relaxed),
                i64
            ),
            ("next_store_id", self.next_id.load(Ordering::Relaxed), i64),
        );
    }

    /// Removes the accounts in the input `reclaims` from the tracked "count" of
    /// their corresponding  storage entries. Note this does not actually free
    /// the memory from the storage entries until all the storage entries for
    /// a given slot `S` are empty, at which point `process_dead_slots` will
    /// remove all the storage entries for `S`.
    ///
    /// # Arguments
    /// * `reclaims` - The accounts to remove from storage entries' "count". Note here
    ///   that we should not remove cache entries, only entries for accounts actually
    ///   stored in a storage entry.
    /// * `handle_reclaims`. `purge_stats` are stats used to track performance of purging
    ///   dead slots if value is `ProcessDeadSlots`.
    ///   Otherwise, there can be no dead slots
    ///   that happen as a result of this call, and the function will check that no slots are
    ///   cleaned up/removed via `process_dead_slots`. For instance, on store, no slots should
    ///   be cleaned up, but during the background clean accounts purges accounts from old rooted
    ///   slots, so outdated slots may be removed.
    /// * 'mark_accounts_obsolete' - Whether to mark accounts as obsolete or not. If `Yes`, then
    ///   obsolete account entry will be marked in the storage so snapshots/accounts hash can
    ///   determine the state of the account at a specified slot. This should only be done if the
    ///   account is already removed from the accounts index
    ///   It must be removed to avoid double counting or missed counting in shrink
    ///
    /// Returns the set of dead slots that were removed from storage as a result of this call.
    fn handle_reclaims<'a, I>(
        &'a self,
        reclaims: I,
        purge_stats: &PurgeStats,
        mark_accounts_obsolete: MarkAccountsObsolete,
    ) -> IntSet<Slot>
    where
        I: Iterator<Item = &'a (Slot, AccountInfo)>,
    {
        let dead_slots = self.remove_dead_accounts(reclaims, mark_accounts_obsolete);

        self.process_dead_slots(&dead_slots, purge_stats);
        dead_slots
    }

    // Must be kept private!, does sensitive cleanup that should only be called from
    // supported pipelines in AccountsDb
    fn process_dead_slots(&self, dead_slots: &IntSet<Slot>, purge_stats: &PurgeStats) {
        if dead_slots.is_empty() {
            return;
        }

        let mut purge_removed_slots = Measure::start("reclaims::purge_removed_slots");
        self.purge_dead_slots_from_storage(dead_slots.iter(), purge_stats);
        purge_removed_slots.stop();

        // If the slot is dead, remove the need to shrink the storages as
        // the storage entries will be purged.
        {
            let mut list = self.shrink_candidate_slots.lock().unwrap();
            for slot in dead_slots {
                list.remove(slot);
            }
        }

        debug!(
            "process_dead_slots({}): {} {:?}",
            dead_slots.len(),
            purge_removed_slots,
            dead_slots,
        );
    }

    /// load the account index entry for the first `count` items in `accounts`
    /// store a reference to all alive accounts in `alive_accounts`
    /// return sum of account size for all alive accounts
    fn load_accounts_index_for_shrink<'a, T: ShrinkCollector<'a>>(
        &self,
        accounts: &'a [AccountFromStorage],
        stats: &ShrinkStats,
        slot_to_shrink: Slot,
    ) -> LoadAccountsIndexForShrink<T> {
        let count = accounts.len();
        let mut alive_accounts = T::with_capacity(count, slot_to_shrink);

        let mut index = 0;
        let mut index_scan_returned_some_count = 0;
        let mut index_scan_returned_none_count = 0;
        self.accounts_index.scan(
            accounts.iter().map(|account| account.pubkey()),
            |_pubkey, slot_list| {
                let stored_account = &accounts[index];
                if let Some(slot_list) = slot_list {
                    index_scan_returned_some_count += 1;
                    let is_alive = slot_list.iter().any(|(slot, _acct_info)| {
                        // if the accounts index contains an entry at this slot, then the append vec we're asking about contains this item and thus, it is alive at this slot
                        *slot == slot_to_shrink
                    });

                    // All obsolete and tombstones have been filtered. Account MUST be alive in this slot
                    assert!(is_alive);
                    alive_accounts.add(stored_account, slot_list);
                } else {
                    index_scan_returned_none_count += 1;
                    // getting None here means the account is 'normal' and was written to disk. This means it must have
                    // slot_list.len() = 1. This means it must be alive in this slot. This is by far the most common case.
                    // Note that we could get Some(...) here if the account is in the in mem index because it is hot.
                    // Note this could also mean the account isn't on disk either. That would indicate a bug in accounts db.
                    // Account is alive.
                    let slot_list = [(slot_to_shrink, AccountInfo::default())];
                    alive_accounts.add(stored_account, &slot_list);
                }
                index += 1;
            },
            self.scan_filter_for_shrinking,
        );
        assert_eq!(index, std::cmp::min(accounts.len(), count));
        stats
            .index_scan_returned_some
            .fetch_add(index_scan_returned_some_count, Ordering::Relaxed);
        stats
            .index_scan_returned_none
            .fetch_add(index_scan_returned_none_count, Ordering::Relaxed);

        LoadAccountsIndexForShrink { alive_accounts }
    }

    /// get all accounts in all the storages passed in
    /// for duplicate pubkeys, the account with the highest write_value is returned
    pub fn get_unique_accounts_from_storage(
        &self,
        store: &AccountStorageEntry,
    ) -> GetUniqueAccountsResult {
        let written_bytes = store.written_bytes();
        let mut stored_accounts = Vec::with_capacity(store.count());
        store
            .accounts
            .scan_accounts_without_data(|offset, account| {
                // file_id is unused and can be anything. We will always be loading whatever storage is in the slot.
                let file_id = 0;
                stored_accounts.push(AccountFromStorage {
                    index_info: AccountInfo::new(
                        StorageLocation::AccountsFile(file_id, offset),
                        account.is_zero_lamport(),
                    ),
                    pubkey: *account.pubkey(),
                    data_len: account.data_len as u64,
                });
            })
            .expect("must scan accounts storage");

        // sort by pubkey bin to keep account index lookups close
        stored_accounts.sort_unstable_by_key(|account| {
            self.accounts_index
                .bin_calculator
                .bin_from_pubkey(account.pubkey())
        });

        GetUniqueAccountsResult {
            stored_accounts,
            written_bytes,
        }
    }

    pub(crate) fn get_unique_accounts_from_storage_for_shrink(
        &self,
        store: &AccountStorageEntry,
        stats: &ShrinkStats,
    ) -> GetUniqueAccountsResult {
        let (result, storage_read_elapsed_us) =
            measure_us!(self.get_unique_accounts_from_storage(store));
        stats
            .storage_read_elapsed
            .fetch_add(storage_read_elapsed_us, Ordering::Relaxed);
        result
    }

    /// shared code for shrinking normal slots and combining into ancient append vecs
    /// note 'unique_accounts' is passed by ref so we can return references to data within it, avoiding self-references
    pub(crate) fn shrink_collect<'a: 'b, 'b, T: ShrinkCollector<'b>>(
        &self,
        store: &'a AccountStorageEntry,
        unique_accounts: &'b mut GetUniqueAccountsResult,
        stats: &ShrinkStats,
    ) -> ShrinkCollect<T> {
        let slot = store.slot();

        let GetUniqueAccountsResult {
            stored_accounts,
            written_bytes,
        } = unique_accounts;

        let mut index_read_elapsed = Measure::start("index_read_elapsed");

        // Get a set of all obsolete offsets
        // Slot is not needed, as all obsolete accounts can be considered
        // dead for shrink. Zero lamport accounts are not marked obsolete
        let obsolete_offsets: IntSet<_> = store
            .obsolete_accounts_read_lock()
            .filter_obsolete_accounts(None)
            .map(|(offset, _)| offset)
            .collect();

        // Filter all the accounts that are marked obsolete
        let total_starting_accounts = stored_accounts.len();
        stored_accounts.retain(|account| !obsolete_offsets.contains(&account.index_info.offset()));
        let num_obsolete_filtered = total_starting_accounts - stored_accounts.len();

        // Filter and collect tombstones
        let can_purge_zero_lamport_accounts = self.can_purge_zero_lamport_accounts(slot);
        let mut tombstones_to_carry_forward = Vec::new();
        let tombstone_offsets = store.tombstone_offsets_read_lock();
        if !tombstone_offsets.is_empty() {
            stored_accounts.retain(|account| {
                if tombstone_offsets.contains(&account.index_info.offset()) {
                    // If we can't purge zero lamport accounts, they need to be rewritten after shrink
                    if !can_purge_zero_lamport_accounts {
                        tombstones_to_carry_forward.push(*account);
                    }
                    false
                } else {
                    true
                }
            });
        }
        drop(tombstone_offsets);

        let tombstones_total_bytes = tombstones_to_carry_forward
            .iter()
            .map(|account| account.stored_size())
            .sum();

        let len = stored_accounts.len();
        let shrink_collect = Mutex::new(ShrinkCollect {
            slot,
            written_bytes: *written_bytes,
            alive_accounts: T::with_capacity(len, slot),
            tombstones_to_carry_forward,
            tombstones_total_bytes,
            total_starting_accounts,
            alive_total_bytes: 0, // will be updated after `alive_accounts` is populated
        });

        stats
            .accounts_loaded
            .fetch_add(len as u64, Ordering::Relaxed);
        stats
            .obsolete_accounts_filtered
            .fetch_add(num_obsolete_filtered as u64, Ordering::Relaxed);
        self.thread_pool_background.install(|| {
            stored_accounts
                .par_chunks(SHRINK_COLLECT_CHUNK_SIZE)
                .for_each(|stored_accounts| {
                    let LoadAccountsIndexForShrink { alive_accounts } =
                        self.load_accounts_index_for_shrink(stored_accounts, stats, slot);

                    // collect
                    let mut shrink_collect = shrink_collect.lock().unwrap();
                    shrink_collect.alive_accounts.collect(alive_accounts);
                });
        });

        index_read_elapsed.stop();

        let mut shrink_collect = shrink_collect.into_inner().unwrap();
        let alive_total_bytes = shrink_collect.alive_accounts.alive_bytes();
        shrink_collect.alive_total_bytes = alive_total_bytes;

        stats
            .index_read_elapsed
            .fetch_add(index_read_elapsed.as_us(), Ordering::Relaxed);

        // Tombstones carried forward are rewritten into the new storage, not reclaimed, so exclude
        // them from the "removed" totals (which measure what shrink actually freed).
        stats.accounts_removed.fetch_add(
            total_starting_accounts
                - shrink_collect.alive_accounts.len()
                - shrink_collect.tombstones_to_carry_forward.len(),
            Ordering::Relaxed,
        );
        stats.bytes_removed.fetch_add(
            written_bytes
                .saturating_sub(alive_total_bytes as u64)
                .saturating_sub(shrink_collect.tombstones_total_bytes as u64),
            Ordering::Relaxed,
        );

        shrink_collect
    }

    /// common code from shrink and combine_ancient_slots
    /// get rid of all original store_ids in the slot
    pub(crate) fn remove_old_stores_shrink(
        &self,
        slot: Slot,
        stats: &ShrinkStats,
        shrink_in_progress: Option<ShrinkInProgress>,
        shrink_can_be_active: bool,
    ) {
        let mut time = Measure::start("remove_old_stores_shrink");

        // Purge old, overwritten storage entries
        // This has the side effect of dropping `shrink_in_progress`, which removes the old storage completely. The
        // index has to be correct before we drop the old storage.
        let dead_storages =
            self.mark_dirty_dead_stores(slot, shrink_in_progress, shrink_can_be_active);
        let dead_storages_len = dead_storages.len();

        let (_, drop_storage_entries_elapsed) = measure_us!(drop(dead_storages));
        time.stop();

        self.stats
            .dropped_stores
            .fetch_add(dead_storages_len as u64, Ordering::Relaxed);
        stats
            .drop_storage_entries_elapsed
            .fetch_add(drop_storage_entries_elapsed, Ordering::Relaxed);
        stats
            .remove_old_stores_shrink_us
            .fetch_add(time.as_us(), Ordering::Relaxed);
    }

    /// Shrinks `store` by rewriting the alive accounts to a new storage
    fn shrink_storage(&self, store: Arc<AccountStorageEntry>) {
        let slot = store.slot();
        if self.accounts_cache.contains(slot) {
            // It is not correct to shrink a slot while it is in the write cache until flush is complete and the slot is removed from the write cache.
            // There can exist a window after a slot is made a root and before the write cache flushing for that slot begins and then completes.
            // There can also exist a window after a slot is being flushed from the write cache until the index is updated and the slot is removed from the write cache.
            // During the second window, once an append vec has been created for the slot, it could be possible to try to shrink that slot.
            // Shrink no-ops before this function if there is no store for the slot (notice this function requires 'store' to be passed).
            // So, if we enter this function but the slot is still in the write cache, reasonable behavior is to skip shrinking this slot.
            // Flush will ONLY write alive accounts to the append vec, which is what shrink does anyway.
            // Flush then adds the slot to 'uncleaned_roots', which causes clean to take a look at the slot.
            // Clean causes us to mark accounts as dead, which causes shrink to later take a look at the slot.
            // This could be an assert, but it could lead to intermittency in tests.
            // It is 'correct' to ignore calls to shrink when a slot is still in the write cache.
            return;
        }
        let mut unique_accounts =
            self.get_unique_accounts_from_storage_for_shrink(&store, &self.shrink_stats);
        debug!("do_shrink_slot_store: slot: {slot}");
        let shrink_collect = self.shrink_collect::<AliveAccounts<'_>>(
            &store,
            &mut unique_accounts,
            &self.shrink_stats,
        );

        let total_rewrite_bytes =
            shrink_collect.alive_total_bytes + shrink_collect.tombstones_total_bytes;

        // Nothing to rewrite: nothing alive would be copied to a new storage, so the whole
        // storage is dead. Marking the slot dead is clean's job.
        if total_rewrite_bytes == 0 {
            self.shrink_stats
                .skipped_shrink
                .fetch_add(1, Ordering::Relaxed);
            return;
        }

        // Shrink candidates are gated on the same alive-bytes accounting that feeds
        // `total_rewrite_bytes`, so reaching here means that accounting is wrong.
        if Self::should_not_shrink(total_rewrite_bytes as u64, shrink_collect.written_bytes) {
            info!(
                "Unexpected shrink for slot {} rewrite bytes {} written {}, likely caused by a \
                 bug for calculating alive bytes.",
                slot, total_rewrite_bytes, shrink_collect.written_bytes
            );
            self.shrink_stats
                .skipped_shrink
                .fetch_add(1, Ordering::Relaxed);
            return;
        }

        let total_accounts_after_shrink = shrink_collect.alive_accounts.len();
        debug!(
            "shrinking: slot: {}, accounts: ({} => {}) bytes: {} original: {}",
            slot,
            shrink_collect.total_starting_accounts,
            total_accounts_after_shrink,
            shrink_collect.alive_total_bytes,
            shrink_collect.written_bytes,
        );

        let mut stats_sub = ShrinkStatsSub::default();
        let mut rewrite_elapsed = Measure::start("rewrite_elapsed");
        let (shrink_in_progress, time_us) = measure_us!(self.get_store_for_shrink(
            slot,
            Arc::clone(&store),
            total_rewrite_bytes as u64
        ));
        stats_sub.create_and_insert_store_elapsed_us = Saturating(time_us);

        // here, we're writing back alive_accounts. That should be an atomic operation
        // without use of rather wide locks in this whole function, because we're
        // mutating rooted slots; There should be no writers to them.
        let accounts = [(slot, &shrink_collect.alive_accounts.alive_accounts()[..])];
        let storable_accounts = StorableAccountsBySlot::new(slot, &accounts, self);
        stats_sub.store_accounts_stats =
            self.store_accounts_for_shrink(storable_accounts, shrink_in_progress.new_storage());

        let tombstone_refs: Vec<_> = shrink_collect.tombstones_to_carry_forward.iter().collect();
        let tombstone_accounts = [(slot, &tombstone_refs[..])];
        let storable_tombstones = StorableAccountsBySlot::new(slot, &tombstone_accounts, self);
        let (num_tombstones_carried_forward, tombstone_carry_forward_us) = measure_us!(
            self.store_tombstones(shrink_in_progress.new_storage(), storable_tombstones)
        );
        stats_sub.tombstone_carry_forward_us = Saturating(tombstone_carry_forward_us);
        stats_sub.num_tombstones_carried_forward =
            Saturating(num_tombstones_carried_forward as u64);

        // Count the bytes actually written to the new storage
        self.shrink_stats.bytes_written.fetch_add(
            shrink_in_progress.new_storage().written_bytes(),
            Ordering::Relaxed,
        );

        rewrite_elapsed.stop();
        stats_sub.rewrite_elapsed_us = Saturating(rewrite_elapsed.as_us());

        // `store_accounts_for_shrink()` above may have purged accounts from some
        // other storage entries (the ones that were just overwritten by this
        // new storage entry). This means some of those stores might have caused
        // this slot to be read to `self.shrink_candidate_slots`, so delete
        // those here
        self.shrink_candidate_slots.lock().unwrap().remove(&slot);

        self.remove_old_stores_shrink(slot, &self.shrink_stats, Some(shrink_in_progress), false);

        self.reopen_storage_as_readonly_shrinking_in_progress_ok(slot);

        self.shrink_stats.accumulate_sub_stats(stats_sub, true);
        self.shrink_stats.report();
    }

    /// get stores for 'slot'
    /// Drop 'shrink_in_progress', which will cause the old store to be removed from the storage map.
    /// For 'shrink_in_progress'.'old_storage' which is not retained, insert in 'dead_storages'
    /// This is the end of the life cycle of `shrink_in_progress`.
    pub fn mark_dirty_dead_stores(
        &self,
        slot: Slot,
        shrink_in_progress: Option<ShrinkInProgress>,
        shrink_can_be_active: bool,
    ) -> Vec<Arc<AccountStorageEntry>> {
        let mut dead_storages = Vec::default();

        let mut not_retaining_store = |store: &Arc<AccountStorageEntry>| {
            dead_storages.push(store.clone());
        };

        if let Some(shrink_in_progress) = shrink_in_progress {
            // shrink is in progress, so 1 new append vec to keep, 1 old one to throw away
            not_retaining_store(shrink_in_progress.old_storage());
            // dropping 'shrink_in_progress' removes the old append vec that was being shrunk from db's storage
        } else if let Some(store) = self.storage.remove(&slot, shrink_can_be_active) {
            // no shrink in progress, so all append vecs in this slot are dead
            not_retaining_store(&store);
        }

        dead_storages
    }

    /// we are done writing to the storage at `slot`. It can be re-opened as read-only if that would help
    /// system performance.
    pub(crate) fn reopen_storage_as_readonly_shrinking_in_progress_ok(&self, slot: Slot) {
        if let Some(storage) = self
            .storage
            .get_slot_storage_entry_shrinking_in_progress_ok(slot)
            && let Some(new_storage) = storage.reopen_as_readonly()
        {
            // consider here the race condition of tx processing having looked up something in the index,
            // which could return (slot, append vec id). We want the lookup for the storage to get a storage
            // that works whether the lookup occurs before or after the replace call here.
            // So, the two storages have to be exactly equivalent wrt offsets, counts, len, id, etc.
            assert_eq!(storage.id(), new_storage.id());
            assert_eq!(storage.accounts.len(), new_storage.accounts.len());
            self.storage
                .replace_storage_with_equivalent(slot, Arc::new(new_storage));
        }
    }

    /// return a store that can contain 'size' bytes
    pub fn get_store_for_shrink(
        &self,
        slot: Slot,
        old_store: Arc<AccountStorageEntry>,
        size: u64,
    ) -> ShrinkInProgress<'_> {
        let shrunken_store = Arc::new(self.create_store(slot, size));
        self.storage
            .shrinking_in_progress(slot, old_store, shrunken_store)
    }

    // Reads all accounts in given slot's AppendVecs and filter only to alive,
    // then create a minimum AppendVec filled with the alive.
    fn shrink_slot_forced(&self, slot: Slot) {
        debug!("shrink_slot_forced: slot: {slot}");

        if let Some(store) = self
            .storage
            .get_slot_storage_entry_shrinking_in_progress_ok(slot)
            && self.is_shrinking_productive(&store)
        {
            self.shrink_storage(store)
        }
    }

    fn all_slots_in_storage(&self) -> Vec<Slot> {
        self.storage.all_slots()
    }

    /// Given the input `ShrinkCandidates`, this function sorts the stores by their alive ratio
    /// in increasing order with the most sparse entries in the front. It will then simulate the
    /// shrinking by working on the most sparse entries first and if the overall alive ratio is
    /// achieved, it will stop and return:
    /// first tuple element: the filtered-down candidates and
    /// second duple element: the candidates which
    /// are skipped in this round and might be eligible for the future shrink.
    fn select_candidates_by_total_usage(
        &self,
        shrink_slots: &ShrinkCandidates,
        shrink_ratio: f64,
    ) -> (IntMap<Slot, Arc<AccountStorageEntry>>, ShrinkCandidates) {
        struct StoreUsageInfo {
            slot: Slot,
            alive_ratio: f64,
            alive_bytes_after_shrink: u64,
            store: Arc<AccountStorageEntry>,
        }
        let mut store_usages = Vec::with_capacity(shrink_slots.len());
        let mut total_alive_bytes: u64 = 0;
        let mut total_bytes: u64 = 0;
        for slot in shrink_slots {
            let Some(store) = self.storage.get_slot_storage_entry(*slot) else {
                continue;
            };
            let alive_bytes_after_shrink = self.alive_bytes_after_shrink(&store) as u64;
            total_alive_bytes += alive_bytes_after_shrink;
            let written_bytes = store.written_bytes();
            total_bytes += written_bytes;
            debug_assert!(
                written_bytes > 0,
                "shrink candidate has zero written bytes! slot: {slot} id: {}",
                store.id(),
            );
            let alive_ratio = alive_bytes_after_shrink as f64 / written_bytes as f64;
            store_usages.push(StoreUsageInfo {
                slot: *slot,
                alive_ratio,
                alive_bytes_after_shrink,
                store: store.clone(),
            });
        }
        store_usages.sort_by(|a, b| {
            a.alive_ratio
                .partial_cmp(&b.alive_ratio)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Working from the beginning of store_usage which are the most sparse and see when we can stop
        // shrinking while still achieving the overall goals.
        let mut shrink_slots = IntMap::default();
        let mut shrink_slots_next_batch = ShrinkCandidates::default();
        for store_usage in &store_usages {
            let store = &store_usage.store;
            let alive_ratio = (total_alive_bytes as f64) / (total_bytes as f64);
            debug!(
                "alive_ratio: {:?} store_id: {:?}, store_ratio: {:?} requirement: {:?}, \
                 total_bytes: {:?} total_alive_bytes: {:?}",
                alive_ratio,
                store_usage.store.id(),
                store_usage.alive_ratio,
                shrink_ratio,
                total_bytes,
                total_alive_bytes
            );
            if alive_ratio > shrink_ratio {
                // we have reached our goal, stop
                debug!(
                    "Shrinking goal can be achieved at slot {:?}, total_alive_bytes: {:?} \
                     total_bytes: {:?}, alive_ratio: {:}, shrink_ratio: {:?}",
                    store_usage.slot, total_alive_bytes, total_bytes, alive_ratio, shrink_ratio
                );
                if store_usage.alive_ratio < shrink_ratio {
                    shrink_slots_next_batch.insert(store_usage.slot);
                } else {
                    break;
                }
            } else {
                let current_store_size = store.written_bytes();
                let after_shrink_size = store_usage.alive_bytes_after_shrink;
                let bytes_saved = current_store_size.saturating_sub(after_shrink_size);
                total_bytes -= bytes_saved;
                shrink_slots.insert(store_usage.slot, Arc::clone(store));
            }
        }
        (shrink_slots, shrink_slots_next_batch)
    }

    /// return all slots that are more than one epoch old and thus could already be an ancient append vec
    /// or which could need to be combined into a new or existing ancient append vec
    /// offset is used to combine newer slots than we normally would. This is designed to be used for testing.
    fn get_sorted_potential_ancient_slots(&self, oldest_non_ancient_slot: Slot) -> Vec<Slot> {
        // Only storages can be combined into ancient append vecs, so the storage map is the
        // source of truth here.
        let mut ancient_slots = self.storage.slots_less_than(oldest_non_ancient_slot);
        ancient_slots.sort_unstable();
        ancient_slots
    }

    /// get a sorted list of slots older than an epoch
    /// squash those slots into ancient append vecs
    pub fn shrink_ancient_slots(&self, epoch_schedule: &EpochSchedule) {
        if self.ancient_append_vec_offset.is_none() {
            return;
        }

        let oldest_non_ancient_slot = self.get_oldest_non_ancient_slot(epoch_schedule);
        let can_randomly_shrink = true;
        let (sorted_slots, select_slots_us) =
            measure_us!(self.get_sorted_potential_ancient_slots(oldest_non_ancient_slot));
        self.shrink_ancient_stats
            .select_slots_us
            .fetch_add(select_slots_us, Ordering::Relaxed);
        self.combine_ancient_slots_packed(sorted_slots, can_randomly_shrink);
    }

    pub fn shrink_candidate_slots(&self, epoch_schedule: &EpochSchedule) -> usize {
        let oldest_non_ancient_slot = self.get_oldest_non_ancient_slot(epoch_schedule);

        let shrink_candidates_slots =
            std::mem::take(&mut *self.shrink_candidate_slots.lock().unwrap());
        self.shrink_stats
            .initial_candidates_count
            .store(shrink_candidates_slots.len() as u64, Ordering::Relaxed);

        let candidates_count = shrink_candidates_slots.len();
        let ((mut shrink_slots, shrink_slots_next_batch), select_time_us) = measure_us!({
            if let AccountShrinkThreshold::TotalSpace { shrink_ratio } = self.shrink_ratio {
                let (shrink_slots, shrink_slots_next_batch) =
                    self.select_candidates_by_total_usage(&shrink_candidates_slots, shrink_ratio);
                (shrink_slots, Some(shrink_slots_next_batch))
            } else {
                (
                    // lookup storage for each slot
                    shrink_candidates_slots
                        .into_iter()
                        .filter_map(|slot| {
                            self.storage
                                .get_slot_storage_entry(slot)
                                .map(|storage| (slot, storage))
                        })
                        .collect(),
                    None,
                )
            }
        });

        // If there are too few slots to shrink, add an ancient slot
        // for shrinking.
        if shrink_slots.len() < SHRINK_INSERT_ANCIENT_THRESHOLD {
            let mut ancients = self.best_ancient_slots_to_shrink.write().unwrap();
            while let Some((slot, written_bytes)) = ancients.pop_front() {
                if let Some(store) = self.storage.get_slot_storage_entry(slot)
                    && !shrink_slots.contains(&slot)
                    && written_bytes == store.written_bytes()
                    && self.is_candidate_for_shrink(&store)
                {
                    let ancient_bytes_added_to_shrink =
                        self.alive_bytes_after_shrink(&store) as u64;
                    shrink_slots.insert(slot, store);
                    self.shrink_stats
                        .ancient_bytes_added_to_shrink
                        .fetch_add(ancient_bytes_added_to_shrink, Ordering::Relaxed);
                    self.shrink_stats
                        .ancient_slots_added_to_shrink
                        .fetch_add(1, Ordering::Relaxed);
                    break;
                }
            }
        }
        if shrink_slots.is_empty()
            && shrink_slots_next_batch
                .as_ref()
                .map(|s| s.is_empty())
                .unwrap_or(true)
        {
            return 0;
        }

        let _guard = (!shrink_slots.is_empty())
            .then_some(|| self.active_stats.activate(ActiveStatItem::Shrink));

        let num_selected = shrink_slots.len();
        let (_, shrink_all_us) = measure_us!({
            self.thread_pool_background.install(|| {
                shrink_slots
                    .into_par_iter()
                    .for_each(|(slot, slot_shrink_candidate)| {
                        if self.ancient_append_vec_offset.is_some()
                            && slot < oldest_non_ancient_slot
                        {
                            self.shrink_stats
                                .num_ancient_slots_shrunk
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        self.shrink_storage(slot_shrink_candidate);
                    });
            })
        });

        let mut pended_counts: usize = 0;
        if let Some(shrink_slots_next_batch) = shrink_slots_next_batch {
            let mut shrink_slots = self.shrink_candidate_slots.lock().unwrap();
            pended_counts = shrink_slots_next_batch.len();
            for slot in shrink_slots_next_batch {
                shrink_slots.insert(slot);
            }
        }

        datapoint_info!(
            "shrink_candidate_slots",
            ("select_time_us", select_time_us, i64),
            ("shrink_all_us", shrink_all_us, i64),
            ("candidates_count", candidates_count, i64),
            ("selected_count", num_selected, i64),
            ("deferred_to_next_round_count", pended_counts, i64)
        );

        num_selected
    }

    /// This is only called at startup from bank when we are being extra careful such as when we downloaded a snapshot.
    /// Also called from tests.
    /// `newest_slot_skip_shrink_inclusive` is used to avoid shrinking the slot we are loading a snapshot from. If we shrink that slot, we affect
    /// the bank hash calculation verification at startup.
    pub fn shrink_all_slots(
        &self,
        is_startup: bool,
        newest_slot_skip_shrink_inclusive: Option<Slot>,
    ) {
        let _guard = self.active_stats.activate(ActiveStatItem::Shrink);
        const OUTER_CHUNK_SIZE: usize = 2000;
        let mut slots = self.all_slots_in_storage();
        if let Some(newest_slot_skip_shrink_inclusive) = newest_slot_skip_shrink_inclusive {
            // at startup, we cannot shrink the slot that we're about to replay and recalculate bank hash for.
            // That storage's contents are used to verify the bank hash (and accounts delta hash) of the startup slot.
            slots.retain(|slot| slot < &newest_slot_skip_shrink_inclusive);
        }

        if is_startup {
            let threads = num_cpus::get();
            let inner_chunk_size = std::cmp::max(OUTER_CHUNK_SIZE / threads, 1);
            slots.chunks(OUTER_CHUNK_SIZE).for_each(|chunk| {
                chunk.par_chunks(inner_chunk_size).for_each(|slots| {
                    for slot in slots {
                        self.shrink_slot_forced(*slot);
                    }
                });
            });
        } else {
            for slot in slots {
                self.shrink_slot_forced(slot);
            }
        }
    }

    /// Scans all accounts visible from `ancestors`, invoking `scan_func` for each.
    /// Pre-scans the write cache to capture entries not yet flushed to the accounts index, then
    /// deduplicates against the index scan, calling `scan_func` with the newest version of each
    /// account
    pub(crate) fn scan_accounts<F>(
        &self,
        ancestors: &Ancestors,
        bank_id: BankId,
        mut scan_func: F,
        config: &ScanConfig,
    ) -> ScanResult<()>
    where
        F: FnMut(Option<(&Pubkey, AccountSharedData, Slot)>),
    {
        // Register this scan so that slots needed by the scan are not cleaned out from under us.
        let mut scan_guard = ScanGuard::try_new(&self.scan_tracker, bank_id, || self.max_root())
            .ok_or(ScanError::SlotRemoved {
                slot: ancestors.max_slot(),
                bank_id,
            })?;

        // If the scan's ancestors are all rooted, drop them and scan roots only
        // Scan Guard max root must be used as the scan guard guarantees that
        // the account state as of max root is persisted in the database
        let max_root_ancestors = Ancestors::from(vec![scan_guard.max_root()]);
        let ancestors = if scan_guard.should_use_ancestors(ancestors) {
            ancestors
        } else {
            &max_root_ancestors
        };

        // Step 1: Pre-scan the cache index to find the newest visible cached version of each
        // pubkey. Hold the Arc<CachedAccount> to keep the data alive even if the cache flushes
        // between now and step 3 (Arc clone is just a refcount bump).
        let cached_pubkeys = self.accounts_cache.cached_pubkeys();
        let mut cached_versions = ahash::HashMap::with_capacity(cached_pubkeys.len());
        for pubkey in cached_pubkeys {
            if config.is_aborted() || scan_guard.is_bank_removed() {
                break;
            }

            if let Some((cached_account, slot)) =
                self.accounts_cache.load_latest(&pubkey, ancestors)
            {
                cached_versions.insert(pubkey, (cached_account, slot));
            }
        }

        // Step 2: Scan the accounts_index. For each pubkey, return the newest version found in
        // either the storage or the cache. If both versions are the same, use the cached version
        // to avoid a redundant load from storage.
        // Bound max_root by ancestors.min_slot() so that roots from slots
        // beyond the querying bank's ancestor chain are not visible.
        let mut max_root = scan_guard.max_root();
        if let Some(min) = ancestors.min_slot() {
            max_root = max_root.min(min);
        }
        self.accounts_index.scan_accounts(
            ancestors,
            max_root,
            |pubkey, (account_info, slot)| {
                if let Some((cached_account, cache_slot)) = cached_versions.remove(pubkey)
                    && cache_slot >= slot
                {
                    scan_func(Some((pubkey, cached_account.account.clone(), cache_slot)));
                    return;
                }

                let mut account_accessor =
                    self.get_account_accessor(slot, &account_info.storage_location());

                let account_slot = account_accessor.get_loaded_account(|loaded_account| {
                    (pubkey, loaded_account.take_account(), slot)
                });
                scan_func(account_slot)
            },
            || config.is_aborted() || scan_guard.is_bank_removed(),
        );

        // Step 3: Call scan_func on cache-only entries — pubkeys that exist in the cache but not
        // in the accounts index at all.
        for (pubkey, (cached_account, slot)) in cached_versions {
            if config.is_aborted() || scan_guard.is_bank_removed() {
                break;
            }
            scan_func(Some((&pubkey, cached_account.account.clone(), slot)));
        }

        // Check whether the bank was removed while the scan was in progress.
        if scan_guard.was_scan_corrupted() {
            return Err(ScanError::SlotRemoved {
                slot: ancestors.max_slot(),
                bank_id,
            });
        }
        Ok(())
    }

    pub(crate) fn index_scan_accounts<F>(
        &self,
        ancestors: &Ancestors,
        bank_id: BankId,
        index_key: IndexKey,
        mut scan_func: F,
        config: &ScanConfig,
    ) -> ScanResult<bool>
    where
        F: FnMut(Option<(&Pubkey, AccountSharedData, Slot)>),
    {
        let key = match &index_key {
            IndexKey::ProgramId(key) => key,
            IndexKey::SplTokenMint(key) => key,
            IndexKey::SplTokenOwner(key) => key,
        };
        if !self.account_indexes.include_key(key) {
            // the requested key was not indexed in the secondary index, so do a normal scan
            let used_index = false;
            self.scan_accounts(ancestors, bank_id, scan_func, config)?;
            return Ok(used_index);
        }

        // Register this scan so that slots needed by the scan are not cleaned out from under us.
        let mut scan_guard = ScanGuard::try_new(&self.scan_tracker, bank_id, || self.max_root())
            .ok_or(ScanError::SlotRemoved {
                slot: ancestors.max_slot(),
                bank_id,
            })?;

        // If the scan's ancestors are all rooted, drop them and scan roots only
        // Scan Guard max root must be used as the scan guard guarantees that
        // the account state as of max root is persisted in the database
        let max_root_ancestors = Ancestors::from(vec![scan_guard.max_root()]);
        let ancestors = if scan_guard.should_use_ancestors(ancestors) {
            ancestors
        } else {
            &max_root_ancestors
        };

        for pubkey in self.accounts_index.get_index_key_pubkeys(&index_key) {
            if config.is_aborted() || scan_guard.is_bank_removed() {
                break;
            }
            if let Some((account, slot)) = self.do_load(
                ancestors,
                &pubkey,
                LoadHint::Unspecified,
                PopulateReadCache::False,
                None::<fn(_, &_, _) -> _>,
            ) {
                scan_func(Some((&pubkey, account, slot)));
            }
        }

        // Check whether the bank was removed while the scan was in progress.
        if scan_guard.was_scan_corrupted() {
            return Err(ScanError::SlotRemoved {
                slot: ancestors.max_slot(),
                bank_id,
            });
        }
        let used_index = true;
        Ok(used_index)
    }

    /// Scan a specific slot through all the account storage
    pub(crate) fn scan_account_storage<R, B>(
        &self,
        slot: Slot,
        cache_map_func: impl Fn(&LoadedAccount) -> Option<R> + Sync,
        storage_scan_func: impl for<'a, 'b, 'storage> Fn(
            &'b mut B,
            &'a StoredAccountInfoWithoutData<'storage>,
            Option<&'storage [u8]>, // account data
        ) + Sync,
        scan_account_storage_data: ScanAccountStorageData,
    ) -> ScanStorageResult<R, B>
    where
        R: Send,
        B: Send + Default + Sync,
    {
        self.scan_cache_storage_fallback(slot, cache_map_func, |retval, storage| {
            match scan_account_storage_data {
                ScanAccountStorageData::NoData => {
                    storage.scan_accounts_without_data(|_offset, account_without_data| {
                        storage_scan_func(retval, &account_without_data, None);
                    })
                }
                ScanAccountStorageData::DataRefForStorage => {
                    let mut reader = append_vec::new_scan_accounts_reader();
                    storage.scan_accounts(&mut reader, |_offset, account| {
                        let account_without_data = StoredAccountInfoWithoutData::new_from(&account);
                        storage_scan_func(retval, &account_without_data, Some(account.data));
                    })
                }
            }
            .expect("must scan accounts storage");
        })
    }

    /// Scan the cache with a fallback to storage for a specific slot.
    pub fn scan_cache_storage_fallback<R, B>(
        &self,
        slot: Slot,
        cache_map_func: impl Fn(&LoadedAccount) -> Option<R> + Sync,
        storage_fallback_func: impl Fn(&mut B, &AccountStorageEntry) + Sync,
    ) -> ScanStorageResult<R, B>
    where
        R: Send,
        B: Send + Default + Sync,
    {
        if let Some(slot_cache) = self.accounts_cache.slot_cache(slot) {
            // If we see the slot in the cache, then all the account information
            // is in this cached slot
            if slot_cache.len() > SCAN_SLOT_PAR_ITER_THRESHOLD {
                ScanStorageResult::Cached(self.thread_pool_foreground.install(|| {
                    slot_cache
                        .par_iter()
                        .filter_map(|cached_account| {
                            cache_map_func(&LoadedAccount::Cached(Cow::Borrowed(
                                cached_account.value(),
                            )))
                        })
                        .collect()
                }))
            } else {
                ScanStorageResult::Cached(
                    slot_cache
                        .iter()
                        .filter_map(|cached_account| {
                            cache_map_func(&LoadedAccount::Cached(Cow::Borrowed(
                                cached_account.value(),
                            )))
                        })
                        .collect(),
                )
            }
        } else {
            let mut retval = B::default();
            // If the slot is not in the cache, then all the account information must have
            // been flushed. This is guaranteed because we only remove the rooted slot from
            // the cache *after* we've finished flushing in `flush_slot_cache`.
            // Regarding `shrinking_in_progress_ok`:
            // This fn could be running in the foreground, so shrinking could be running in the background, independently.
            // Even if shrinking is running, there will be 0-1 active storages to scan here at any point.
            // When a concurrent shrink completes, the active storage at this slot will
            // be replaced with an equivalent storage with only alive accounts in it.
            // A shrink on this slot could have completed anytime before the call here, a shrink could currently be in progress,
            // or the shrink could complete immediately or anytime after this call. This has always been true.
            // So, whether we get a never-shrunk, an about-to-be shrunk, or a will-be-shrunk-in-future storage here to scan,
            // all are correct and possible in a normally running system.
            if let Some(storage) = self
                .storage
                .get_slot_storage_entry_shrinking_in_progress_ok(slot)
            {
                storage_fallback_func(&mut retval, &storage);
            }

            ScanStorageResult::Stored(retval)
        }
    }

    /// note this returns None for accounts with zero lamports
    pub fn load(
        &self,
        ancestors: &Ancestors,
        pubkey: &Pubkey,
        load_hint: LoadHint,
        populate_read_cache: PopulateReadCache,
        load_filter: Option<impl Fn(u64, &Pubkey, usize) -> bool>,
    ) -> Option<(AccountSharedData, Slot)> {
        self.do_load(
            ancestors,
            pubkey,
            load_hint,
            populate_read_cache,
            load_filter,
        )
        .filter(|(account, _)| !account.is_zero_lamport())
    }

    fn read_index_for_accessor_or_load_slow<'a>(
        &'a self,
        ancestors: &Ancestors,
        pubkey: &'a Pubkey,
        clone_in_lock: bool,
    ) -> Option<(Slot, StorageLocation, Option<LoadedAccountAccessor>)> {
        self.accounts_index
            .get_with_and_then(pubkey, ancestors, true, |(slot, account_info)| {
                let storage_location = account_info.storage_location();
                let account_accessor =
                    clone_in_lock.then(|| self.get_account_accessor(slot, &storage_location));
                (slot, storage_location, account_accessor)
            })
    }

    fn retry_to_get_account_accessor<'a>(
        &'a self,
        mut slot: Slot,
        mut storage_location: StorageLocation,
        ancestors: &'a Ancestors,
        pubkey: &'a Pubkey,
        load_hint: LoadHint,
    ) -> Option<(LoadedAccountAccessor, Slot)> {
        // Happy drawing time! :)
        //
        // Reader                               | Accessed data source for stored
        // -------------------------------------+----------------------------------
        // R1 read_index_for_accessor_or_load_slow()| stored: index
        //          |                           |
        //        <(store_id, offset, ..)>      |
        //          V                           |
        // R2 retry_to_get_account_accessor()/  | stored: map of stores
        //        get_account_accessor()        |
        //          |                           |
        //        <Accessor>                    |
        //          V                           |
        // R3 check_and_get_loaded_account()/   | stored: store's entry for slot
        //        get_loaded_account()          |
        //          |                           |
        //        <LoadedAccount>               |
        //          V                           |
        // R4 take_account()                    | stored: entry of storage for (slot, pubkey)
        //          |                           |
        //        <AccountSharedData>           |
        //          V                           |
        //    Account!!                         V
        //
        // Flusher                              | Accessed data source for cached/stored
        // -------------------------------------+----------------------------------
        // F1 flush_slot_cache()                | N/A
        //          |                           |
        //          V                           |
        // F2 store_accounts_for_flush()/       | map of stores (creates new entry)
        //        write_accounts_to_storage()   |
        //          |                           |
        //          V                           |
        // F3 store_accounts_for_flush()/       | index
        //        update_index_stored_accounts()| (replaces existing store_id, offset in caches)
        //          |                           |
        //          V                           |
        // F4 accounts_cache.remove_slot()      | map of caches (removes old entry)
        //                                      V
        //
        // Remarks for flusher: So, for any reading operations, it's a race condition where F4 happens
        // between R1 and R2. In that case, retrying from R1 is safu because F3 should have
        // been occurred.
        //
        // Shrinker                             | Accessed data source for stored
        // -------------------------------------+----------------------------------
        // S1 do_shrink_slot_store()            | N/A
        //          |                           |
        //          V                           |
        // S2 store_accounts_for_shrink()/      | map of stores (creates new entry)
        //        write_accounts_to_storage()   |
        //          |                           |
        //          V                           |
        // S3 store_accounts_for_shrink()/      | index
        //        update_index_for_shrink()     | (replaces existing store_id, offset in stores)
        //          |                           |
        //          V                           |
        // S4 do_shrink_slot_store()/           | map of stores (removes old entry)
        //        dead_storages
        //
        // Remarks for shrinker: So, for any reading operations, it's a race condition
        // where S4 happens between R1 and R2. In that case, retrying from R1 is safu because S3 should have
        // been occurred, and S3 atomically replaced the index accordingly.
        //
        // Cleaner                              | Accessed data source for stored
        // -------------------------------------+----------------------------------
        // C1 clean_accounts()                  | N/A
        //          |                           |
        //          V                           |
        // C2 clean_accounts()/                 | index
        //        purge_keys_exact()            | (removes existing store_id, offset for stores)
        //          |                           |
        //          V                           |
        // C3 clean_accounts()/                 | map of stores (removes old entry)
        //        handle_reclaims()             |
        //
        // Remarks for cleaner: So, for any reading operations, it's a race condition
        // where C3 happens between R1 and R2. In that case, retrying from R1 is safu.
        // In that case, None would be returned while bailing out at R1.
        //
        // Purger                                 | Accessed data source for cached/stored
        // ---------------------------------------+----------------------------------
        // P1 purge_slot()                        | N/A
        //          |                             |
        //          V                             |
        // P2 purge_slots_from_cache()            | map of caches (removes old entry)
        //          |                             |
        //          V                             |
        // P3 clean_accounts()/                   | index
        //     handle_pubkeys_removed_from_cache()| (secondary index removal + write-through
        //                                        |  for pubkeys that P2 removed from the cache)
        //
        // Remarks for purger: So, for any reading operations, it's a race condition
        // where P2 happens between R1 and R2. In that case, retrying from R1 is safu,
        // and None would be returned: a purged (unrooted) slot is never present in the
        // primary index, so P3 (deferred to clean) is not a step readers race with.

        #[cfg(test)]
        {
            // Give some time for cache flushing to occur here for unit tests
            thread::sleep(Duration::from_millis(self.load_delay));
        }

        // Failsafe for potential race conditions with other subsystems
        let mut num_acceptable_failed_iterations = 0;
        loop {
            let account_accessor = self.get_account_accessor(slot, &storage_location);
            match account_accessor {
                LoadedAccountAccessor::Stored(Some(_)) => {
                    // Great! There was no race, just return :) This is the most usual situation
                    return Some((account_accessor, slot));
                }
                LoadedAccountAccessor::Stored(None) => {
                    match load_hint {
                        LoadHint::FixedMaxRoot => {
                            // When running replay on the validator, or banking stage on the leader,
                            // it should be very rare that the storage entry doesn't exist if the
                            // entry in the accounts index is the latest version of this account.
                            //
                            // There are only a few places where the storage entry may not exist
                            // after reading the index:
                            // 1) Shrink has removed the old storage entry and rewritten to
                            // a newer storage entry
                            // 2) The `pubkey` asked for in this function is a zero-lamport account,
                            // and the storage entry holding this account qualified for zero-lamport clean.
                            //
                            // In both these cases, it should be safe to retry and recheck the accounts
                            // index indefinitely, without incrementing num_acceptable_failed_iterations.
                            // That's because if the root is fixed, there should be a bounded number
                            // of pending cleans/shrinks (depends how far behind the AccountsBackgroundService
                            // is), termination to the desired condition is guaranteed.
                            //
                            // Also note that in both cases, if we do find the storage entry,
                            // we can guarantee that the storage entry is safe to read from because
                            // we grabbed a reference to the storage entry while it was still in the
                            // storage map. This means even if the storage entry is removed from the storage
                            // map after we grabbed the storage entry, the recycler should not reset the
                            // storage entry until we drop the reference to the storage entry.
                            //
                            // eh, no code in this arm? yes!
                        }
                        LoadHint::Unspecified => {
                            // RPC get_account() may have fetched an old root from the index that was
                            // either:
                            // 1) Cleaned up by clean_accounts(), so the accounts index has been updated
                            // and the storage entries have been removed.
                            // 2) Dropped by purge_slots() because the slot was on a minor fork, which
                            // removes the slots' storage entries but doesn't purge from the accounts index
                            // (account index cleanup is left to clean for stored slots). Note that
                            // this generally is impossible to occur in the wild because the RPC
                            // should hold the slot's bank, preventing it from being purged() to
                            // begin with.
                            num_acceptable_failed_iterations += 1;
                        }
                    }
                }
            }
            #[cfg(not(test))]
            let load_limit = ABSURD_CONSECUTIVE_FAILED_ITERATIONS;

            #[cfg(test)]
            let load_limit = self.load_limit.load(Ordering::Relaxed);

            let fallback_to_slow_path = if num_acceptable_failed_iterations >= load_limit {
                // The latest version of the account existed in the index, but could not be
                // fetched from storage. This means a race occurred between this function and clean
                // accounts/purge_slots
                let message = format!(
                    "do_load() failed to get key: {pubkey} from storage, latest attempt was for \
                     slot: {slot}, storage_location: {storage_location:?}, load_hint: \
                     {load_hint:?}",
                );
                datapoint_warn!("accounts_db-do_load_warn", ("warn", message, String));
                true
            } else {
                false
            };

            // Because reading from the cache/storage failed, retry from the index read
            let (new_slot, new_storage_location, maybe_account_accessor) = self
                .read_index_for_accessor_or_load_slow(ancestors, pubkey, fallback_to_slow_path)?;
            // Notice the subtle `?` at previous line, we bail out pretty early if missing.

            if new_slot == slot && new_storage_location.is_store_id_equal(&storage_location) {
                self.accounts_index
                    .get_and_then(pubkey, |entry| -> (_, ()) {
                        let message = format!(
                            "Bad index entry detected ({pubkey}, {slot}, {storage_location:?}, \
                             {load_hint:?}, {new_storage_location:?}, {entry:?})"
                        );
                        // Considering that we've failed to get accessor above and further that
                        // the index still returned the same (slot, store_id) tuple, offset must be same
                        // too.
                        assert!(
                            new_storage_location.is_offset_equal(&storage_location),
                            "{message}"
                        );

                        // If this is not a cache entry, then this was a minor fork slot
                        // that had its storage entries cleaned up by purge_slots() but hasn't been
                        // cleaned yet. That means this must be rpc access and not replay/banking at the
                        // very least. Note that purge shouldn't occur even for RPC as caller must hold all
                        // of ancestor slots..
                        assert_eq!(load_hint, LoadHint::Unspecified, "{message}");

                        // Everything being assert!()-ed, let's panic!() here as it's an error condition
                        // after all....
                        // That reasoning is based on the fact all of code-path reaching this fn
                        // retry_to_get_account_accessor() must outlive the Arc<Bank> (and its all
                        // ancestors) over this fn invocation, guaranteeing the prevention of being purged,
                        // first of all.
                        // For details, see the comment in ScanGuard::should_use_ancestors(),
                        // which is referring back here.
                        panic!("{message}");
                    });
            } else if fallback_to_slow_path {
                // the above bad-index-entry check must had been checked first to retain the same
                // behavior
                return Some((
                    maybe_account_accessor.expect("must be some if clone_in_lock=true"),
                    new_slot,
                ));
            }

            slot = new_slot;
            storage_location = new_storage_location;
        }
    }

    fn do_load(
        &self,
        ancestors: &Ancestors,
        pubkey: &Pubkey,
        load_hint: LoadHint,
        populate_read_cache: PopulateReadCache,
        load_filter: Option<impl Fn(u64, &Pubkey, usize) -> bool>,
    ) -> Option<(AccountSharedData, Slot)> {
        let starting_max_root = self.max_root();

        // Check the write cache first; a hit is the freshest version visible on this fork
        if let Some((cached_account, cached_slot)) =
            self.accounts_cache.load_latest(pubkey, ancestors)
        {
            self.load_account_stats
                .num_loaded_from_write_cache
                .fetch_add(1, Ordering::Relaxed);

            let account = &cached_account.account;
            let should_load_account = load_filter.as_ref().is_none_or(|load_filter| {
                load_filter(account.lamports(), account.owner(), account.data().len())
            });

            return should_load_account.then(|| (cached_account.account.clone(), cached_slot));
        }

        let (slot, storage_location, _maybe_account_accessor) =
            self.read_index_for_accessor_or_load_slow(ancestors, pubkey, false)?;
        // Notice the subtle `?` at previous line, we bail out pretty early if missing.

        let result = self.read_only_accounts_cache.load(*pubkey, slot);
        if let Some(account) = result {
            self.load_account_stats
                .num_loaded_from_read_cache
                .fetch_add(1, Ordering::Relaxed);

            let should_load_account = load_filter.as_ref().is_none_or(|load_filter| {
                load_filter(account.lamports(), account.owner(), account.data().len())
            });

            return should_load_account.then_some((account, slot));
        }

        let (mut account_accessor, slot) = self.retry_to_get_account_accessor(
            slot,
            storage_location,
            ancestors,
            pubkey,
            load_hint,
        )?;
        self.load_account_stats
            .num_loaded_from_index_storage
            .fetch_add(1, Ordering::Relaxed);

        let maybe_account =
            account_accessor.check_and_get_loaded_account_shared_data(load_filter.as_ref());

        if let Some(ref account) = maybe_account
            && populate_read_cache == PopulateReadCache::True
        {
            /*
            We show this store into the read-only cache for account 'A' and future loads of 'A' from the read-only cache are
            safe/reflect 'A''s latest state on this fork.
            This safety holds if during replay of slot 'S', we show we only read 'A' from the write cache,
            not the read-only cache, after it's been updated in replay of slot 'S'.
            Assume for contradiction this is not true, and we read 'A' from the read-only cache *after* it had been updated in 'S'.
            This means an entry '(S, A)' was added to the read-only cache after 'A' had been updated in 'S'.
            Now when '(S, A)' was being added to the read-only cache, it must have been true that  'is_cache == false',
            which means '(S', A)' does not exist in the write cache yet.
            However, by the assumption for contradiction above ,  'A' has already been updated in 'S' which means '(S, A)'
            must exist in the write cache, which is a contradiction.
            */
            self.read_only_accounts_cache
                .store(*pubkey, slot, account.clone());
        }
        if load_hint == LoadHint::FixedMaxRoot {
            // If the load hint is that the max root is fixed, the max root should be fixed.
            let ending_max_root = self.max_root();
            if starting_max_root != ending_max_root {
                warn!(
                    "do_load_with_populate_read_cache() scanning pubkey {pubkey} called with \
                     fixed max root, but max root changed from {starting_max_root} to \
                     {ending_max_root} during function call"
                );
            }
        }
        maybe_account.map(|account| (account, slot))
    }

    #[cfg_attr(test, qualifiers(pub(crate)))]
    fn get_account_accessor(
        &self,
        slot: Slot,
        storage_location: &StorageLocation,
    ) -> LoadedAccountAccessor {
        match storage_location {
            StorageLocation::AccountsFile(store_id, offset) => {
                let maybe_storage_entry = self
                    .storage
                    .get_account_storage_entry(slot, *store_id)
                    .map(|account_storage_entry| (account_storage_entry, *offset));
                LoadedAccountAccessor::Stored(maybe_storage_entry)
            }
        }
    }

    #[cfg_attr(test, qualifiers(pub(crate)))]
    fn create_store(&self, slot: Slot, size: u64) -> AccountStorageEntry {
        self.stats
            .create_store_count
            .fetch_add(1, Ordering::Relaxed);
        let paths = &self.paths;
        let path_index = rng().random_range(0..paths.len());
        AccountStorageEntry::new(
            Path::new(&paths[path_index]),
            slot,
            self.next_id(),
            size,
            self.accounts_file_provider,
        )
    }

    pub fn enable_bank_drop_callback(&self) {
        self.is_bank_drop_callback_enabled
            .store(true, Ordering::Release);
    }

    /// This should only be called after the `Bank::drop()` runs in bank.rs, See BANK_DROP_SAFETY
    /// comment below for more explanation.
    /// * `is_serialized_with_abs` - indicates whether this call runs sequentially
    ///   with all other accounts_db relevant calls, such as shrinking, purging etc.,
    ///   in accounts background service.
    pub fn purge_slot(&self, slot: Slot, bank_id: BankId, is_serialized_with_abs: bool) {
        if self.is_bank_drop_callback_enabled.load(Ordering::Acquire) && !is_serialized_with_abs {
            panic!(
                "bad drop callpath detected; Bank::drop() must run serially with other logic in \
                 ABS like clean_accounts()"
            )
        }

        // BANK_DROP_SAFETY: Because this function only runs once the bank is dropped,
        // we know that there are no longer any ongoing scans on this bank, because scans require
        // and hold a reference to the bank at the tip of the fork they're scanning. Hence it's
        // safe to remove this bank_id from the `removed_bank_ids` list at this point.
        if self
            .scan_tracker
            .removed_bank_ids
            .lock()
            .unwrap()
            .remove(&bank_id)
        {
            // If this slot was already cleaned up, no need to do any further cleans
            return;
        }

        self.purge_slots(std::iter::once(&slot));
    }

    /// Purges each slot in `removed_slots` from the write cache, and defers any pubkeys that
    /// were fully removed from the write cache to clean to handle removal from the secondary
    /// index. Slots no longer present in the cache are skipped. This never touches backing
    /// storage, so it cannot delete a flushed slot's data. Returns whether any slot was actually
    /// removed from the cache. This allows the snapshot minimizer to determine whether
    /// it should purge the storage as well
    fn purge_slots_from_cache<'a>(
        &self,
        removed_slots: impl Iterator<Item = &'a Slot>,
        purge_stats: &PurgeStats,
    ) -> bool {
        let mut remove_cache_elapsed_across_slots = 0;
        let mut num_cached_slots_removed = 0;
        let mut total_removed_cached_bytes = 0;
        for remove_slot in removed_slots {
            // This function runs in parallel with the ABS operations (flush, shrink, clean) and
            // must be safe with respect to them. ABS operations will not operate on this slot as
            // it is unrooted (unless the snapshot minimizer is being used).
            let mut remove_cache_elapsed = Measure::start("remove_cache_elapsed");
            if let Some(slot_cache) = self.accounts_cache.slot_cache(*remove_slot) {
                num_cached_slots_removed += 1;
                total_removed_cached_bytes += slot_cache.total_bytes();
                remove_cache_elapsed.stop();
                remove_cache_elapsed_across_slots += remove_cache_elapsed.as_us();
                // Nobody else should have removed the slot cache entry yet
                let pubkeys_removed = self
                    .accounts_cache
                    .remove_slot(*remove_slot)
                    .expect("slot cache entry must still be present");
                self.add_pubkeys_removed_from_cache(pubkeys_removed);
            }
        }

        purge_stats
            .remove_cache_elapsed
            .fetch_add(remove_cache_elapsed_across_slots, Ordering::Relaxed);
        purge_stats
            .num_cached_slots_removed
            .fetch_add(num_cached_slots_removed, Ordering::Relaxed);
        purge_stats
            .total_removed_cached_bytes
            .fetch_add(total_removed_cached_bytes, Ordering::Relaxed);

        num_cached_slots_removed > 0
    }

    /// Add any keys that were removed from the cache and need follow-up work by clean
    /// Only required if secondary indexes are enabled, or write-through is enabled
    fn add_pubkeys_removed_from_cache(&self, pubkeys: Vec<Pubkey>) {
        if self.account_indexes.is_empty() && !self.accounts_index.should_write_through() {
            return;
        }
        self.pubkeys_removed_from_cache
            .lock()
            .unwrap()
            .push(pubkeys);
    }

    /// For each pubkey in the list:
    /// 1. remove the pubkey from the secondary index if it is not present in either the cache
    ///    or the index
    /// 2. write-through to disk if the pubkey is dirty and not present in the cache
    fn handle_pubkeys_removed_from_cache(&self) {
        let pubkeys_removed_from_cache =
            mem::take(&mut *self.pubkeys_removed_from_cache.lock().unwrap());
        for mut pubkeys in pubkeys_removed_from_cache {
            if !self.account_indexes.is_empty() {
                let removed_keys = self.accounts_index.handle_dead_keys(&pubkeys);
                self.purge_secondary_indexes_for_dead_keys(&removed_keys);
            }

            // Write through any pubkey that hasn't been re-added to the cache in the meantime
            pubkeys.retain(|pubkey| !self.accounts_cache.contains_pubkey(pubkey));
            self.accounts_index.write_through_pubkeys(pubkeys);
        }
    }

    /// Purges every slot in `removed_slots` from both the cache and storage. This includes
    /// entries in the accounts index, cache entries, and any backing storage entries.
    ///
    /// This fn is to only be called by snapshot minimizer
    #[cfg(feature = "dev-context-only-utils")]
    pub fn purge_slots_for_snapshot_minimizer<'a>(
        &self,
        removed_slots: impl Iterator<Item = &'a Slot>,
    ) {
        let purge_stats = PurgeStats::default();
        for remove_slot in removed_slots {
            // Unlike the consensus purge paths, minimization may purge slots that have already
            // been flushed to storage, so fall back to purging storage for any slot that is no
            // longer in the cache.
            if !self.purge_slots_from_cache(iter::once(remove_slot), &purge_stats) {
                self.purge_slot_storage(*remove_slot, &purge_stats);
            }
        }
    }

    /// Purge the backing storage entries for the given slot, does not purge from
    /// the cache!
    fn purge_dead_slots_from_storage<'a>(
        &'a self,
        removed_slots: impl Iterator<Item = &'a Slot> + Clone,
        purge_stats: &PurgeStats,
    ) {
        let mut total_removed_stored_bytes = 0;
        let mut all_removed_slot_storages = vec![];

        let mut remove_storage_entries_elapsed = Measure::start("remove_storage_entries_elapsed");
        for remove_slot in removed_slots {
            // Remove the storage entries and collect some metrics
            if let Some(store) = self.storage.remove(remove_slot, false) {
                total_removed_stored_bytes += store.written_bytes();
                all_removed_slot_storages.push(store);
            }
        }
        remove_storage_entries_elapsed.stop();
        let num_stored_slots_removed = all_removed_slot_storages.len();

        // Backing mmaps for removed storages entries explicitly dropped here outside
        // of any locks
        let mut drop_storage_entries_elapsed = Measure::start("drop_storage_entries_elapsed");
        drop(all_removed_slot_storages);
        drop_storage_entries_elapsed.stop();

        purge_stats
            .remove_storage_entries_elapsed
            .fetch_add(remove_storage_entries_elapsed.as_us(), Ordering::Relaxed);
        purge_stats
            .drop_storage_entries_elapsed
            .fetch_add(drop_storage_entries_elapsed.as_us(), Ordering::Relaxed);
        purge_stats
            .num_stored_slots_removed
            .fetch_add(num_stored_slots_removed, Ordering::Relaxed);
        purge_stats
            .total_removed_stored_bytes
            .fetch_add(total_removed_stored_bytes, Ordering::Relaxed);
        self.stats
            .dropped_stores
            .fetch_add(num_stored_slots_removed as u64, Ordering::Relaxed);
    }

    #[cfg(feature = "dev-context-only-utils")]
    fn purge_slot_storage(&self, remove_slot: Slot, purge_stats: &PurgeStats) {
        // Because AccountsBackgroundService synchronously flushes from the accounts cache
        // and handles all Bank::drop() (the cleanup function that leads to this
        // function call), then we don't need to worry above an overlapping cache flush
        // with this function call. This means, if we get into this case, we can be
        // confident that the entire state for this slot has been flushed to the storage
        // already.
        let mut scan_storages_elapsed = Measure::start("scan_storages_elapsed");
        let mut stored_keys = ahash::HashSet::new();
        if let Some(storage) = self
            .storage
            .get_slot_storage_entry_shrinking_in_progress_ok(remove_slot)
        {
            storage
                .scan_accounts_without_data(|_offset, account| {
                    stored_keys.insert((*account.pubkey(), remove_slot));
                })
                .expect("must scan accounts storage");
        }
        scan_storages_elapsed.stop();
        purge_stats
            .scan_storages_elapsed
            .fetch_add(scan_storages_elapsed.as_us(), Ordering::Relaxed);

        let mut purge_accounts_index_elapsed = Measure::start("purge_accounts_index_elapsed");
        // Purge this slot from the accounts index
        let reclaims = self.purge_keys_exact(stored_keys);
        purge_accounts_index_elapsed.stop();
        purge_stats
            .purge_accounts_index_elapsed
            .fetch_add(purge_accounts_index_elapsed.as_us(), Ordering::Relaxed);

        // `handle_reclaims()` should remove all the account index entries and
        // storage entries
        let mut handle_reclaims_elapsed = Measure::start("handle_reclaims_elapsed");
        // There is no reason to mark accounts obsolete as the slot storage is being purged
        if !reclaims.is_empty() {
            let dead_slots =
                self.handle_reclaims(reclaims.iter(), purge_stats, MarkAccountsObsolete::No);
            // Ensure the expected slot is marked dead
            assert_eq!(dead_slots, IntSet::from_iter(iter::once(remove_slot)));
        } else if self
            .storage
            .get_slot_storage_entry(remove_slot)
            .is_some_and(|store| store.has_only_tombstones())
        {
            // A tombstone-only storage has no index entries to reclaim, so purge it directly
            self.purge_dead_slots_from_storage(iter::once(&remove_slot), purge_stats);
        }
        handle_reclaims_elapsed.stop();
        purge_stats
            .handle_reclaims_elapsed
            .fetch_add(handle_reclaims_elapsed.as_us(), Ordering::Relaxed);
        // After handling the reclaimed entries, this slot's
        // storage entries should be purged from self.storage
        assert!(
            self.storage.get_slot_storage_entry(remove_slot).is_none(),
            "slot {remove_slot} is not none"
        );
    }

    fn purge_slots<'a>(&self, slots: impl Iterator<Item = &'a Slot> + Clone) {
        // `add_root()` should be called first
        let mut safety_checks_elapsed = Measure::start("safety_checks_elapsed");
        let non_roots = slots
            // Only purge slots that are still in the write cache and are not
            // unflushed roots. Flushed roots have already been removed from the
            // cache, so the `contains` check excludes them; the
            // `contains_unflushed_root` check excludes roots still pending flush.
            //
            // Only safe to check when there are duplicate versions of a slot
            // because ReplayStage will not make new roots before dumping the
            // duplicate slots first. Thus we will not be in a case where we
            // root slot `S`, then try to dump some other version of slot `S`, the
            // dumping has to finish first
            //
            // Also note roots are never removed via `remove_unrooted_slot()`, so
            // it's safe to filter them out here as they won't need deletion from
            // self.scan_tracker.removed_bank_ids in
            // `purge_slots_from_cache()`.
            .filter(|slot| {
                self.accounts_cache.contains(**slot)
                    && !self.accounts_cache.contains_unflushed_root(**slot)
            });
        safety_checks_elapsed.stop();
        self.external_purge_slots_stats
            .safety_checks_elapsed
            .fetch_add(safety_checks_elapsed.as_us(), Ordering::Relaxed);
        self.purge_slots_from_cache(non_roots, &self.external_purge_slots_stats);
        self.external_purge_slots_stats
            .report("external_purge_slots_stats", Some(1000));
    }

    pub fn remove_unrooted_slots(&self, remove_slots: &[(Slot, BankId)]) {
        assert!(
            remove_slots.iter().all(|(slot, _)| {
                debug_assert!(
                    self.accounts_cache.contains(*slot),
                    "Trying to remove slot not in cache {slot}"
                );
                !self.accounts_cache.contains_unflushed_root(*slot)
            }),
            "Trying to remove accounts for rooted slots {remove_slots:?}"
        );

        // Mark down these slots are about to be purged so that new attempts to scan these
        // banks fail, and any ongoing scans over these slots abort promptly, releasing the
        // bank references their callers hold
        self.scan_tracker.mark_banks_removed(
            remove_slots
                .iter()
                .map(|(_slot, remove_bank_id)| *remove_bank_id),
        );

        let remove_unrooted_purge_stats = PurgeStats::default();
        self.purge_slots_from_cache(
            remove_slots.iter().map(|(slot, _)| slot),
            &remove_unrooted_purge_stats,
        );
        remove_unrooted_purge_stats.report("remove_unrooted_slots_purge_slots_stats", None);
    }

    /// Calculates the `AccountLtHash` of `account`
    pub fn lt_hash_account(account: &impl ReadableAccount, pubkey: &Pubkey) -> AccountLtHash {
        if account.lamports() == 0 {
            return ZERO_LAMPORT_ACCOUNT_LT_HASH;
        }

        let hasher = Self::write_account_hash_input(account, pubkey, blake3::Hasher::new());
        AccountLtHash(LtHash::with(&hasher))
    }

    /// Group-adds `account`'s lattice hash into `accumulator`, folded in once the
    /// batch flushes. Zero-lamport accounts hash to the identity and are skipped.
    ///
    /// The batch API only adds, so subtracting an account means feeding it into a
    /// separate accumulator and mixing that accumulator's final hash out.
    pub fn add_account_to_lt_hash(
        accumulator: &mut batch::Accumulator,
        account: &impl ReadableAccount,
        pubkey: &Pubkey,
    ) {
        if account.lamports() == 0 {
            return;
        }
        Self::write_account_hash_input(account, pubkey, accumulator.start_message()).finish();
    }

    /// The single source of truth for the per-account hash input layout.
    #[inline]
    fn write_account_hash_input<S: SingleLtHashUpdater>(
        account: &impl ReadableAccount,
        pubkey: &Pubkey,
        mut sink: S,
    ) -> S {
        sink.write_part(&account.lamports().to_le_bytes());
        sink.write_part(account.data());
        sink.write_part(&[u8::from(account.executable())]);
        sink.write_part(account.owner().as_ref());
        sink.write_part(pubkey.as_ref());
        sink
    }

    pub fn mark_slot_frozen(&self, slot: Slot) {
        if let Some(slot_cache) = self.accounts_cache.slot_cache(slot) {
            slot_cache.mark_slot_frozen();
            slot_cache.report_slot_store_metrics();
        }
        self.accounts_cache.report_size();
    }

    /// true if write cache is too big and there are unflushed roots available to flush.
    /// If there are no unflushed roots, we cannot reduce cache size because unrooted
    /// slots are not flushed.
    fn should_aggressively_flush_cache(&self) -> bool {
        self.write_cache_limit_bytes
            .unwrap_or(WRITE_CACHE_LIMIT_BYTES_DEFAULT)
            < self.accounts_cache.size()
            && self.accounts_cache.num_unflushed_roots() > 0
    }

    // `force_flush` flushes all the cached roots `<= requested_flush_root`. It also then
    // flushes excess remaining rooted slots while 'should_aggressively_flush_cache' is true
    pub fn flush_accounts_cache(&self, force_flush: bool, requested_flush_root: Option<Slot>) {
        #[cfg(not(test))]
        assert!(requested_flush_root.is_some());

        if !force_flush && !self.should_aggressively_flush_cache() {
            return;
        }

        // Flush only the roots <= requested_flush_root, so that snapshotting has all
        // the relevant roots in storage.
        let mut flush_roots_elapsed = Measure::start("flush_roots_elapsed");

        let _guard = self.active_stats.activate(ActiveStatItem::Flush);

        // Note even if force_flush is false, we will still flush all roots <= the
        // given `requested_flush_root`, even if some of the later roots cannot be used for
        // cleaning due to an ongoing scan
        let (total_new_cleaned_roots, num_cleaned_roots_flushed, mut flush_stats) =
            self.flush_rooted_accounts_cache_with_clean(requested_flush_root);
        flush_roots_elapsed.stop();

        // Note we don't purge unrooted slots here because there may be ongoing scans/references
        // for those slots, let the Bank::drop() implementation do cleanup instead on dead
        // banks

        // If 'should_aggressively_flush_cache', then flush the excess ones to storage
        let (total_new_excess_roots, num_excess_roots_flushed, flush_stats_aggressively) =
            if self.should_aggressively_flush_cache() {
                // Cannot do any cleaning on roots past `requested_flush_root` because future
                // snapshots may need updates from those later slots, hence we call the
                // without clean variant
                self.flush_rooted_accounts_cache_without_clean()
            } else {
                (0, 0, FlushStats::default())
            };
        flush_stats.accumulate(&flush_stats_aggressively);

        datapoint_info!(
            "accounts_db-flush_accounts_cache",
            ("total_new_cleaned_roots", total_new_cleaned_roots, i64),
            ("num_cleaned_roots_flushed", num_cleaned_roots_flushed, i64),
            ("total_new_excess_roots", total_new_excess_roots, i64),
            ("num_excess_roots_flushed", num_excess_roots_flushed, i64),
            ("flush_roots_elapsed", flush_roots_elapsed.as_us(), i64),
            ("account_bytes_stored", flush_stats.num_bytes_stored.0, i64),
            (
                "num_accounts_stored",
                flush_stats.num_accounts_stored.0,
                i64
            ),
            (
                "account_bytes_skipped",
                flush_stats.num_bytes_skipped.0,
                i64
            ),
            (
                "num_accounts_skipped",
                flush_stats.num_accounts_skipped.0,
                i64
            ),
            (
                "num_zero_lamport_accounts_skipped",
                flush_stats.num_zero_lamport_accounts_skipped.0,
                i64
            ),
            (
                "store_accounts_total_us",
                flush_stats.store_accounts_total_us.0,
                i64
            ),
            ("write_accounts_us", flush_stats.write_accounts_us.0, i64),
            ("update_index_us", flush_stats.update_index_us.0, i64),
            ("handle_reclaims_us", flush_stats.handle_reclaims_us.0, i64),
            (
                "num_tombstones_marked",
                flush_stats.num_tombstones_marked.0,
                i64
            ),
            ("num_reclaims", flush_stats.num_reclaims.0, i64),
            (
                "num_obsolete_slots_removed",
                flush_stats.num_obsolete_slots_removed.0,
                i64
            ),
            (
                "num_obsolete_bytes_removed",
                flush_stats.num_obsolete_bytes_removed.0,
                i64
            ),
            ("select_pubkeys_us", flush_stats.select_pubkeys_us.0, i64),
            (
                "disk_index_write_through_us",
                flush_stats.disk_index_write_through_us.0,
                i64
            ),
        );
    }

    /// Flush all rooted slots up to `requested_flush_root` with cleaning. Cleaning stores only
    /// the newest version of any given account when flushing multiple rooted slots. This reduces
    /// storage space and speeds up background account cleaning. If an ongoing scan is occurring,
    /// cleaning can only be done up to the minimum scan root to avoid cleaning accounts that may
    /// be needed by the scan.
    fn flush_rooted_accounts_cache_with_clean(
        &self,
        requested_flush_root: Option<Slot>,
    ) -> (usize, usize, FlushStats) {
        // If there is a long running scan going on, this could prevent any cleaning
        // based on updates from slots > `max_clean_root`.
        let max_clean_root = self.max_clean_root(requested_flush_root);
        self.flush_rooted_accounts_cache(
            requested_flush_root,
            FlushShouldClean::Yes { max_clean_root },
        )
    }

    // Flush all rooted slots without cleaning. This is used when rooted accounts must be flushed
    // to storage but older versions of the accounts are still required. For example, the
    // older versions may be needed for snapshotting.
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn flush_rooted_accounts_cache_without_clean(&self) -> (usize, usize, FlushStats) {
        self.flush_rooted_accounts_cache(None, FlushShouldClean::No)
    }

    /// Flush all rooted slots up to `requested_flush_root`.
    ///
    /// When `should_clean` is `Yes`, only the newest version of each account across the
    /// flushed roots (at or below its `max_clean_root`) is written to storage; older
    /// versions are purged from the index instead. When `No`, every account in
    /// every flushed root is written.
    fn flush_rooted_accounts_cache(
        &self,
        requested_flush_root: Option<Slot>,
        should_clean: FlushShouldClean,
    ) -> (usize, usize, FlushStats) {
        // Always flush up to `requested_flush_root`, which is necessary for things like snapshotting.
        let flushed_roots: BTreeSet<Slot> =
            self.accounts_cache.roots_to_flush(requested_flush_root);
        let max_flush_root = flushed_roots.last().copied();
        let num_new_roots = flushed_roots.len();

        // For each root being flushed, which of its cached accounts to write to storage.
        let (pubkeys_to_store, select_pubkeys_us) = match should_clean {
            FlushShouldClean::Yes { max_clean_root } => {
                measure_us!(self.select_pubkeys_to_store(&flushed_roots, max_clean_root))
            }
            // Not cleaning: every root writes all of its accounts.
            FlushShouldClean::No => (
                flushed_roots
                    .iter()
                    .map(|&root| (root, PubkeysToStore::All))
                    .collect(),
                0,
            ),
        };

        let mut num_roots_flushed = 0;
        let mut flush_stats = FlushStats {
            select_pubkeys_us: Saturating(select_pubkeys_us),
            ..FlushStats::default()
        };
        for root in flushed_roots {
            if let Some(stats) = self.flush_slot_cache(root, &pubkeys_to_store[&root]) {
                num_roots_flushed += 1;
                flush_stats.accumulate(&stats);
            } else {
                // A root with no cache to flush (e.g. genesis, whose accounts load straight to
                // storage) was still tracked by `add_root`. `flush_slot_cache` couldn't drop it
                // via `remove_slot`, so untrack it here to keep it from lingering as an unflushed
                // root at or below max_flushed_root.
                self.accounts_cache.remove_unflushed_root(root);
            }
        }

        max_flush_root.inspect(|&root| self.accounts_cache.set_max_flush_root(root));

        (num_new_roots, num_roots_flushed, flush_stats)
    }

    /// Determines which of each flushed root's accounts to write to storage when cleaning: each
    /// root keeps `Only` the newest version of each account, deduped newest-first. A root above
    /// `max_clean_root` instead flushes `All`, since an in-flight scan may still need those
    /// versions; `None` means there is no bound and every flushed root is cleaned.
    fn select_pubkeys_to_store(
        &self,
        flushed_roots: &BTreeSet<Slot>,
        max_clean_root: Option<Slot>,
    ) -> IntMap<Slot, PubkeysToStore> {
        let mut pubkeys_to_store = IntMap::with_capacity(flushed_roots.len());

        // Presize the dedup set from the newest root (flushed first), doubled to leave room
        // for unique accounts contributed by older roots.
        let dedup_capacity = flushed_roots
            .last()
            .and_then(|&root| self.accounts_cache.slot_cache(root))
            .map_or(0, |slot_cache| slot_cache.len() * 2);
        let mut written_accounts = ahash::HashSet::with_capacity(dedup_capacity);

        // Iterate from newest root to oldest root being flushed in this batch
        for &root in flushed_roots.iter().rev() {
            let cleaned = max_clean_root.is_none_or(|max_clean_root| root <= max_clean_root);
            let to_flush = if !cleaned {
                PubkeysToStore::All
            } else {
                let mut flush_keys = ahash::HashSet::default();
                if let Some(slot_cache) = self.accounts_cache.slot_cache(root) {
                    for entry in slot_cache.iter() {
                        let pubkey = *entry.key();
                        // If not seen in a newer root, this is the newest version, so flush it.
                        if written_accounts.insert(pubkey) {
                            flush_keys.insert(pubkey);
                        }
                    }
                }
                PubkeysToStore::Only(flush_keys)
            };
            pubkeys_to_store.insert(root, to_flush);
        }
        pubkeys_to_store
    }

    fn do_flush_slot_cache(
        &self,
        slot: Slot,
        slot_cache: &SlotCache,
        pubkeys_to_store: &PubkeysToStore,
    ) -> FlushStats {
        debug_assert!(self.accounts_cache.contains_unflushed_root(slot));
        let mut flush_stats = FlushStats::default();
        let mut skipped_zero_lamport_pubkeys = Vec::new();
        let iter_items: Vec<_> = slot_cache.iter().collect();

        // Use ReclaimOldSlots to reclaim old slots if marking obsolete accounts and cleaning.
        // Cleaning is enabled if pubkeys_to_store is PubkeysToStore::Only
        // pubkeys_to_store is PubkeysToStore::All when
        // 1) There's an ongoing scan to avoid reclaiming accounts being scanned.
        // 2) The slot is > max_clean_root to prevent unrooted slots from reclaiming rooted versions.
        let reclaim_method = match pubkeys_to_store {
            PubkeysToStore::Only(_) => UpsertReclaim::ReclaimOldSlots,
            PubkeysToStore::All => UpsertReclaim::IgnoreReclaims,
        };

        let accounts: Vec<(&Pubkey, &AccountSharedData)> = iter_items
            .iter()
            .filter_map(|iter_item| {
                let key = iter_item.key();
                let account = &iter_item.value().account;
                let mut should_store = match pubkeys_to_store {
                    PubkeysToStore::All => true,
                    PubkeysToStore::Only(store_keys) => store_keys.contains(key),
                };
                // `true` keeps a disk-loaded entry in-mem for the index upsert below
                if should_store
                    && account.is_zero_lamport()
                    && !self
                        .accounts_index
                        .get_and_then(key, |entry| (true, entry.is_some()))
                {
                    // A zero-lamport account with no index entry has no older rooted version
                    // in storage to shadow, so it can just be skipped
                    flush_stats.num_zero_lamport_accounts_skipped += 1;
                    if !self.account_indexes.is_empty() {
                        skipped_zero_lamport_pubkeys.push(*key);
                    }
                    should_store = false;
                }
                if should_store {
                    flush_stats.num_bytes_stored +=
                        AppendVec::calculate_stored_size(account.data().len()) as u64;
                    flush_stats.num_accounts_stored += 1;
                    if account.is_zero_lamport() && reclaim_method == UpsertReclaim::ReclaimOldSlots
                    {
                        // Stored zero-lamport accounts are deleted from the index and
                        // marked as tombstones in the flushed storage
                        flush_stats.num_tombstones_marked += 1;
                    }
                    Some((key, account))
                } else {
                    // Skip writing this account. Either superseded or zero lamport
                    flush_stats.num_bytes_skipped +=
                        AppendVec::calculate_stored_size(account.data().len()) as u64;
                    flush_stats.num_accounts_skipped += 1;
                    None
                }
            })
            .collect();

        if !accounts.is_empty() {
            let (store_accounts_for_flush_stats, store_accounts_for_flush_us) =
                measure_us!(self.store_accounts_for_flush(
                    (slot, &accounts[..]),
                    flush_stats.num_bytes_stored.0,
                    reclaim_method,
                ));
            flush_stats.accumulate_store_accounts_for_flush(store_accounts_for_flush_stats);
            flush_stats.store_accounts_total_us += Saturating(store_accounts_for_flush_us);
        }

        // Remove this slot from the cache, which will to AccountsDb's new readers should look like an
        // atomic switch from the cache to storage.
        // There is some racy condition for existing readers who just has read exactly while
        // flushing. That case is handled by retry_to_get_account_accessor()
        let pubkeys_removed = self
            .accounts_cache
            .remove_slot(slot)
            .expect("slot must be in the cache when flushing");

        // Zero-lamport accounts that were skipped above were never added to the primary
        // index, so their secondary index entries may be purgeable.
        self.purge_secondary_indexes_for_dead_keys(&skipped_zero_lamport_pubkeys);

        // Now that this slot has left the cache, any pubkey that no longer appears
        // in any cached slot is eligible to be written through so its in-mem entry
        // becomes clean and can be evicted.
        let (_, disk_index_write_through_us) =
            measure_us!(self.accounts_index.write_through_pubkeys(pubkeys_removed));
        flush_stats.disk_index_write_through_us = Saturating(disk_index_write_through_us);
        if reclaim_method == UpsertReclaim::ReclaimOldSlots {
            // Zero lamport accounts were deleted from the index by update_index_for_flush, so
            // their secondary index entries may be purgeable.
            self.purge_secondary_indexes_for_dead_keys(
                accounts
                    .iter()
                    .filter_map(|(pubkey, account)| account.is_zero_lamport().then_some(*pubkey)),
            );
        } else {
            // Add `accounts` to uncleaned_pubkeys since they were written to storage
            // without cleaning and should be visited by `clean`.
            self.uncleaned_pubkeys
                .entry(slot)
                .or_default()
                .extend(accounts.into_iter().map(|(pubkey, _account)| *pubkey));
        }

        flush_stats
    }

    /// `pubkeys_to_store` selects which accounts are written to storage: `Only(set)` stores
    /// just the pubkeys in the set, dropping the rest with the cache, while `All` stores every
    /// account in the slot.
    fn flush_slot_cache(
        &self,
        slot: Slot,
        pubkeys_to_store: &PubkeysToStore,
    ) -> Option<FlushStats> {
        // If a slot cache exists for this slot, flush it.
        self.accounts_cache
            .slot_cache(slot)
            .map(|slot_cache| self.do_flush_slot_cache(slot, &slot_cache, pubkeys_to_store))
    }

    fn report_store_stats(&self) {
        let mut total_count = 0;
        let mut newest_slot = 0;
        let mut oldest_slot = u64::MAX;
        let mut total_bytes = 0;
        let mut total_alive_bytes = 0;
        for (slot, store) in self.storage.iter() {
            total_count += 1;
            newest_slot = std::cmp::max(newest_slot, slot);

            oldest_slot = std::cmp::min(oldest_slot, slot);

            total_alive_bytes += store.alive_bytes();
            total_bytes += store.written_bytes();
        }
        info!(
            "total_stores: {total_count}, newest_slot: {newest_slot}, oldest_slot: {oldest_slot}"
        );

        let total_alive_ratio = if total_bytes > 0 {
            total_alive_bytes as f64 / total_bytes as f64
        } else {
            0.
        };

        datapoint_info!(
            "accounts_db-stores",
            ("total_count", total_count, i64),
            ("total_bytes", total_bytes, i64),
            ("total_alive_bytes", total_alive_bytes, i64),
            ("total_alive_ratio", total_alive_ratio, f64),
            (
                "append_vecs_open",
                append_vec::APPEND_VEC_STATS
                    .files_open
                    .load(Ordering::Relaxed),
                i64
            ),
            (
                "append_vecs_dirty",
                append_vec::APPEND_VEC_STATS
                    .files_dirty
                    .load(Ordering::Relaxed),
                i64
            ),
        );
    }

    /// Calculates the accounts lt hash
    ///
    /// Only intended to be called at startup (or by tests).
    /// Only intended to be used while testing the experimental accumulator hash.
    /// NOT safe to call concurrently with flush operations
    pub fn calculate_accounts_lt_hash_at_startup_from_index(
        &self,
        ancestors: &Ancestors,
    ) -> AccountsLtHash {
        // This impl iterates over all the index bins in parallel, and computes the lt hash
        // sequentially per bin.  Then afterwards reduces to a single lt hash.
        // This implementation is quite fast.  Runtime is about 150 seconds on mnb as of 10/2/2024.
        // The sequential implementation took about 6,275 seconds!
        // A different parallel implementation that iterated over the bins *sequentially* and then
        // hashed the accounts *within* a bin in parallel took about 600 seconds.  That impl uses
        // less memory, as only a single index bin is loaded into mem at a time.
        let mut lt_hash = self
            .accounts_index
            .account_maps
            .par_iter()
            .fold(
                LtHash::identity,
                |mut accumulator_lt_hash, accounts_index_bin| {
                    for pubkey in accounts_index_bin.keys() {
                        let account_lt_hash = self
                            .accounts_index
                            .get_with_and_then(&pubkey, ancestors, false, |(slot, account_info)| {
                                (!account_info.is_zero_lamport()).then(|| {
                                    self.get_account_accessor(
                                        slot,
                                        &account_info.storage_location(),
                                    )
                                    .get_loaded_account(|loaded_account| {
                                        Self::lt_hash_account(&loaded_account, &pubkey)
                                    })
                                    // SAFETY: The index said this pubkey exists, so
                                    // there must be an account to load.
                                    .unwrap()
                                })
                            })
                            .flatten();
                        if let Some(account_lt_hash) = account_lt_hash {
                            accumulator_lt_hash.mix_in(&account_lt_hash.0);
                        }
                    }
                    accumulator_lt_hash
                },
            )
            .reduce(LtHash::identity, |mut accum, elem| {
                accum.mix_in(&elem);
                accum
            });

        let cache_lt_hash = {
            let mut cache_lt_hash = LtHash::identity();
            for pubkey in self.accounts_cache.cached_pubkeys().iter() {
                // mix out whatever older version the index walk produced (if any)
                self.accounts_index.get_with_and_then(
                    pubkey,
                    ancestors,
                    false,
                    |(slot, account_info)| {
                        self.get_account_accessor(slot, &account_info.storage_location())
                            .get_loaded_account(|loaded_account| {
                                cache_lt_hash
                                    .mix_out(&Self::lt_hash_account(&loaded_account, pubkey).0);
                            });
                    },
                );
                // mix in the cache version
                if let Some((account, _slot)) = self.load(
                    ancestors,
                    pubkey,
                    LoadHint::FixedMaxRoot,
                    PopulateReadCache::False,
                    None::<fn(_, &_, _) -> _>,
                ) {
                    cache_lt_hash.mix_in(&Self::lt_hash_account(&account, pubkey).0);
                }
            }
            cache_lt_hash
        };
        lt_hash.mix_in(&cache_lt_hash);

        AccountsLtHash(lt_hash)
    }

    /// Calculates the capitalization
    ///
    /// Panics if capitalization overflows a u64.
    ///
    /// Note, this is *very* expensive!  It walks the whole accounts index,
    /// account-by-account, summing each account's balance.
    ///
    /// Only intended to be called at startup by ledger-tool or tests.
    pub fn calculate_capitalization_at_startup_from_index(&self, ancestors: &Ancestors) -> u64 {
        let stored_lamports = |pubkey: &Pubkey| {
            self.accounts_index
                .get_with_and_then(pubkey, ancestors, false, |(slot, account_info)| {
                    (!account_info.is_zero_lamport()).then(|| {
                        self.get_account_accessor(slot, &account_info.storage_location())
                            .get_loaded_account(|loaded_account| loaded_account.lamports())
                            // SAFETY: The index said this pubkey exists, so
                            // there must be an account to load.
                            .unwrap()
                    })
                })
                .flatten()
                .unwrap_or(0)
        };

        let storage_capitialization = self
            .accounts_index
            .account_maps
            .par_iter()
            .map(|accounts_index_bin| {
                accounts_index_bin
                    .keys()
                    .into_iter()
                    .map(|pubkey| stored_lamports(&pubkey))
                    .try_fold(0, u64::checked_add)
            })
            .try_reduce(|| 0, u64::checked_add)
            .expect("capitalization cannot overflow");

        // Sum as i128 because there is potential (although unlikely) for the cache updates to
        // overflow i64::MAX. For example, if the cache has multiple transactions that transfer a
        // large amount of lamports from one account to another, it could sum all of the transfers
        // from accounts first, overflow i128. Wrapping logic could also handle this properly (ie.
        // come to the correct answer), but then detection of overflow would be broken.
        let cached_update = self
            .accounts_cache
            .cached_pubkeys()
            .iter()
            .map(|pubkey| {
                // subtract out whatever older version the index walk produced (if any)
                let stored_lamports = stored_lamports(pubkey);

                // add in the cached amount of lamports
                let cached_lamports = self
                    .load(
                        ancestors,
                        pubkey,
                        LoadHint::FixedMaxRoot,
                        PopulateReadCache::False,
                        None::<fn(_, &_, _) -> _>,
                    )
                    .map(|(account, _slot)| account.lamports())
                    .unwrap_or(0);

                cached_lamports as i128 - stored_lamports as i128
            })
            .sum::<i128>();

        i128::from(storage_capitialization)
            .checked_add(cached_update)
            .and_then(|result| u64::try_from(result).ok())
            .expect("capitalization cannot overflow")
    }

    /// return slot + offset, where offset can be +/-
    fn apply_offset_to_slot(slot: Slot, offset: i64) -> Slot {
        if offset > 0 {
            slot.saturating_add(offset as u64)
        } else {
            slot.saturating_sub(offset.unsigned_abs())
        }
    }

    /// Return all of the accounts for a given slot
    pub fn get_pubkey_account_for_slot(&self, slot: Slot) -> Vec<(Pubkey, AccountSharedData)> {
        let scan_result = self.scan_account_storage(
            slot,
            |loaded_account| {
                // Cache only has one version per key, don't need to worry about versioning
                Some((*loaded_account.pubkey(), loaded_account.take_account()))
            },
            |accum: &mut ahash::HashMap<_, _>, stored_account, data| {
                // SAFETY: We called scan_account_storage() with
                // ScanAccountStorageData::DataRefForStorage, so `data` must be Some.
                let data = data.unwrap();
                let loaded_account =
                    LoadedAccount::Stored(StoredAccountInfo::new_from(stored_account, data));
                // Storage may have duplicates so only keep the latest version for each key
                accum.insert(*loaded_account.pubkey(), loaded_account.take_account());
            },
            ScanAccountStorageData::DataRefForStorage,
        );

        match scan_result {
            ScanStorageResult::Cached(cached_result) => cached_result,
            ScanStorageResult::Stored(stored_result) => stored_result.into_iter().collect(),
        }
    }

    /// Updates the secondary index with the given accounts. If store_account is false, skip storing
    /// the account in the secondary index as the account was not stored in the cache
    /// Used for cached accounts only.
    fn update_secondary_index_cached_accounts<'a>(
        &self,
        accounts: &impl StorableAccounts<'a>,
        store_account: &BitVec,
    ) {
        if !self.account_indexes.is_empty() {
            assert_eq!(accounts.len() as u64, store_account.len());
            for i in 0..accounts.len() {
                if store_account[i as u64] {
                    let pubkey = accounts.pubkey(i);
                    accounts.account(i, |account| {
                        self.accounts_index.update_secondary_indexes(
                            pubkey,
                            &account,
                            &self.account_indexes,
                        );
                    });
                }
            }
        }
    }

    /// Updates the accounts index with the given `infos` and `accounts`.
    /// Used when storing accounts to storage for flush.
    /// Returns a vector of `SlotList<AccountInfo>` containing the reclaims for each batch processed.
    /// The element of the returned vector is guaranteed to be non-empty.
    fn update_index_for_flush<'a>(
        &self,
        infos: Vec<AccountInfo>,
        accounts: &impl StorableAccounts<'a>,
        reclaim: UpsertReclaim,
    ) -> Vec<ReclaimsSlotList<AccountInfo>> {
        let target_slot = accounts.target_slot();
        let len = std::cmp::min(accounts.len(), infos.len());

        let update = |start, end| {
            let mut reclaims = ReclaimsSlotList::with_capacity((end - start) / 2);

            (start..end).for_each(|i| {
                let info: AccountInfo = infos[i];
                let pubkey = accounts.pubkey(i);
                if info.is_zero_lamport() && reclaim == UpsertReclaim::ReclaimOldSlots {
                    self.accounts_index.delete(pubkey, &mut reclaims);
                    // The account's own newest entry: a reclaim at the flushed slot,
                    // which handle_reclaims records as a tombstone in the flushed
                    // storage instead of marking it obsolete
                    reclaims.push((target_slot, info));
                    return;
                }
                let old_slot = accounts.slot(i);
                self.accounts_index.upsert(
                    target_slot,
                    old_slot,
                    pubkey,
                    info,
                    &mut reclaims,
                    reclaim,
                );

                if !self.account_indexes.is_empty() {
                    // Since StorableAccounts::account() may read the account from disk,
                    // avoid calling it unless secondary indexes are enabled.
                    accounts.account(i, |account| {
                        self.accounts_index.update_secondary_indexes(
                            pubkey,
                            &account,
                            &self.account_indexes,
                        );
                    });
                }
            });
            reclaims
        };

        let threshold = 1;
        if len > threshold {
            let thread_pool = &self.thread_pool_background;
            let chunk_size = len.div_ceil(thread_pool.current_num_threads());
            let batches = 1 + len / chunk_size;
            thread_pool.install(|| {
                (0..batches)
                    .into_par_iter()
                    .map(|batch| {
                        let start = batch * chunk_size;
                        let end = std::cmp::min(start + chunk_size, len);
                        update(start, end)
                    })
                    .filter(|reclaims| !reclaims.is_empty())
                    .collect()
            })
        } else {
            let reclaims = update(0, len);
            if reclaims.is_empty() {
                // If no reclaims, return an empty vector
                vec![]
            } else {
                vec![reclaims]
            }
        }
    }

    /// Updates the accounts index for the shrink path: each account at `accounts.slot(i)` has
    /// its existing index entry replaced to point at the rewritten storage at `target_slot`.
    ///
    /// Unlike `update_index_stored_accounts` this does not collect reclaims — the caller is
    /// responsible for the source storage's alive-bytes accounting. Secondary indexes are also
    /// not touched, since shrink only changes `(store_id, offset)` and they index by pubkey.
    fn update_index_for_shrink<'a>(
        &self,
        infos: &[AccountInfo],
        accounts: &impl StorableAccounts<'a>,
    ) {
        let target_slot = accounts.target_slot();
        let len = std::cmp::min(accounts.len(), infos.len());

        let update = |start, end| {
            (start..end).for_each(|i| {
                let info: AccountInfo = infos[i];
                let old_slot = accounts.slot(i);
                let pubkey = accounts.pubkey(i);
                self.accounts_index
                    .replace(target_slot, old_slot, pubkey, info);
            });
        };

        let threshold = 1;
        if len > threshold {
            let thread_pool = &self.thread_pool_background;
            let chunk_size = len.div_ceil(thread_pool.current_num_threads());
            let batches = 1 + len / chunk_size;
            thread_pool.install(|| {
                (0..batches).into_par_iter().for_each(|batch| {
                    let start = batch * chunk_size;
                    let end = std::cmp::min(start + chunk_size, len);
                    update(start, end)
                })
            });
        } else {
            update(0, len);
        }
    }

    fn should_not_shrink(alive_bytes: u64, total_bytes: u64) -> bool {
        alive_bytes >= total_bytes
    }

    /// Can zero lamport accounts in `slot` be purged?
    fn can_purge_zero_lamport_accounts(&self, slot: Slot) -> bool {
        self.latest_full_snapshot_slot()
            .is_none_or(|latest_full_snapshot_slot| slot <= latest_full_snapshot_slot)
    }

    /// Returns the expected alive bytes after shrinking `store`.
    pub(crate) fn alive_bytes_after_shrink(&self, store: &AccountStorageEntry) -> usize {
        // Obsolete accounts are already excluded from `store.alive_bytes()`.
        // Tombstones are counted as alive until shrink can purge them,
        // which is gated by the latest full snapshot slot.
        if self.can_purge_zero_lamport_accounts(store.slot()) {
            store.alive_bytes_exclude_zero_lamport_accounts()
        } else {
            store.alive_bytes()
        }
    }

    fn is_shrinking_productive(&self, store: &AccountStorageEntry) -> bool {
        let alive_count = store.count();
        let total_bytes = store.written_bytes();
        let alive_bytes = self.alive_bytes_after_shrink(store) as u64;
        if Self::should_not_shrink(alive_bytes, total_bytes) {
            trace!(
                "shrink_slot_forced ({}): not able to shrink at all: num alive: {}, bytes alive: \
                 {}, bytes total: {}, bytes saved: {}",
                store.slot(),
                alive_count,
                alive_bytes,
                total_bytes,
                total_bytes.saturating_sub(alive_bytes),
            );
            return false;
        }

        true
    }

    /// Determines whether a given AccountStorageEntry instance is a
    /// candidate for shrinking.
    pub(crate) fn is_candidate_for_shrink(&self, store: &AccountStorageEntry) -> bool {
        let total_bytes = store.written_bytes();
        let alive_bytes = self.alive_bytes_after_shrink(store) as u64;
        match self.shrink_ratio {
            AccountShrinkThreshold::TotalSpace { shrink_ratio: _ } => alive_bytes < total_bytes,
            AccountShrinkThreshold::IndividualStore { shrink_ratio } => {
                (alive_bytes as f64 / total_bytes as f64) < shrink_ratio
            }
        }
    }

    /// returns the dead slots
    fn remove_dead_accounts<'a, I>(
        &'a self,
        reclaims: I,
        mark_accounts_obsolete: MarkAccountsObsolete,
    ) -> IntSet<Slot>
    where
        I: Iterator<Item = &'a (Slot, AccountInfo)>,
    {
        let mut reclaimed_offsets = SlotOffsets::default();

        assert!(self.storage.no_shrink_in_progress());

        let mut dead_slots = IntSet::default();
        let mut new_shrink_candidates = ShrinkCandidates::default();
        let mut measure = Measure::start("remove");
        for (slot, account_info) in reclaims {
            reclaimed_offsets
                .entry(*slot)
                .or_default()
                .insert(account_info.offset());
        }

        self.clean_accounts_stats
            .slots_cleaned
            .fetch_add(reclaimed_offsets.len() as u64, Ordering::Relaxed);

        reclaimed_offsets.into_iter().for_each(|(slot, offsets)| {
            if let Some(store) = self.storage.get_slot_storage_entry(slot) {
                assert_eq!(
                    slot,
                    store.slot(),
                    "AccountsDB::accounts_index corrupted. Storage pointed to: {}, expected: {}, \
                     should only point to one slot",
                    store.slot(),
                    slot
                );

                // Reclaims at the slot itself are tombstones, not obsolete accounts
                let is_tombstone_reclaim =
                    mark_accounts_obsolete == MarkAccountsObsolete::Yes(slot);

                let remaining_accounts = if is_tombstone_reclaim {
                    // Tombstones stay alive in the storage; only record their offsets
                    store.batch_insert_tombstone_offsets(offsets);
                    store.count()
                } else if offsets.len() == store.count() {
                    // all remaining alive accounts in the storage are being removed, so the entire storage/slot is dead
                    store.remove_accounts(store.alive_bytes(), offsets.len())
                } else {
                    // not all accounts are being removed, so figure out sizes of accounts we are removing and update the alive bytes and alive account count
                    let (remaining_accounts, us) = measure_us!({
                        let mut offsets = offsets.iter().cloned().collect::<Vec<_>>();
                        // sort so offsets are in order. This improves efficiency of loading the accounts.
                        offsets.sort_unstable();
                        let data_lens = store.accounts.get_account_data_lens(&offsets);
                        let dead_bytes = data_lens
                            .iter()
                            .map(|len| store.accounts.calculate_stored_size(*len))
                            .sum();
                        let remaining_accounts = store.remove_accounts(dead_bytes, offsets.len());

                        if let MarkAccountsObsolete::Yes(slot_marked_obsolete) =
                            mark_accounts_obsolete
                        {
                            store
                                .obsolete_accounts
                                .write()
                                .unwrap()
                                .mark_accounts_obsolete(
                                    offsets.into_iter().zip(data_lens),
                                    slot_marked_obsolete,
                                );
                        }
                        remaining_accounts
                    });
                    self.clean_accounts_stats
                        .get_account_sizes_us
                        .fetch_add(us, Ordering::Relaxed);
                    remaining_accounts
                };

                // Check if we have removed all accounts from the storage, or just have
                // a storage composed of purgable tombstones
                // This may be different from the check above as this
                // can be multithreaded
                if remaining_accounts == 0
                    || (remaining_accounts == store.num_tombstones()
                        && self.can_purge_zero_lamport_accounts(slot))
                {
                    // Every remaining account is a tombstone and the slot is older than
                    // the latest full snapshot slot, safe to remove
                    dead_slots.insert(slot);
                } else if self.is_shrinking_productive(&store)
                    && self.is_candidate_for_shrink(&store)
                {
                    // Checking that this single storage entry is ready for shrinking,
                    // should be a sufficient indication that the slot is ready to be shrunk
                    // because slots should only have one storage entry, namely the one that was
                    // created by `flush_slot_cache()`.
                    new_shrink_candidates.insert(slot);
                }
            }
        });
        measure.stop();
        self.clean_accounts_stats
            .remove_dead_accounts_remove_us
            .fetch_add(measure.as_us(), Ordering::Relaxed);

        let mut measure = Measure::start("shrink");
        let mut shrink_candidate_slots = self.shrink_candidate_slots.lock().unwrap();
        for slot in new_shrink_candidates {
            shrink_candidate_slots.insert(slot);
        }
        drop(shrink_candidate_slots);
        measure.stop();
        self.clean_accounts_stats
            .remove_dead_accounts_shrink_us
            .fetch_add(measure.as_us(), Ordering::Relaxed);

        dead_slots
    }

    /// Stores accounts in the write cache and updates the index.
    /// This should only be used for accounts that are unrooted (unfrozen)
    pub(crate) fn store_accounts_unfrozen<'a>(
        &self,
        accounts: impl StorableAccounts<'a>,
        ancestors: &Ancestors,
    ) {
        // If all transactions in a batch are errored,
        // it's possible to get a store with no accounts.
        if accounts.is_empty() {
            return;
        }

        // Store the accounts in the write cache
        let write_accounts_time = Measure::start("write_accounts");
        let (store_account, write_stats) =
            self.write_accounts_to_cache(accounts.target_slot(), &accounts, ancestors);
        let write_accounts_us = write_accounts_time.end_as_us();

        // Update the secondary index
        if !self.account_indexes.is_empty() {
            let update_secondary_index_time = Measure::start("update_secondary_index");
            self.update_secondary_index_cached_accounts(&accounts, &store_account);
            let update_secondary_index_us = update_secondary_index_time.end_as_us();
            self.store_accounts_unfrozen_stats
                .update_secondary_index_us
                .fetch_add(update_secondary_index_us, Ordering::Relaxed);
        }

        let stats = &self.store_accounts_unfrozen_stats;
        stats
            .write_to_cache_us
            .fetch_add(write_accounts_us, Ordering::Relaxed);
        stats
            .num_initial_accounts_to_store
            .fetch_add(write_stats.num_initial_accounts_to_store, Ordering::Relaxed);
        stats
            .num_accounts_stored
            .fetch_add(write_stats.num_accounts_stored, Ordering::Relaxed);
        stats.num_duplicate_accounts_skipped.fetch_add(
            write_stats.num_duplicate_accounts_skipped,
            Ordering::Relaxed,
        );
        stats.num_ephemeral_accounts_skipped.fetch_add(
            write_stats.num_ephemeral_accounts_skipped,
            Ordering::Relaxed,
        );
        stats.num_ancestors_zero_lamport_skipped.fetch_add(
            write_stats.num_ancestors_zero_lamport_skipped,
            Ordering::Relaxed,
        );
        stats
            .account_data_bytes_stored
            .fetch_add(write_stats.account_data_bytes_stored, Ordering::Relaxed);
        stats.report();
        self.report_store_timings();
    }

    /// Store `accounts` into `storage`.
    ///
    /// This fn is to only be called by ancient squash.
    pub(crate) fn store_accounts_for_squash<'a>(
        &self,
        accounts: impl StorableAccounts<'a>,
        storage: &AccountStorageEntry,
    ) -> StoreAccountsForSquashStats {
        let slot = accounts.target_slot();

        // Flush the read cache if necessary
        let flush_read_cache_us = if self.read_only_accounts_cache.can_slot_be_in_cache(slot) {
            let flush_read_cache_time = Measure::start("flush_read_cache");
            (0..accounts.len()).for_each(|index| {
                // Based on the patterns of how a validator writes accounts, it is almost always
                // the case that there is no read only cache entry for this pubkey and slot.
                // So, we can give that hint to the `remove` for performance.
                self.read_only_accounts_cache
                    .remove_assume_not_present(accounts.pubkey(index));
            });
            flush_read_cache_time.end_as_us()
        } else {
            0
        };

        let store_accounts_for_shrink_stats = self.store_accounts_for_shrink(accounts, storage);
        StoreAccountsForSquashStats {
            store_accounts_for_shrink_stats,
            flush_read_cache_us,
        }
    }

    /// Stores accounts in the storage and updates the index.
    /// This function is intended for accounts that are being shrunk (moving from one store to another)
    /// - `UpsertReclaims` is set to `IgnoreReclaims`. If the slot in `accounts` differs from the new slot,
    ///   accounts may be removed from the account index. In such cases, the caller must ensure that alive
    ///   accounts are decremented for the older storage or that the old storage is removed entirely
    pub fn store_accounts_for_shrink<'a>(
        &self,
        accounts: impl StorableAccounts<'a>,
        storage: &AccountStorageEntry,
    ) -> StoreAccountsForShrinkStats {
        let slot = accounts.target_slot();
        let num_accounts_stored = accounts.len();

        // Write the accounts to storage
        let write_accounts_time = Measure::start("write_accounts");
        let infos = self.write_accounts_to_storage(slot, storage, &accounts);
        let write_accounts_us = write_accounts_time.end_as_us();

        let update_index_time = Measure::start("update_index");
        self.update_index_for_shrink(&infos, &accounts);
        let update_index_us = update_index_time.end_as_us();

        StoreAccountsForShrinkStats {
            write_accounts_us,
            update_index_us,
            num_accounts_stored: num_accounts_stored as u64,
        }
    }

    /// Write tombstones into new_storage and store the new offsets on its tombstone_offsets
    /// Note: They are not added to the index
    /// Returns the number of tombstones stored
    fn store_tombstones<'a>(
        &self,
        new_storage: &AccountStorageEntry,
        tombstones: impl StorableAccounts<'a>,
    ) -> usize {
        if tombstones.is_empty() {
            return 0;
        }
        let tombstone_infos =
            self.write_accounts_to_storage(tombstones.target_slot(), new_storage, &tombstones);
        new_storage.batch_insert_tombstone_offsets(tombstone_infos.iter().map(|info| info.offset()))
    }

    /// Stores accounts into a new storage and updates the index.
    /// This function is intended for accounts that are being flushed (moving from the cache to storage)
    /// - `UpsertReclaims` determines whether to reclaim old slots. If `ReclaimOldSlots` is used, all
    ///   old versions of the account are reclaimed. If `IgnoreReclaims` is used, old versions of the
    ///   account are not reclaimed and must be cleaned later.
    fn store_accounts_for_flush<'a>(
        &self,
        accounts: impl StorableAccounts<'a>,
        size_for_new_storage: u64,
        reclaim_handling: UpsertReclaim,
    ) -> StoreAccountsForFlushStats {
        let slot = accounts.target_slot();

        debug_assert!(self.accounts_cache.contains_unflushed_root(slot));

        let storage = self.create_store(slot, size_for_new_storage);

        // Write the accounts to storage
        let write_accounts_time = Measure::start("write_accounts");
        let infos = self.write_accounts_to_storage(slot, &storage, &accounts);
        let write_accounts_us = write_accounts_time.end_as_us();

        // This ensures that all updates are written to storage, before any
        // updates to the index happen, so anybody that sees a real entry in the index,
        // will be able to find the account in storage.
        self.storage.insert(Arc::new(storage));

        let update_index_time = Measure::start("update_index");
        let reclaims = self.update_index_for_flush(infos, &accounts, reclaim_handling);
        let update_index_us = update_index_time.end_as_us();

        // If there are any reclaims then they should be handled. Reclaims affect
        // all storages, and may result in the removal of dead storages.
        // since reclaims only contains non-empty SlotList<AccountInfo>, we
        // should skip handle_reclaims only when reclaims is empty. No need to
        // check the elements of reclaims are empty.
        let handle_reclaims_time = Measure::start("handle_reclaims");
        let mut num_reclaims = 0;
        let mut num_obsolete_slots_removed = 0;
        let mut num_obsolete_bytes_removed = 0;
        let mut is_slot_dead = false;
        if !reclaims.is_empty() {
            num_reclaims = reclaims.iter().map(|r| r.len() as u64).sum();
            let purge_stats = PurgeStats::default();
            let dead_slots = self.handle_reclaims(
                reclaims.iter().flatten(),
                &purge_stats,
                MarkAccountsObsolete::Yes(slot),
            );
            is_slot_dead = dead_slots.contains(&slot);
            num_obsolete_slots_removed =
                purge_stats.num_stored_slots_removed.load(Ordering::Relaxed) as u64;
            num_obsolete_bytes_removed = purge_stats
                .total_removed_stored_bytes
                .load(Ordering::Relaxed);
        }
        let handle_reclaims_us = handle_reclaims_time.end_as_us();

        // Handling reclaims purges the flushed storage when every flushed account became a
        // tombstone covered by the latest full snapshot. Otherwise the storage must still
        // exist, and just one storage is enough to hold all the data for the slot.
        if !is_slot_dead {
            assert!(self.storage.get_slot_storage_entry(slot).is_some());
            self.reopen_storage_as_readonly_shrinking_in_progress_ok(slot);
        }

        StoreAccountsForFlushStats {
            write_accounts_us,
            update_index_us,
            handle_reclaims_us,
            num_reclaims,
            num_obsolete_slots_removed,
            num_obsolete_bytes_removed,
        }
    }

    /// Returns whether the latest version of pubkey from ancestors is zero-lamport
    /// Returns `None` if the account doesn't exist
    fn is_ancestor_zero_lamport(&self, pubkey: &Pubkey, ancestors: &Ancestors) -> Option<bool> {
        if let Some((cached_account, _cache_slot)) =
            self.accounts_cache.load_latest(pubkey, ancestors)
        {
            // Check the write cache first; a hit is the freshest version visible on this fork,
            // so return it
            Some(cached_account.account.lamports() == 0)
        } else {
            self.accounts_index
                .get_with_and_then(pubkey, ancestors, true, |(_, account)| {
                    account.is_zero_lamport()
                })
        }
    }

    // Stores accounts in the write cache. If an account is zero-lamport and not present in the
    // cache or index, there is no need to store it in the write cache as it will not affect the
    // accounts hash. The function returns a BitVec indicating whether each account was stored in
    // the cache. Ordering of accounts is important as duplicate pubkeys are possible. The last
    // account in accounts_and_meta_to_store for each pubkey is stored in the write cache.
    fn write_accounts_to_cache<'a, 'b>(
        &self,
        slot: Slot,
        accounts_and_meta_to_store: &impl StorableAccounts<'b>,
        ancestors: &Ancestors,
    ) -> (BitVec, WriteAccountsToCacheStats) {
        let len = accounts_and_meta_to_store.len();
        let mut pubkey_set = ahash::HashSet::with_capacity(len);
        let mut stats = WriteAccountsToCacheStats {
            num_initial_accounts_to_store: len as u64,
            ..Default::default()
        };
        let mut store_account = BitVec::new_fill(false, len as u64);

        (0..len).rev().for_each(|index| {
            accounts_and_meta_to_store.account_default_if_zero_lamport(index, |account| {
                let pubkey = account.pubkey();
                let is_duplicate_account = !pubkey_set.insert(*pubkey);
                if is_duplicate_account {
                    // If the same account is written multiple times in the same batch,
                    // only store the latest version
                    stats.num_duplicate_accounts_skipped += 1;
                    return;
                }
                if account.is_zero_lamport() {
                    match self.is_ancestor_zero_lamport(pubkey, ancestors) {
                        None => {
                            stats.num_ephemeral_accounts_skipped += 1;
                            return;
                        }
                        Some(true) => {
                            stats.num_ancestors_zero_lamport_skipped += 1;
                            return;
                        }
                        Some(false) => {}
                    }
                }

                let account_shared_data = account.take_account();
                let account_data_len = account_shared_data.data().len();
                self.accounts_cache.store(slot, pubkey, account_shared_data);
                store_account.set(index as u64, true);
                stats.num_accounts_stored += 1;
                stats.account_data_bytes_stored += account_data_len as u64;
            })
        });

        (store_account, stats)
    }

    fn write_accounts_to_storage<'a>(
        &self,
        slot: Slot,
        storage: &AccountStorageEntry,
        accounts_and_meta_to_store: &impl StorableAccounts<'a>,
    ) -> Vec<AccountInfo> {
        let num_accounts = accounts_and_meta_to_store.len();
        let mut infos = Vec::with_capacity(num_accounts);
        if num_accounts == 0 {
            return infos;
        }

        let store_id = storage.id();
        let stored_accounts_info = storage
            .accounts
            .write_accounts(accounts_and_meta_to_store)
            .unwrap_or_else(|| {
                panic!(
                    "failed to write accounts to storage: slot! {slot}, id: {store_id}, len: {} \
                     bytes, num accounts: {num_accounts}",
                    storage.accounts.len(),
                )
            });

        assert_eq!(
            stored_accounts_info.offsets.len(),
            num_accounts,
            "failed to write all accounts to storage! {slot}, id: {store_id}, len: {} bytes, num \
             accounts written: {}, num accounts total: {num_accounts}",
            storage.accounts.len(),
            stored_accounts_info.offsets.len(),
        );

        for (i, offset) in stored_accounts_info.offsets.iter().enumerate() {
            infos.push(AccountInfo::new(
                StorageLocation::AccountsFile(store_id, *offset),
                accounts_and_meta_to_store.is_zero_lamport(i),
            ));
        }
        storage.add_accounts(
            stored_accounts_info.offsets.len(),
            stored_accounts_info.size,
        );

        infos
    }

    fn report_store_timings(&self) {
        if self.stats.last_store_report.should_update(1000) {
            let read_cache_stats = self.read_only_accounts_cache.get_and_reset_stats();
            datapoint_info!(
                "accounts_db_store_timings",
                (
                    "stakes_cache_check_and_store_us",
                    self.stats
                        .stakes_cache_check_and_store_us
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "read_only_accounts_cache_entries",
                    self.read_only_accounts_cache.cache_len(),
                    i64
                ),
                (
                    "read_only_accounts_cache_data_size",
                    self.read_only_accounts_cache.data_size(),
                    i64
                ),
                ("read_only_accounts_cache_hits", read_cache_stats.hits, i64),
                (
                    "read_only_accounts_cache_misses",
                    read_cache_stats.misses,
                    i64
                ),
                (
                    "read_only_accounts_cache_evicts",
                    read_cache_stats.evicts,
                    i64
                ),
                (
                    "read_only_accounts_cache_load_us",
                    read_cache_stats.load_us,
                    i64
                ),
                (
                    "read_only_accounts_cache_store_us",
                    read_cache_stats.store_us,
                    i64
                ),
                (
                    "read_only_accounts_cache_evict_us",
                    read_cache_stats.evict_us,
                    i64
                ),
                (
                    "read_only_accounts_cache_evict_run_count",
                    read_cache_stats.evict_run_count,
                    i64
                ),
                (
                    "handle_dead_keys_us",
                    self.stats.handle_dead_keys_us.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "purge_exact_us",
                    self.stats.purge_exact_us.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "purge_exact_count",
                    self.stats.purge_exact_count.swap(0, Ordering::Relaxed),
                    i64
                ),
            );

            datapoint_info!(
                "accounts_db_store_timings2",
                (
                    "create_store_count",
                    self.stats.create_store_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "dropped_stores",
                    self.stats.dropped_stores.swap(0, Ordering::Relaxed),
                    i64
                ),
            );

            self.load_account_stats.report();
        }
    }

    pub fn add_root(&self, slot: Slot) -> AccountsAddRootTiming {
        let mut cache_time = Measure::start("cache_add_root");
        self.accounts_cache.add_root(slot);
        cache_time.stop();

        self.max_root.fetch_max(slot, Ordering::Relaxed);

        AccountsAddRootTiming {
            cache_us: cache_time.as_us(),
        }
    }

    /// Returns the largest slot that has been added as a root via `add_root`.
    pub fn max_root(&self) -> Slot {
        self.max_root.load(Ordering::Relaxed)
    }

    /// Returns storages for `requested_slots`
    pub fn get_storages(
        &self,
        requested_slots: impl RangeBounds<Slot> + Sync,
    ) -> (Vec<Arc<AccountStorageEntry>>, Vec<Slot>) {
        let start = Instant::now();
        let (slots, storages) = self
            .storage
            .get_if(|slot, storage| requested_slots.contains(slot) && storage.has_accounts())
            .into_vec()
            .into_iter()
            .unzip();
        let duration = start.elapsed();
        debug!("get_snapshot_storages: {duration:?}");
        (storages, slots)
    }

    /// Returns the latest full snapshot slot
    pub fn latest_full_snapshot_slot(&self) -> Option<Slot> {
        self.latest_full_snapshot_slot.read()
    }

    /// Sets the latest full snapshot slot to `slot`
    pub fn set_latest_full_snapshot_slot(&self, slot: Slot) {
        *self.latest_full_snapshot_slot.lock_write() = Some(slot);
        self.latest_full_snapshot_slot_advanced_since_clean
            .store(true, Ordering::Release);
    }

    /// Marks slots <= slot as already swept for zero-lamport-single-ref shrink eligibility
    pub fn set_last_swept_full_snapshot_slot(&self, slot: Slot) {
        // Prior to setting this, the latest full snapshot slot must be set, and
        // last_swept_full_snapshot_slot value must be less than or equal to it.
        assert!(
            self.latest_full_snapshot_slot()
                .is_some_and(|snapshot_slot| slot <= snapshot_slot),
            "last swept full snapshot slot {slot} cannot be greater than latest full snapshot \
             slot {:?}",
            self.latest_full_snapshot_slot()
        );
        self.last_swept_full_snapshot_slot
            .store(slot, Ordering::Relaxed);
    }

    fn generate_index_for_slot<'a>(
        &self,
        reader: &mut impl RequiredLenBufFileRead<'a>,
        accum: &mut IndexGenerationAccumulator,
        storage_index: usize,
        storage: &'a AccountStorageEntry,
    ) {
        let slot = storage.slot();
        let store_id = storage.id();

        let mut capitalization = 0_u64;
        let mut accounts_data_len = 0;
        let mut stored_size_alive = 0;
        let mut all_accounts_are_zero_lamports = true;
        accum.slot_arena.ensure_empty();
        let keyed_account_infos = &mut accum.slot_arena.keyed_account_infos;
        let mut zero_lamport_pubkeys = Vec::new();
        // Batches this thread's account lt-hashes across all its storages; merged
        // into other accumulators in `accumulate`.
        let lt_hash_acc = &mut accum.lt_hash_acc;

        let geyser_notifier = self
            .accounts_update_notifier
            .as_ref()
            .filter(|notifier| notifier.snapshot_notifications_enabled());

        // If geyser notifications at startup from snapshot are enabled, we need to pass in a
        // write version for each account notification.  This value does not need to be
        // globally unique, as geyser plugins also receive the slot number.  We only need to
        // ensure that more recent accounts have a higher write version than older accounts.
        // Even more relaxed, we really only need to have different write versions if there are
        // multiple versions of the same account in a single storage, which is not allowed.
        //
        // Since we scan the storage from oldest to newest, we can simply increment a local
        // counter per account and use that for the write version.
        let mut write_version_for_geyser = 0;
        let num_obsolete_accounts_skipped = storage
            .scan_accounts(reader, |offset, account| {
                let data_len = account.data.len();
                stored_size_alive += storage.accounts.calculate_stored_size(data_len);
                let is_account_zero_lamport = account.is_zero_lamport();
                if !is_account_zero_lamport {
                    accounts_data_len += data_len as u64;
                    all_accounts_are_zero_lamports = false;
                } else {
                    // Collect zero-lamport pubkeys so they can be added to `uncleaned_pubkeys`
                    // after the scan, for clean to examine and remove.
                    zero_lamport_pubkeys.push(*account.pubkey);
                }
                keyed_account_infos.push((
                    *account.pubkey,
                    AccountInfo::new(
                        StorageLocation::AccountsFile(store_id, offset), // will never be cached
                        is_account_zero_lamport,
                    ),
                ));

                if !self.account_indexes.is_empty() {
                    self.accounts_index.update_secondary_indexes(
                        account.pubkey,
                        &account,
                        &self.account_indexes,
                    );
                }

                if !is_account_zero_lamport {
                    Self::add_account_to_lt_hash(lt_hash_acc, &account, account.pubkey);
                }

                // SAFETY: The bank capitalization field is a u64, so the lamport sum of
                // all accounts modified in a single slot must fit into a u64.
                capitalization = capitalization
                    .checked_add(account.lamports())
                    .expect("capitalization cannot overflow");

                if let Some(geyser_notifier) = geyser_notifier {
                    debug_assert!(geyser_notifier.snapshot_notifications_enabled());
                    let account_for_geyser = AccountForGeyser {
                        pubkey: account.pubkey(),
                        lamports: account.lamports(),
                        owner: account.owner(),
                        executable: account.executable(),
                        rent_epoch: account.rent_epoch(),
                        data: account.data(),
                    };
                    geyser_notifier.notify_account_restore_from_snapshot(
                        slot,
                        write_version_for_geyser,
                        &account_for_geyser,
                    );
                    write_version_for_geyser += 1;
                }
            })
            .expect("must scan accounts storage");

        accum.capitalization = accum
            .capitalization
            .checked_add(u128::from(capitalization))
            .expect("capitalization cannot overflow");

        let (insert_info, insert_time_us) = measure_us!(
            self.accounts_index
                .insert_new_if_missing_into_primary_index(slot, keyed_account_infos)
        );

        if insert_info.count > 0 {
            // push summary info for store_id into thread state (all threads build a piece of full list)
            let info = StorageSizeAndCount {
                stored_size: stored_size_alive,
                count: insert_info.count,
            };

            // sanity check that stored_size is not larger than the u64 aligned size of the accounts files.
            // Note that the stored_size is aligned, so it can be larger than the size of the accounts file.
            assert!(
                info.stored_size <= u64_align!(storage.accounts.len()),
                "Stored size ({}) is larger than the size of the accounts file ({}) for store_id: \
                 {}",
                info.stored_size,
                storage.accounts.len(),
                store_id
            );
            accum.storage_info.push((store_id, info));
        }

        // Zero-lamport accounts stay alive in the index until clean removes them. Their storages
        // are not otherwise dirty, so add the pubkeys into `uncleaned_pubkeys` for the first
        // clean to examine them.
        if !zero_lamport_pubkeys.is_empty() {
            accum.num_zero_lamport_pubkeys += zero_lamport_pubkeys.len() as u64;
            // Each slot has exactly one storage, so this is the only insert for `slot`.
            self.uncleaned_pubkeys.insert(slot, zero_lamport_pubkeys);
        }

        accum.num_accounts += insert_info.count as u64;
        accum.insert_time_us += insert_time_us;
        accum.accounts_data_len += accounts_data_len;
        accum.num_did_not_exist += insert_info.num_did_not_exist;
        accum.num_existed_in_mem += insert_info.num_existed_in_mem;
        accum.num_existed_on_disk += insert_info.num_existed_on_disk;
        accum.num_obsolete_accounts_skipped += num_obsolete_accounts_skipped;
        if all_accounts_are_zero_lamports {
            accum.all_accounts_are_zero_lamports_slots += 1;
            accum
                .slots_with_only_zero_lamport_accounts
                .push((slot, storage_index));
        }
    }

    pub fn generate_index(
        &self,
        limit_load_slot_count_from_snapshot: Option<usize>,
        verify: bool,
    ) -> IndexGenerationInfo {
        let mut total_time = Measure::start("generate_index");

        let mut storages = self.storage.all_storages();
        storages.sort_unstable_by_key(|storage| storage.slot());
        if let Some(limit) = limit_load_slot_count_from_snapshot {
            storages.truncate(limit); // get rid of the newer slots and keep just the older
        }
        let num_storages = storages.len();

        // `storages` is sorted by slot, so the last one is the highest root.
        if let Some(storage) = storages.last() {
            self.max_root.fetch_max(storage.slot(), Ordering::Relaxed);
        }

        self.accounts_index.set_startup(Startup::Startup);

        let mut total_accum = IndexGenerationAccumulator::with_slots_capacity(num_storages);
        let storages_orderer =
            AccountStoragesOrderer::with_random_order(&storages).into_concurrent_consumer();
        let exit_logger = AtomicBool::new(false);
        let num_processed = AtomicU64::new(0);
        let num_threads = num_cpus::get();
        let mut index_time = Measure::start("index");
        thread::scope(|s| {
            let thread_handles = (0..num_threads)
                .map(|i| {
                    thread::Builder::new()
                        .name(format!("solGenIndex{i:02}"))
                        .spawn_scoped(s, || {
                            let mut thread_accum = IndexGenerationAccumulator::with_slots_capacity(
                                num_storages.div_ceil(num_threads),
                            );
                            let mut reader = append_vec::new_scan_accounts_reader();
                            for next_item in storages_orderer.iter() {
                                let storage = next_item.storage;
                                self.generate_index_for_slot(
                                    &mut reader,
                                    &mut thread_accum,
                                    next_item.original_index,
                                    storage,
                                );
                                num_processed.fetch_add(1, Ordering::Relaxed);
                            }
                            thread_accum
                        })
                })
                .collect::<Result<Vec<_>, _>>()
                .expect("spawn threads");
            let logger_thread_handle = thread::Builder::new()
                .name("solGenIndexLog".to_string())
                .spawn_scoped(s, || {
                    let mut last_update = Instant::now();
                    loop {
                        if exit_logger.load(Ordering::Relaxed) {
                            break;
                        }
                        let num_processed = num_processed.load(Ordering::Relaxed);
                        if num_processed == num_storages as u64 {
                            info!("generating index: processed all slots");
                            break;
                        }
                        let now = Instant::now();
                        if now - last_update > Duration::from_secs(2) {
                            info!(
                                "generating index: processed {num_processed}/{num_storages} \
                                 slots..."
                            );
                            last_update = now;
                        }
                        thread::sleep(Duration::from_millis(500))
                    }
                })
                .expect("spawn thread");
            for thread_handle in thread_handles {
                let Ok(thread_accum) = thread_handle.join() else {
                    exit_logger.store(true, Ordering::Relaxed);
                    panic!("index generation failed");
                };
                total_accum.accumulate(thread_accum);
            }
            // Make sure to join the logger thread *after* the main threads.
            // This way, if a main thread errors, we won't spin indefinitely
            // waiting for the logger thread to finish (it never will).
            logger_thread_handle.join().expect("join thread");
        });
        index_time.stop();

        {
            // Update the index stats now.
            let index_stats = self.accounts_index.stats();

            // stats for inserted entries that previously did *not* exist
            index_stats.inc_insert_count(total_accum.num_did_not_exist);
            index_stats.add_mem_count(total_accum.num_did_not_exist as usize);

            // stats for inserted entries that previous did exist *in-mem*
            index_stats
                .entries_from_mem
                .fetch_add(total_accum.num_existed_in_mem, Ordering::Relaxed);
            index_stats
                .updates_in_mem
                .fetch_add(total_accum.num_existed_in_mem, Ordering::Relaxed);

            // stats for inserted entries that previously did exist *on-disk*
            index_stats.add_mem_count(total_accum.num_existed_on_disk as usize);
            index_stats
                .entries_missing
                .fetch_add(total_accum.num_existed_on_disk, Ordering::Relaxed);
            index_stats
                .updates_in_mem
                .fetch_add(total_accum.num_existed_on_disk, Ordering::Relaxed);
        }

        if let Some(geyser_notifier) = &self.accounts_update_notifier {
            // We've finished scanning all the storages, and have thus sent all the
            // account notifications.  Now, let the geyser plugins know we're done.
            geyser_notifier.notify_end_of_restore_from_snapshot();
        }

        if verify {
            info!("Verifying index...");
            let start = Instant::now();
            storages.par_iter().for_each(|storage| {
                let store_id = storage.id();
                let slot = storage.slot();
                storage
                    .scan_accounts_without_data(|offset, account| {
                        let key = account.pubkey();
                        self.accounts_index.get_and_then(key, |entry| {
                            let index_entry = entry.unwrap();
                            let slot_list = index_entry.slot_list_read_lock();
                            let mut count = 0;
                            for (slot2, account_info2) in slot_list.iter() {
                                if *slot2 == slot {
                                    count += 1;
                                    let ai = AccountInfo::new(
                                        StorageLocation::AccountsFile(store_id, offset), // will never be cached
                                        account.is_zero_lamport(),
                                    );
                                    assert_eq!(&ai, account_info2);
                                }
                            }
                            assert_eq!(1, count);
                            (false, ())
                        });
                    })
                    .expect("must scan accounts storage");
            });
            info!("Verifying index... Done in {:?}", start.elapsed());
        }

        let total_duplicate_slot_keys = AtomicU64::default();
        let total_num_unique_duplicate_keys = AtomicU64::default();

        // outer vec is accounts index bin (determined by pubkey value)
        // inner vec is the pubkeys within that bin that are present in > 1 slot
        let unique_pubkeys_by_bin = Mutex::new(Vec::<Vec<Pubkey>>::default());
        // tell accounts index we are done adding the initial accounts at startup
        let mut m = Measure::start("accounts_index_idle_us");
        self.accounts_index.set_startup(Startup::Normal);
        m.stop();
        let index_flush_us = m.as_us();

        let populate_duplicate_keys_us = measure_us!({
            // this has to happen before visit_duplicate_pubkeys_during_startup below
            // get duplicate keys from acct idx. We have to wait until we've finished flushing.
            self.accounts_index
                .populate_and_retrieve_duplicate_keys_from_startup(|slot_keys| {
                    total_duplicate_slot_keys.fetch_add(slot_keys.len() as u64, Ordering::Relaxed);
                    let unique_keys =
                        ahash::HashSet::<Pubkey>::from_iter(slot_keys.iter().map(|(_, key)| *key));
                    let unique_pubkeys_by_bin_inner = unique_keys.into_iter().collect::<Vec<_>>();
                    total_num_unique_duplicate_keys
                        .fetch_add(unique_pubkeys_by_bin_inner.len() as u64, Ordering::Relaxed);
                    // does not matter that this is not ordered by slot
                    unique_pubkeys_by_bin
                        .lock()
                        .unwrap()
                        .push(unique_pubkeys_by_bin_inner);
                });
        })
        .1;
        let unique_pubkeys_by_bin = unique_pubkeys_by_bin.into_inner().unwrap();

        let mut timings = GenerateIndexTimings {
            index_flush_us,
            index_time: index_time.as_us(),
            insertion_time_us: total_accum.insert_time_us,
            total_duplicate_slot_keys: total_duplicate_slot_keys.load(Ordering::Relaxed),
            total_num_unique_duplicate_keys: total_num_unique_duplicate_keys
                .load(Ordering::Relaxed),
            populate_duplicate_keys_us,
            total_including_duplicates: total_accum.num_accounts,
            total_slots: num_storages as u64,
            all_accounts_are_zero_lamports_slots: total_accum.all_accounts_are_zero_lamports_slots,
            num_obsolete_accounts_skipped: total_accum.num_obsolete_accounts_skipped,
            ..GenerateIndexTimings::default()
        };

        #[derive(Debug, Default)]
        struct DuplicatePubkeysVisitedInfo {
            accounts_data_len_from_duplicates: u64,
            num_duplicate_accounts: u64,
            duplicates_lt_hash: Box<DuplicatesLtHash>,
            capitalization_from_duplicates: u128,
        }
        impl DuplicatePubkeysVisitedInfo {
            fn reduce(mut self, other: Self) -> Self {
                self.accounts_data_len_from_duplicates += other.accounts_data_len_from_duplicates;
                self.num_duplicate_accounts += other.num_duplicate_accounts;
                self.duplicates_lt_hash
                    .0
                    .mix_in(&other.duplicates_lt_hash.0);
                self.capitalization_from_duplicates = self
                    .capitalization_from_duplicates
                    .checked_add(other.capitalization_from_duplicates)
                    .expect("capitalization cannot overflow");
                self
            }
        }

        let mut visit_duplicate_accounts_timer = Measure::start("visit duplicate accounts");
        let DuplicatePubkeysVisitedInfo {
            accounts_data_len_from_duplicates,
            num_duplicate_accounts,
            duplicates_lt_hash,
            capitalization_from_duplicates,
        } = unique_pubkeys_by_bin
            .par_iter()
            .fold(
                DuplicatePubkeysVisitedInfo::default,
                |accum, pubkeys_by_bin| {
                    let intermediate = pubkeys_by_bin
                        .par_chunks(4096)
                        .fold(DuplicatePubkeysVisitedInfo::default, |accum, pubkeys| {
                            let (
                                accounts_data_len_from_duplicates,
                                accounts_duplicates_num,
                                duplicates_lt_hash,
                                capitalization_from_duplicates,
                            ) = self.visit_duplicate_pubkeys_during_startup(pubkeys);
                            let intermediate = DuplicatePubkeysVisitedInfo {
                                accounts_data_len_from_duplicates,
                                num_duplicate_accounts: accounts_duplicates_num,
                                duplicates_lt_hash,
                                capitalization_from_duplicates,
                            };
                            DuplicatePubkeysVisitedInfo::reduce(accum, intermediate)
                        })
                        .reduce(
                            DuplicatePubkeysVisitedInfo::default,
                            DuplicatePubkeysVisitedInfo::reduce,
                        );
                    DuplicatePubkeysVisitedInfo::reduce(accum, intermediate)
                },
            )
            .reduce(
                DuplicatePubkeysVisitedInfo::default,
                DuplicatePubkeysVisitedInfo::reduce,
            );
        visit_duplicate_accounts_timer.stop();
        timings.visit_duplicate_accounts_time_us = visit_duplicate_accounts_timer.as_us();
        timings.num_duplicate_accounts = num_duplicate_accounts;

        // Finalize the batched account hashes, then remove the duplicates' hashes.
        let mut accounts_lt_hash = total_accum.lt_hash_acc.into_lt_hash();
        accounts_lt_hash.mix_out(&duplicates_lt_hash.0);
        total_accum.capitalization = total_accum
            .capitalization
            .checked_sub(capitalization_from_duplicates)
            .expect("capitalization cannot underflow");
        total_accum.accounts_data_len -= accounts_data_len_from_duplicates;
        info!("accounts data len: {}", total_accum.accounts_data_len);

        // insert all zero lamport account storage into the dirty stores and add them into the uncleaned roots for clean to pick up
        info!(
            "insert all zero slots to clean at startup {}",
            total_accum.slots_with_only_zero_lamport_accounts.len()
        );

        self.set_storage_count_and_alive_bytes(total_accum.storage_info, &mut timings);

        let mut mark_obsolete_accounts_time = Measure::start("mark_obsolete_accounts_time");
        // Mark all reclaims at max_slot. This is safe because only the snapshot paths care about
        // this information. Since this account was just restored from the previous snapshot and
        // it is known that it was already obsolete at that time, it must hold true that it will
        // still be obsolete if a newer snapshot is created, since a newer snapshot will always
        // be performed on a slot greater than the current slot
        let slot_marked_obsolete = storages.last().unwrap().slot();
        let obsolete_account_stats =
            self.mark_obsolete_accounts_at_startup(slot_marked_obsolete, unique_pubkeys_by_bin);

        mark_obsolete_accounts_time.stop();
        timings.mark_obsolete_accounts_us = mark_obsolete_accounts_time.as_us();
        timings.num_obsolete_accounts_marked = obsolete_account_stats.accounts_marked_obsolete;
        timings.num_slots_removed_as_obsolete = obsolete_account_stats.slots_removed;
        timings.num_zero_lamport_pubkeys = total_accum.num_zero_lamport_pubkeys;
        total_time.stop();
        timings.total_time_us = total_time.as_us();
        timings.report(self.accounts_index.get_startup_stats());

        self.accounts_index.log_secondary_indexes();

        // Now that the index is generated, get the total length and capacity of the in-mem maps
        // across all the bins and set the initial value for the stat.
        // We do this all at once, at the end, since getting the capacity requires iterating all
        // the bins and grabbing a read lock, which we try to avoid whenever possible.
        let (index_len, index_capacity) = self
            .accounts_index
            .account_maps
            .iter()
            .map(|bin| bin.len_and_cap_for_startup())
            .fold((0, 0), |mut accum, (len, cap)| {
                accum.0 += len;
                accum.1 += cap;
                accum
            });
        self.accounts_index
            .stats()
            .count_in_mem
            .store(index_len, Ordering::Relaxed);
        self.accounts_index
            .stats()
            .capacity_in_mem
            .store(index_capacity, Ordering::Relaxed);

        // The bank capitalization field is a u64, so a valid capitalization must fit into a u64.
        // The lamports from duplicate accounts have now been removed, so try casting.
        let Ok(calculated_capitalization) = u64::try_from(total_accum.capitalization) else {
            panic!(
                "calculated capitalization overflowed a u64, which is invalid! calculated \
                 capitalization: {}",
                total_accum.capitalization,
            );
        };
        IndexGenerationInfo {
            accounts_data_len: total_accum.accounts_data_len,
            calculated_accounts_lt_hash: AccountsLtHash(accounts_lt_hash),
            calculated_capitalization,
        }
    }

    /// Use the duplicated pubkeys to mark all older version of the pubkeys as obsolete
    /// This will remove the older entries from the slot lists and then reclaim the accounts
    fn mark_obsolete_accounts_at_startup(
        &self,
        slot_marked_obsolete: Slot,
        pubkeys_with_duplicates_by_bin: Vec<Vec<Pubkey>>,
    ) -> ObsoleteAccountsStats {
        let stats: ObsoleteAccountsStats = pubkeys_with_duplicates_by_bin
            .par_iter()
            .map(|pubkeys_by_bin| {
                let reclaims = self
                    .accounts_index
                    .clean_rooted_entries_by_bin(pubkeys_by_bin);
                let stats = PurgeStats::default();

                // Mark all the entries as obsolete, and remove any empty storages
                if !reclaims.is_empty() {
                    self.handle_reclaims(
                        reclaims.iter(),
                        &stats,
                        MarkAccountsObsolete::Yes(slot_marked_obsolete),
                    );
                }
                ObsoleteAccountsStats {
                    accounts_marked_obsolete: reclaims.len() as u64,
                    slots_removed: stats.num_stored_slots_removed.load(Ordering::Relaxed) as u64,
                }
            })
            .sum();
        stats
    }

    /// Used during generate_index() to:
    /// 1. get the _duplicate_ accounts from the given pubkeys
    /// 2. get the slots that contained duplicate pubkeys
    /// 3. build up the duplicates lt hash
    ///
    /// Note this should only be used when ALL entries in the accounts index are roots.
    ///
    /// returns tuple of:
    /// - data len sum of all older duplicates
    /// - number of duplicate accounts
    /// - lt hash of duplicates
    /// - capitalization of duplicates
    fn visit_duplicate_pubkeys_during_startup(
        &self,
        pubkeys: &[Pubkey],
    ) -> (u64, u64, Box<DuplicatesLtHash>, u128) {
        let mut accounts_data_len_from_duplicates = 0;
        let mut num_duplicate_accounts = 0_u64;
        let mut duplicates_lt_hash = Box::new(DuplicatesLtHash::default());
        let mut capitalization_from_duplicates = 0_u128;
        self.accounts_index.scan(
            pubkeys.iter(),
            |pubkey, slot_list| {
                if let Some(slot_list) = slot_list
                    && slot_list.len() > 1
                {
                    // Only the account data len in the highest slot should be used, and the rest are
                    // duplicates.  So find the max slot to keep.
                    // Then sum up the remaining data len, which are the duplicates.
                    // All of the slots need to go in the 'uncleaned_slots' list. For clean to work properly,
                    // the slot where duplicate accounts are found in the index need to be in 'uncleaned_slots' list, too.
                    let max = slot_list.iter().map(|(slot, _)| slot).max().unwrap();
                    slot_list.iter().for_each(|(slot, account_info)| {
                        if slot == max {
                            // the info in 'max' is the most recent, current info for this pubkey
                            return;
                        }
                        let maybe_storage_entry = self
                            .storage
                            .get_account_storage_entry(*slot, account_info.store_id());
                        let mut accessor = LoadedAccountAccessor::Stored(
                            maybe_storage_entry.map(|entry| (entry, account_info.offset())),
                        );
                        accessor.check_and_get_loaded_account(|loaded_account| {
                            let data_len = loaded_account.data_len();
                            let lamports = loaded_account.lamports();
                            if lamports > 0 {
                                accounts_data_len_from_duplicates += data_len;
                            }
                            num_duplicate_accounts += 1;
                            let account_lt_hash = Self::lt_hash_account(&loaded_account, pubkey);
                            duplicates_lt_hash.0.mix_in(&account_lt_hash.0);
                            capitalization_from_duplicates = capitalization_from_duplicates
                                .checked_add(u128::from(lamports))
                                .expect("capitalization cannot overflow");
                        });
                    });
                }
            },
            ScanFilter::All,
        );
        (
            accounts_data_len_from_duplicates as u64,
            num_duplicate_accounts,
            duplicates_lt_hash,
            capitalization_from_duplicates,
        )
    }

    fn set_storage_count_and_alive_bytes(
        &self,
        stored_sizes_and_counts: StorageSizeAndCountList,
        timings: &mut GenerateIndexTimings,
    ) {
        // store count and size for each storage
        let mut storage_size_storages_time = Measure::start("storage_size_storages");
        let stored_sizes_and_counts: IntMap<_, _> = stored_sizes_and_counts.into_iter().collect();
        for (_slot, store) in self.storage.iter() {
            let id = store.id();
            // Should be default at this point
            assert_eq!(store.alive_bytes(), 0);
            if let Some(entry) = stored_sizes_and_counts.get(&id) {
                trace!(
                    "id: {} setting count: {} cur: {}",
                    id,
                    entry.count,
                    store.count(),
                );
                {
                    let prev_count = store
                        .num_alive_accounts
                        .swap(entry.count, Ordering::Release);
                    assert_eq!(prev_count, 0);
                }
                store
                    .num_alive_bytes
                    .store(entry.stored_size, Ordering::Release);
            } else {
                trace!("id: {id} clearing count");
                store.num_alive_accounts.store(0, Ordering::Release);
            }
        }
        storage_size_storages_time.stop();
        timings.storage_size_storages_us = storage_size_storages_time.as_us();
    }

    pub fn print_accounts_stats(&self, label: &str) {
        self.print_index();
        self.print_count_and_status(label);
    }

    fn print_index(&self) {
        self.accounts_index.account_maps.iter().for_each(|map| {
            for pubkey in map.keys() {
                self.accounts_index.get_and_then(&pubkey, |account_entry| {
                    if let Some(account_entry) = account_entry {
                        let list_r = account_entry.slot_list_read_lock();
                        info!(" key: {pubkey} slots: {list_r:?}");
                    }
                    let add_to_in_mem_cache = false;
                    (add_to_in_mem_cache, ())
                });
            }
        });
    }

    pub fn print_count_and_status(&self, label: &str) {
        let mut slots: Vec<_> = self.storage.all_slots();
        #[allow(clippy::stable_sort_primitive)]
        slots.sort();
        info!("{}: count_and status for {} slots:", label, slots.len());
        for slot in &slots {
            let entry = self.storage.get_slot_storage_entry(*slot).unwrap();
            info!(
                "  slot: {} id: {} count: {} len: {}",
                slot,
                entry.id(),
                entry.count(),
                entry.accounts.len(),
            );
        }
    }
}

/// Whether a rooted-cache flush should clean (dedup across roots, keeping only the newest
/// version of each account and reclaiming older ones) or write every account untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FlushShouldClean {
    /// Write every account in every flushed root. Older versions must be preserved (e.g. for
    /// snapshotting).
    No,
    /// Clean roots at or below `max_clean_root`; roots above it still flush all their accounts
    /// (`None` means clean every flushed root).
    Yes { max_clean_root: Option<Slot> },
}

/// Which of a slot's cached accounts to write to storage when flushing it.
#[derive(Debug, PartialEq, Eq)]
enum PubkeysToStore {
    /// Store every account in the slot, reclaiming nothing. Used for roots above
    /// `max_clean_root` and when not cleaning, since an in-flight scan may still need
    /// those versions.
    All,
    /// Store only these pubkeys (the newest version of each, per `select_pubkeys_to_store`),
    /// purging the rest from the index and reclaiming older versions.
    Only(ahash::HashSet<Pubkey>),
}

/// Specify whether obsolete accounts should be marked or not during reclaims
/// They should only be marked if they are also getting removed from the index
///
/// When an account is marked obsolete at the slot it is present in (Eg. if the account is present
/// in slot 10 and marked obsolete at slot 10), it means the account was deleted rather than
/// overwritten to a newer copy. These are marked as tombstones rather than obsolete.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum MarkAccountsObsolete {
    Yes(Slot),
    // only constructed by dev-context-only-utils callers
    #[allow(dead_code)]
    No,
}

// These functions/fields are only usable from a dev context (i.e. tests and benches)
#[cfg(feature = "dev-context-only-utils")]
impl AccountStorageEntry {
    fn accounts_count(&self) -> usize {
        let mut count = 0;
        self.accounts
            .scan_pubkeys(|_| {
                count += 1;
            })
            .expect("must scan accounts storage");
        count
    }
}

// These functions/fields are only usable from a dev context (i.e. tests and benches)
#[cfg(feature = "dev-context-only-utils")]
impl AccountsDb {
    pub fn default_for_tests() -> Self {
        Self::new_for_tests_with_config(Vec::new(), ACCOUNTS_DB_CONFIG_FOR_TESTING)
    }

    pub fn new_for_tests_with_config(
        paths: Vec<PathBuf>,
        accounts_db_config: AccountsDbConfig,
    ) -> Self {
        Self::new_with_config(paths, accounts_db_config, None, Arc::default())
    }

    /// Return the number of slots marked with uncleaned pubkeys.
    /// This is useful for testing clean algorithms.
    pub fn get_len_of_slots_with_uncleaned_pubkeys(&self) -> usize {
        self.uncleaned_pubkeys.len()
    }

    /// Call clean_accounts() with the common parameters that tests/benches use.
    pub fn clean_accounts_for_tests(&self) {
        self.clean_accounts(None, false)
    }

    pub fn flush_accounts_cache_slot_for_tests(&self, slot: Slot) {
        assert!(self.accounts_cache.contains_unflushed_root(slot));
        self.flush_slot_cache(slot, &PubkeysToStore::All);
    }

    /// useful to adapt tests written prior to introduction of the write cache
    /// to use the write cache
    pub fn add_root_and_flush_write_cache(&self, slot: Slot) {
        self.add_root(slot);
        self.flush_root_write_cache(slot);
    }

    /// note this returns Some for accounts with zero lamports
    /// Note that this is non-deterministic if clean is running asynchronously.
    /// If a zero lamport account exists in the index, then Some is returned.
    /// Once it is cleaned from the index, None is returned.
    fn do_load_for_tests(
        &self,
        ancestors: &Ancestors,
        pubkey: &Pubkey,
    ) -> Option<(AccountSharedData, Slot)> {
        self.do_load(
            ancestors,
            pubkey,
            LoadHint::Unspecified,
            PopulateReadCache::True,
            None::<fn(_, &_, _) -> _>,
        )
    }

    pub fn assert_load_account(&self, slot: Slot, pubkey: Pubkey, expected_lamports: u64) {
        let ancestors = Ancestors::from(vec![slot]);
        let (account, slot) = self.do_load_for_tests(&ancestors, &pubkey).unwrap();
        assert_eq!((account.lamports(), slot), (expected_lamports, slot));
    }

    pub fn assert_not_load_account(&self, slot: Slot, pubkey: Pubkey) {
        let ancestors = Ancestors::from(vec![slot]);
        let load = self.do_load_for_tests(&ancestors, &pubkey);
        assert!(load.is_none(), "{load:?}");
    }

    /// Is `pubkey` in the db?
    #[cfg(feature = "dev-context-only-utils")]
    pub fn contains(&self, pubkey: &Pubkey) -> bool {
        self.accounts_cache.contains_pubkey(pubkey) || self.accounts_index.contains(pubkey)
    }

    pub fn check_accounts(&self, pubkeys: &[Pubkey], slot: Slot, num: usize, count: usize) {
        let ancestors = Ancestors::from(vec![slot]);
        for _ in 0..num {
            let idx = rng().random_range(0..num);
            let account = self.do_load_for_tests(&ancestors, &pubkeys[idx]);
            let account1 = Some((
                AccountSharedData::new(
                    (idx + count) as u64,
                    0,
                    AccountSharedData::default().owner(),
                ),
                slot,
            ));
            assert_eq!(account, account1);
        }
    }

    // Store accounts for tests. For zero-lamport accounts, first store a single-lamport
    // placeholder, then store the actual account. This is to ensure that an index entry is created
    // for zero-lamport accounts.
    pub fn store_for_tests<'a>(&self, accounts: impl StorableAccounts<'a>) {
        let slot = accounts.target_slot();
        let ancestors = Ancestors::from(vec![slot]);

        let placeholder = AccountSharedData::new(1, 0, &Pubkey::default());

        // Build a list of zero-lamport accounts not present in the index
        let mut pre_populate_zero_lamport = Vec::new();
        for i in 0..accounts.len() {
            if accounts.is_zero_lamport(i) {
                let key = *accounts.pubkey(i);
                if self
                    .accounts_index
                    .get_with_and_then(&key, &ancestors, true, |(_, info)| info.is_zero_lamport())
                    .is_none_or(|is_zero| is_zero)
                {
                    pre_populate_zero_lamport.push((key, placeholder.clone()));
                }
            }
        }

        // Pre-populate new zero-lamport accounts with single-lamport placeholders.
        self.store_accounts_unfrozen((slot, pre_populate_zero_lamport.as_slice()), &ancestors);

        // Then store the actual accounts provided by the caller.
        self.store_accounts_unfrozen(accounts, &ancestors);
    }

    #[allow(clippy::needless_range_loop)]
    pub fn modify_accounts(&self, pubkeys: &[Pubkey], slot: Slot, num: usize, count: usize) {
        for idx in 0..num {
            let account = AccountSharedData::new(
                (idx + count) as u64,
                0,
                AccountSharedData::default().owner(),
            );
            self.store_for_tests((slot, [(&pubkeys[idx], &account)].as_slice()));
        }
    }

    pub fn check_storage(&self, slot: Slot, alive_count: usize, total_count: usize) {
        let store = self.storage.get_slot_storage_entry(slot).unwrap();
        assert_eq!(store.count(), alive_count);
        assert_eq!(store.accounts_count(), total_count);
    }

    pub fn create_account(
        &self,
        pubkeys: &mut Vec<Pubkey>,
        slot: Slot,
        num: usize,
        space: usize,
        num_vote: usize,
    ) {
        let ancestors = Ancestors::from(vec![slot]);
        for t in 0..num {
            let pubkey = solana_pubkey::new_rand();
            let account =
                AccountSharedData::new((t + 1) as u64, space, AccountSharedData::default().owner());
            pubkeys.push(pubkey);
            assert!(self.do_load_for_tests(&ancestors, &pubkey).is_none());
            self.store_for_tests((slot, [(&pubkey, &account)].as_slice()));
        }
        for t in 0..num_vote {
            let pubkey = solana_pubkey::new_rand();
            let account =
                AccountSharedData::new((num + t + 1) as u64, space, &solana_vote_program::id());
            pubkeys.push(pubkey);
            let ancestors = Ancestors::from(vec![slot]);
            assert!(self.do_load_for_tests(&ancestors, &pubkey).is_none());
            self.store_for_tests((slot, [(&pubkey, &account)].as_slice()));
        }
    }

    pub fn alive_account_count_in_slot(&self, slot: Slot) -> usize {
        self.storage
            .get_slot_storage_entry(slot)
            .map(|storage| storage.count())
            .unwrap_or(0)
            .saturating_add(
                self.accounts_cache
                    .slot_cache(slot)
                    .map(|slot_cache| slot_cache.len())
                    .unwrap_or_default(),
            )
    }

    /// useful to adapt tests written prior to introduction of the write cache
    /// to use the write cache
    pub fn flush_root_write_cache(&self, root: Slot) {
        assert!(self.accounts_cache.contains_unflushed_root(root));
        self.flush_accounts_cache(true, Some(root));
    }

    pub fn all_account_count_in_accounts_file(&self, slot: Slot) -> usize {
        let store = self.storage.get_slot_storage_entry(slot);
        if let Some(store) = store {
            store.accounts_count()
        } else {
            0
        }
    }
}
