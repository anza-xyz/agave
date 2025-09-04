//! Index generation functionality for AccountsDb.
//! This module contains the `generate_index` function and all its supporting
//! helper functions to build the accounts index from storage files.

#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    super::{AccountsDb, AccountsFileId, SlotLtHash},
    crate::{
        account_info::{AccountInfo, StorageLocation},
        account_storage::AccountStoragesOrderer,
        accounts_db::{
            stats::{ObsoleteAccountsStats, PurgeStats},
            AccountStorageEntry,
        },
        accounts_hash::AccountsLtHash,
        accounts_index::{
            in_mem_accounts_index::StartupStats, AccountsIndexScanResult, IsCached, ScanFilter,
        },
        accounts_index_storage::Startup,
        append_vec::{self, IndexInfo, IndexInfoInner},
        buffered_reader::RequiredLenBufFileRead,
        is_zero_lamport::IsZeroLamport,
        u64_align,
    },
    dashmap::DashMap,
    log::*,
    rayon::prelude::*,
    solana_account::ReadableAccount,
    solana_clock::Slot,
    solana_lattice_hash::lt_hash::LtHash,
    solana_measure::{meas_dur, measure::Measure, measure_us},
    solana_nohash_hasher::BuildNoHashHasher,
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::{
        boxed::Box,
        collections::{HashMap, HashSet},
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, Mutex,
        },
        thread,
        time::{Duration, Instant},
    },
};

/// Information returned from index generation
pub struct IndexGenerationInfo {
    pub accounts_data_len: u64,
    /// The accounts lt hash calculated during index generation.
    /// Will be used when verifying accounts, after rebuilding a Bank.
    pub calculated_accounts_lt_hash: AccountsLtHash,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct StorageSizeAndCount {
    /// total size stored, including both alive and dead bytes
    pub stored_size: usize,
    /// number of accounts in the storage including both alive and dead accounts
    pub count: usize,
}

/// Map from accounts file ID to storage size and count
pub type StorageSizeAndCountMap = DashMap<u32, StorageSizeAndCount, BuildNoHashHasher<u32>>;

/// Timings for various phases of index generation
#[derive(Default, Debug)]
pub struct GenerateIndexTimings {
    pub total_time_us: u64,
    pub index_time: u64,
    pub scan_time: u64,
    pub insertion_time_us: u64,
    pub min_bin_size: usize,
    pub max_bin_size: usize,
    #[allow(dead_code)]
    pub total_items: usize,
    pub storage_size_storages_us: u64,
    pub index_flush_us: u64,
    pub accounts_data_len_dedup_time_us: u64,
    pub total_duplicate_slot_keys: u64,
    pub total_num_unique_duplicate_keys: u64,
    pub populate_duplicate_keys_us: u64,
    pub total_including_duplicates: u64,
    pub total_slots: u64,
    pub num_duplicate_accounts: u64,
    pub mark_obsolete_accounts_us: u64,
    pub num_obsolete_accounts_marked: u64,
    pub num_slots_removed_as_obsolete: u64,
    pub all_accounts_are_zero_lamports_slots: u64,
    pub visit_zero_lamports_us: u64,
    pub num_zero_lamport_single_refs: u64,
    pub par_duplicates_lt_hash_us: AtomicU64,
}

impl GenerateIndexTimings {
    fn report(&self, _startup_stats: &StartupStats) {
        datapoint_info!(
            "generate_index",
            ("overall_us", self.total_time_us, i64),
            ("total_slots", self.total_slots, i64),
            ("index_time", self.index_time, i64),
            ("scan_stores", self.scan_time, i64),
            ("insertion_time_us", self.insertion_time_us, i64),
            ("min_bin_size", self.min_bin_size as i64, i64),
            ("max_bin_size", self.max_bin_size as i64, i64),
            (
                "storage_size_storages_us",
                self.storage_size_storages_us,
                i64
            ),
            ("index_flush_us", self.index_flush_us, i64),
            (
                "accounts_data_len_dedup_time_us",
                self.accounts_data_len_dedup_time_us,
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
            (
                "populate_duplicate_keys_us",
                self.populate_duplicate_keys_us,
                i64
            ),
            (
                "total_including_duplicates",
                self.total_including_duplicates,
                i64
            ),
            ("num_duplicate_accounts", self.num_duplicate_accounts, i64),
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
                "all_accounts_are_zero_lamports_slots",
                self.all_accounts_are_zero_lamports_slots,
                i64
            ),
            ("visit_zero_lamports_us", self.visit_zero_lamports_us, i64),
            (
                "num_zero_lamport_single_refs",
                self.num_zero_lamport_single_refs,
                i64
            ),
            (
                "par_duplicates_lt_hash_us",
                self.par_duplicates_lt_hash_us.load(Ordering::Relaxed),
                i64
            ),
        );
        // startup_stats.log();  // No log method available on StartupStats
    }
}

/// Information about slot-specific index generation
#[derive(Debug, Default)]
pub struct SlotIndexGenerationInfo {
    insert_time_us: u64,
    num_accounts: u64,
    accounts_data_len: u64,
    zero_lamport_pubkeys: Vec<Pubkey>,
    all_accounts_are_zero_lamports: bool,
    /// Number of accounts in this slot that didn't already exist in the index
    num_did_not_exist: u64,
    /// Number of accounts in this slot that already existed, and were in-mem
    num_existed_in_mem: u64,
    /// Number of accounts in this slot that already existed, and were on-disk
    num_existed_on_disk: u64,
    /// The accounts lt hash *of only this slot*
    slot_lt_hash: SlotLtHash,
}

/// Accumulator for index generation across multiple threads/slots
#[derive(Debug)]
struct IndexGenerationAccumulator {
    insert_us: u64,
    num_accounts: u64,
    accounts_data_len: u64,
    zero_lamport_pubkeys: HashSet<Pubkey, PubkeyHasherBuilder>,
    all_accounts_are_zero_lamports_slots: u64,
    all_zeros_slots: Vec<(u64, Arc<AccountStorageEntry>)>,
    num_did_not_exist: u64,
    num_existed_in_mem: u64,
    num_existed_on_disk: u64,
    lt_hash: Box<LtHash>,
}

impl IndexGenerationAccumulator {
    fn new() -> Self {
        Self {
            insert_us: 0,
            num_accounts: 0,
            accounts_data_len: 0,
            zero_lamport_pubkeys: HashSet::with_hasher(PubkeyHasherBuilder::default()),
            all_accounts_are_zero_lamports_slots: 0,
            all_zeros_slots: Vec::default(),
            num_did_not_exist: 0,
            num_existed_in_mem: 0,
            num_existed_on_disk: 0,
            lt_hash: Box::new(LtHash::identity()),
        }
    }

    fn accumulate(&mut self, other: IndexGenerationAccumulator) {
        self.insert_us += other.insert_us;
        self.num_accounts += other.num_accounts;
        self.accounts_data_len += other.accounts_data_len;
        self.zero_lamport_pubkeys.extend(other.zero_lamport_pubkeys);
        self.all_accounts_are_zero_lamports_slots += other.all_accounts_are_zero_lamports_slots;
        self.all_zeros_slots.extend(other.all_zeros_slots);
        self.num_did_not_exist += other.num_did_not_exist;
        self.num_existed_in_mem += other.num_existed_in_mem;
        self.num_existed_on_disk += other.num_existed_on_disk;
        self.lt_hash.mix_in(&other.lt_hash);
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

/// Information about duplicate pubkeys visited during startup
#[derive(Debug, Default)]
struct DuplicatePubkeysVisitedInfo {
    accounts_data_len_from_duplicates: u64,
    num_duplicate_accounts: u64,
    duplicates_lt_hash: Box<DuplicatesLtHash>,
}
impl DuplicatePubkeysVisitedInfo {
    fn reduce(mut self, other: Self) -> Self {
        self.accounts_data_len_from_duplicates += other.accounts_data_len_from_duplicates;
        self.num_duplicate_accounts += other.num_duplicate_accounts;
        self.duplicates_lt_hash
            .0
            .mix_in(&other.duplicates_lt_hash.0);
        self
    }
}

impl AccountsDb {
    /// Generate the accounts index from all storage files
    ///
    /// This function rebuilds the in-memory accounts index from persistent storage.
    /// It processes all account storage files in parallel, building up an index
    /// that maps each account pubkey to its storage location and metadata.
    pub fn generate_index(
        &self,
        limit_load_slot_count_from_snapshot: Option<usize>,
        verify: bool,
    ) -> IndexGenerationInfo {
        // Step 1: Prepare storages and stats
        let mut total_time = Measure::start("generate_index");

        let mut storages = self.storage.all_storages();
        storages.sort_unstable_by_key(|storage| storage.slot);
        if let Some(limit) = limit_load_slot_count_from_snapshot {
            storages.truncate(limit); // get rid of the newer slots and keep just the older
        }
        let num_storages = storages.len();

        self.accounts_index
            .set_startup(Startup::StartupWithExtraThreads);
        let storage_info = StorageSizeAndCountMap::default();

        // Step 2: Generate index in parallel
        let (mut total_accum, index_time) =
            self.parallel_generate_index(&storages, &storage_info, num_storages);

        // Step 3: Update index stats
        self.update_index_stats(&total_accum);

        // Step 4: Optionally verify index
        if verify {
            self.verify_index(&storages);
        }

        // Step 5: Handle duplicated keys
        let (unique_pubkeys_by_bin, mut timings) =
            self.handle_duplicates(&index_time, &total_accum, num_storages);

        // Step 6: Visit zero lamport pubkeys
        let (num_zero_lamport_single_refs, visit_zero_lamports_us) = measure_us!(
            self.visit_zero_lamport_pubkeys_during_startup(&total_accum.zero_lamport_pubkeys)
        );
        timings.visit_zero_lamports_us = visit_zero_lamports_us;
        timings.num_zero_lamport_single_refs = num_zero_lamport_single_refs;

        // Step 7: Deduplicate accounts data
        self.deduplicate_accounts(&unique_pubkeys_by_bin, &mut timings, &mut total_accum);

        // Step 8: Insert zero lamport slots for cleaning
        self.insert_zero_lamport_slots(&total_accum);

        // Step 9: Add roots for all storages
        for storage in &storages {
            self.accounts_index.add_root(storage.slot());
        }

        // Step 10: Set storage count and alive bytes
        self.set_storage_count_and_alive_bytes(storage_info, &mut timings);

        // Step 11: Mark obsolete accounts if enabled
        self.mark_obsolete_accounts_if_enabled(&storages, unique_pubkeys_by_bin, &mut timings);

        // Step 12: Finalize timings and log
        total_time.stop();
        timings.total_time_us = total_time.as_us();
        timings.report(self.accounts_index.get_startup_stats());
        self.accounts_index.log_secondary_indexes();

        // Step 13: Return index generation info
        IndexGenerationInfo {
            accounts_data_len: total_accum.accounts_data_len,
            calculated_accounts_lt_hash: AccountsLtHash(*total_accum.lt_hash),
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub(crate)))]
    fn generate_index_for_slot<'a>(
        &self,
        reader: &mut impl RequiredLenBufFileRead<'a>,
        storage: &'a AccountStorageEntry,
        slot: Slot,
        store_id: AccountsFileId,
        storage_info: &StorageSizeAndCountMap,
    ) -> SlotIndexGenerationInfo {
        if storage.accounts.get_account_data_lens(&[0]).is_empty() {
            return SlotIndexGenerationInfo::default();
        }

        let mut accounts_data_len = 0;
        let mut stored_size_alive = 0;
        let mut zero_lamport_pubkeys = vec![];
        let mut all_accounts_are_zero_lamports = true;
        let mut slot_lt_hash = SlotLtHash::default();

        let (insert_time_us, generate_index_results) = {
            let mut keyed_account_infos = vec![];
            // this closure is the shared code when scanning the storage
            let mut itemizer = |info: IndexInfo| {
                stored_size_alive += info.stored_size_aligned;
                if info.index_info.lamports > 0 {
                    accounts_data_len += info.index_info.data_len;
                    all_accounts_are_zero_lamports = false;
                } else {
                    // zero lamport accounts
                    zero_lamport_pubkeys.push(info.index_info.pubkey);
                }
                keyed_account_infos.push((
                    info.index_info.pubkey,
                    AccountInfo::new(
                        StorageLocation::AppendVec(store_id, info.index_info.offset), // will never be cached
                        info.index_info.is_zero_lamport(),
                    ),
                ));
            };

            storage
                .accounts
                .scan_accounts(reader, |offset, account| {
                    let data_len = account.data.len() as u64;
                    let stored_size_aligned =
                        storage.accounts.calculate_stored_size(data_len as usize);
                    let info = IndexInfo {
                        stored_size_aligned,
                        index_info: IndexInfoInner {
                            offset,
                            pubkey: *account.pubkey,
                            lamports: account.lamports,
                            data_len,
                        },
                    };
                    itemizer(info);
                    if !self.account_indexes.is_empty() {
                        self.accounts_index.update_secondary_indexes(
                            account.pubkey,
                            &account,
                            &self.account_indexes,
                        );
                    }

                    let account_lt_hash = Self::lt_hash_account(&account, account.pubkey());
                    slot_lt_hash.0.mix_in(&account_lt_hash.0);
                })
                .expect("must scan accounts storage");
            self.accounts_index
                .insert_new_if_missing_into_primary_index(slot, keyed_account_infos)
        };

        {
            // second, collect into the shared DashMap once we've figured out all the info per store_id
            let mut info = storage_info.entry(store_id).or_default();
            info.stored_size += stored_size_alive;
            info.count += generate_index_results.count;

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
        }
        // zero_lamport_pubkeys are candidates for cleaning. So add them to uncleaned_pubkeys
        // for later cleaning. If there is just a single item, there is no cleaning to
        // be done on that pubkey. Use only those pubkeys with multiple updates.
        if !zero_lamport_pubkeys.is_empty() {
            let old = self
                .uncleaned_pubkeys
                .insert(slot, zero_lamport_pubkeys.clone());

            assert!(old.is_none());
        }
        SlotIndexGenerationInfo {
            insert_time_us,
            num_accounts: generate_index_results.count as u64,
            accounts_data_len,
            zero_lamport_pubkeys,
            all_accounts_are_zero_lamports,
            num_did_not_exist: generate_index_results.num_did_not_exist,
            num_existed_in_mem: generate_index_results.num_existed_in_mem,
            num_existed_on_disk: generate_index_results.num_existed_on_disk,
            slot_lt_hash,
        }
    }

    /// Parallel index generation for storages
    /// Returns (total_accumulator, index_time_measure)
    fn parallel_generate_index(
        &self,
        storages: &[Arc<AccountStorageEntry>],
        storage_info: &StorageSizeAndCountMap,
        num_storages: usize,
    ) -> (IndexGenerationAccumulator, Measure) {
        let mut total_accum = IndexGenerationAccumulator::new();
        let storages_orderer =
            AccountStoragesOrderer::with_random_order(storages).into_concurrent_consumer();
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
                            let mut thread_accum = IndexGenerationAccumulator::new();
                            let mut reader = append_vec::new_scan_accounts_reader();
                            while let Some(next_item) = storages_orderer.next() {
                                self.maybe_throttle_index_generation();
                                let storage = next_item.storage;
                                let store_id = storage.id();
                                let slot = storage.slot();
                                let slot_info = self.generate_index_for_slot(
                                    &mut reader,
                                    storage,
                                    slot,
                                    store_id,
                                    storage_info,
                                );
                                thread_accum.insert_us += slot_info.insert_time_us;
                                thread_accum.num_accounts += slot_info.num_accounts;
                                thread_accum.accounts_data_len += slot_info.accounts_data_len;
                                thread_accum
                                    .zero_lamport_pubkeys
                                    .extend(slot_info.zero_lamport_pubkeys);
                                if slot_info.all_accounts_are_zero_lamports {
                                    thread_accum.all_accounts_are_zero_lamports_slots += 1;
                                    thread_accum.all_zeros_slots.push((
                                        slot,
                                        Arc::clone(&storages[next_item.original_index]),
                                    ));
                                }
                                thread_accum.num_did_not_exist += slot_info.num_did_not_exist;
                                thread_accum.num_existed_in_mem += slot_info.num_existed_in_mem;
                                thread_accum.num_existed_on_disk += slot_info.num_existed_on_disk;
                                thread_accum.lt_hash.mix_in(&slot_info.slot_lt_hash.0);
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
                            info!("generating index: processed {num_processed}/{num_storages} slots...");
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

        (total_accum, index_time)
    }

    /// Deduplicate accounts data by processing duplicate pubkeys in parallel
    /// Updates the total accumulator and timings as part of the process
    fn deduplicate_accounts(
        &self,
        unique_pubkeys_by_bin: &[Vec<Pubkey>],
        timings: &mut GenerateIndexTimings,
        total_accum: &mut IndexGenerationAccumulator,
    ) {
        // subtract data.len() from accounts_data_len for all old accounts that are in the index twice
        let mut accounts_data_len_dedup_timer =
            Measure::start("handle accounts data len duplicates");
        let DuplicatePubkeysVisitedInfo {
            accounts_data_len_from_duplicates,
            num_duplicate_accounts,
            duplicates_lt_hash,
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
                            ) = self.visit_duplicate_pubkeys_during_startup(pubkeys, timings);
                            let intermediate = DuplicatePubkeysVisitedInfo {
                                accounts_data_len_from_duplicates,
                                num_duplicate_accounts: accounts_duplicates_num,
                                duplicates_lt_hash,
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
        accounts_data_len_dedup_timer.stop();
        timings.accounts_data_len_dedup_time_us = accounts_data_len_dedup_timer.as_us();
        timings.num_duplicate_accounts = num_duplicate_accounts;

        total_accum.lt_hash.mix_out(&duplicates_lt_hash.0);
        total_accum.accounts_data_len -= accounts_data_len_from_duplicates;
        info!("accounts data len: {}", total_accum.accounts_data_len);
    }

    /// Insert zero lamport account storage into dirty stores for cleaning
    fn insert_zero_lamport_slots(&self, total_accum: &IndexGenerationAccumulator) {
        // insert all zero lamport account storage into the dirty stores and add them into the uncleaned roots for clean to pick up
        info!(
            "insert all zero slots to clean at startup {}",
            total_accum.all_zeros_slots.len()
        );
        for (slot, storage) in &total_accum.all_zeros_slots {
            self.dirty_stores.insert(*slot, storage.clone());
        }
    }

    /// Handle duplicate keys from the accounts index after initial index generation
    /// Returns (unique_pubkeys_by_bin, timings)
    fn handle_duplicates(
        &self,
        index_time: &Measure,
        total_accum: &IndexGenerationAccumulator,
        num_storages: usize,
    ) -> (Vec<Vec<Pubkey>>, GenerateIndexTimings) {
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
                        HashSet::<Pubkey>::from_iter(slot_keys.iter().map(|(_, key)| *key));
                    for (slot, key) in slot_keys {
                        self.uncleaned_pubkeys.entry(slot).or_default().push(key);
                    }
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

        let timings = GenerateIndexTimings {
            index_flush_us,
            scan_time: 0,
            index_time: index_time.as_us(),
            insertion_time_us: total_accum.insert_us,
            total_duplicate_slot_keys: total_duplicate_slot_keys.load(Ordering::Relaxed),
            total_num_unique_duplicate_keys: total_num_unique_duplicate_keys
                .load(Ordering::Relaxed),
            populate_duplicate_keys_us,
            total_including_duplicates: total_accum.num_accounts,
            total_slots: num_storages as u64,
            all_accounts_are_zero_lamports_slots: total_accum.all_accounts_are_zero_lamports_slots,
            ..GenerateIndexTimings::default()
        };

        (unique_pubkeys_by_bin, timings)
    }

    /// Verify the generated index by checking that all accounts in storage
    /// have corresponding entries in the accounts index
    fn verify_index(&self, storages: &[Arc<AccountStorageEntry>]) {
        info!("Verifying index...");
        let start = Instant::now();
        storages.par_iter().for_each(|storage| {
            let store_id = storage.id();
            let slot = storage.slot();
            storage
                .accounts
                .scan_accounts_without_data(|offset, account| {
                    let key = account.pubkey();
                    let index_entry = self.accounts_index.get_cloned(key).unwrap();
                    let slot_list = index_entry.slot_list.read().unwrap();
                    let mut count = 0;
                    for (slot2, account_info2) in slot_list.iter() {
                        if *slot2 == slot {
                            count += 1;
                            let ai = AccountInfo::new(
                                StorageLocation::AppendVec(store_id, offset), // will never be cached
                                account.is_zero_lamport(),
                            );
                            assert_eq!(&ai, account_info2);
                        }
                    }
                    assert_eq!(1, count);
                })
                .expect("must scan accounts storage");
        });
        info!("Verifying index... Done in {:?}", start.elapsed());
    }

    /// Update index statistics based on the accumulator data
    fn update_index_stats(&self, total_accum: &IndexGenerationAccumulator) {
        // Update the index stats now.
        let index_stats = self.accounts_index.bucket_map_holder_stats();

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

    /// Mark obsolete accounts if the feature is enabled
    /// Updates timings with the results
    fn mark_obsolete_accounts_if_enabled(
        &self,
        storages: &[Arc<AccountStorageEntry>],
        unique_pubkeys_by_bin: Vec<Vec<Pubkey>>,
        timings: &mut GenerateIndexTimings,
    ) {
        if self.mark_obsolete_accounts == crate::accounts_db::MarkObsoleteAccounts::Enabled {
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
        }
    }

    /// Use the duplicated pubkeys to mark all older version of the pubkeys as obsolete
    /// This will unref the accounts and then reclaim the accounts
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn mark_obsolete_accounts_at_startup(
        &self,
        slot_marked_obsolete: Slot,
        pubkeys_with_duplicates_by_bin: Vec<Vec<Pubkey>>,
    ) -> ObsoleteAccountsStats {
        let stats: ObsoleteAccountsStats = pubkeys_with_duplicates_by_bin
            .par_iter()
            .map(|pubkeys_by_bin| {
                let reclaims = self.accounts_index.clean_and_unref_rooted_entries_by_bin(
                    pubkeys_by_bin,
                    |slot, account_info| {
                        // Since the unref makes every account a single ref account, all
                        // zero lamport accounts should be tracked as zero_lamport_single_ref
                        if account_info.is_zero_lamport() {
                            self.zero_lamport_single_ref_found(slot, account_info.offset());
                        }
                    },
                );
                let stats = PurgeStats::default();

                // Mark all the entries as obsolete, and remove any empty storages
                self.handle_reclaims(
                    (!reclaims.is_empty()).then(|| reclaims.iter()),
                    None,
                    &HashSet::new(),
                    crate::accounts_db::HandleReclaims::ProcessDeadSlots(&stats),
                    crate::accounts_db::MarkAccountsObsolete::Yes(slot_marked_obsolete),
                );
                ObsoleteAccountsStats {
                    accounts_marked_obsolete: reclaims.len() as u64,
                    slots_removed: stats.total_removed_storage_entries.load(Ordering::Relaxed)
                        as u64,
                }
            })
            .sum();
        stats
    }

    /// Startup processes can consume large amounts of memory while inserting accounts into the index as fast as possible.
    /// Calling this can slow down the insertion process to allow flushing to disk to keep pace.
    fn maybe_throttle_index_generation(&self) {
        // Only throttle if we are generating on-disk index. Throttling is not needed for in-mem index.
        if !self.accounts_index.is_disk_index_enabled() {
            return;
        }
        // This number is chosen to keep the initial ram usage sufficiently small
        // The process of generating the index is governed entirely by how fast the disk index can be populated.
        // 10M accounts is sufficiently small that it will never have memory usage. It seems sufficiently large that it will provide sufficient performance.
        // Performance is measured by total time to generate the index.
        // Just estimating - 150M accounts can easily be held in memory in the accounts index on a 256G machine. 2-300M are also likely 'fine' during startup.
        // 550M was straining a 384G machine at startup.
        // This is a tunable parameter that just needs to be small enough to keep the generation threads from overwhelming RAM and oom at startup.
        const LIMIT: usize = 10_000_000;
        while self
            .accounts_index
            .get_startup_remaining_items_to_flush_estimate()
            > LIMIT
        {
            // 10 ms is long enough to allow some flushing to occur before insertion is resumed.
            // callers of this are typically run in parallel, so many threads will be sleeping at different starting intervals, waiting to resume insertion.
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Visit zero lamport pubkeys and populate zero_lamport_single_ref info on
    /// storage.
    /// Returns the number of zero lamport single ref accounts found.
    fn visit_zero_lamport_pubkeys_during_startup(
        &self,
        pubkeys: &HashSet<Pubkey, PubkeyHasherBuilder>,
    ) -> u64 {
        let mut slot_offsets = HashMap::<_, Vec<_>>::default();
        self.accounts_index.scan(
            pubkeys.iter(),
            |_pubkey, slots_refs, _entry| {
                let (slot_list, ref_count) = slots_refs.unwrap();
                if ref_count == 1 {
                    assert_eq!(slot_list.len(), 1);
                    let (slot_alive, account_info) = slot_list.first().unwrap();
                    assert!(!account_info.is_cached());
                    if account_info.is_zero_lamport() {
                        slot_offsets
                            .entry(*slot_alive)
                            .or_default()
                            .push(account_info.offset());
                    }
                }
                AccountsIndexScanResult::OnlyKeepInMemoryIfDirty
            },
            None,
            false,
            ScanFilter::All,
        );

        let mut count = 0;
        let mut dead_stores = 0;
        let mut shrink_stores = 0;
        let mut non_shrink_stores = 0;
        for (slot, offsets) in slot_offsets {
            if let Some(store) = self.storage.get_slot_storage_entry(slot) {
                count += store.batch_insert_zero_lamport_single_ref_account_offsets(&offsets);
                if store.num_zero_lamport_single_ref_accounts() == store.count() {
                    // all accounts in this storage can be dead
                    self.dirty_stores.entry(slot).or_insert(store);
                    dead_stores += 1;
                } else if Self::is_shrinking_productive(&store)
                    && self.is_candidate_for_shrink(&store)
                {
                    // this store might be eligible for shrinking now
                    if self.shrink_candidate_slots.lock().unwrap().insert(slot) {
                        shrink_stores += 1;
                    }
                } else {
                    non_shrink_stores += 1;
                }
            }
        }
        self.shrink_stats
            .num_zero_lamport_single_ref_accounts_found
            .fetch_add(count, Ordering::Relaxed);

        self.shrink_stats
            .num_dead_slots_added_to_clean
            .fetch_add(dead_stores, Ordering::Relaxed);

        self.shrink_stats
            .num_slots_with_zero_lamport_accounts_added_to_shrink
            .fetch_add(shrink_stores, Ordering::Relaxed);

        self.shrink_stats
            .marking_zero_dead_accounts_in_non_shrinkable_store
            .fetch_add(non_shrink_stores, Ordering::Relaxed);

        count
    }

    /// Used during generate_index() to:
    /// 1. get the _duplicate_ accounts data len from the given pubkeys
    /// 2. get the slots that contained duplicate pubkeys
    /// 3. build up the duplicates lt hash
    ///
    /// Note this should only be used when ALL entries in the accounts index are roots.
    ///
    /// returns tuple of:
    /// - data len sum of all older duplicates
    /// - number of duplicate accounts
    /// - lt hash of duplicates
    fn visit_duplicate_pubkeys_during_startup(
        &self,
        pubkeys: &[Pubkey],
        timings: &GenerateIndexTimings,
    ) -> (u64, u64, Box<DuplicatesLtHash>) {
        let mut accounts_data_len_from_duplicates = 0;
        let mut num_duplicate_accounts = 0_u64;
        let mut duplicates_lt_hash = Box::new(DuplicatesLtHash::default());
        let mut lt_hash_time = Duration::default();
        self.accounts_index.scan(
            pubkeys.iter(),
            |pubkey, slots_refs, _entry| {
                if let Some((slot_list, _ref_count)) = slots_refs {
                    if slot_list.len() > 1 {
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
                            let mut accessor = crate::accounts_db::LoadedAccountAccessor::Stored(
                                maybe_storage_entry.map(|entry| (entry, account_info.offset())),
                            );
                            accessor.check_and_get_loaded_account(|loaded_account| {
                                let data_len = loaded_account.data_len();
                                if loaded_account.lamports() > 0 {
                                    accounts_data_len_from_duplicates += data_len;
                                }
                                num_duplicate_accounts += 1;
                                let (_, duration) = meas_dur!({
                                    let account_lt_hash =
                                        Self::lt_hash_account(&loaded_account, pubkey);
                                    duplicates_lt_hash.0.mix_in(&account_lt_hash.0);
                                });
                                lt_hash_time += duration;
                            });
                        });
                    }
                }
                AccountsIndexScanResult::OnlyKeepInMemoryIfDirty
            },
            None,
            false,
            ScanFilter::All,
        );
        timings
            .par_duplicates_lt_hash_us
            .fetch_add(lt_hash_time.as_micros() as u64, Ordering::Relaxed);
        (
            accounts_data_len_from_duplicates as u64,
            num_duplicate_accounts,
            duplicates_lt_hash,
        )
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn set_storage_count_and_alive_bytes(
        &self,
        stored_sizes_and_counts: StorageSizeAndCountMap,
        timings: &mut GenerateIndexTimings,
    ) {
        // store count and size for each storage
        let mut storage_size_storages_time = Measure::start("storage_size_storages");
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
                    let mut count_and_status = store.count_and_status.lock_write();
                    assert_eq!(count_and_status.0, 0);
                    count_and_status.0 = entry.count;
                }
                store
                    .alive_bytes
                    .store(entry.stored_size, Ordering::Release);
            } else {
                trace!("id: {id} clearing count");
                store.count_and_status.lock_write().0 = 0;
            }
        }
        storage_size_storages_time.stop();
        timings.storage_size_storages_us = storage_size_storages_time.as_us();
    }
}
