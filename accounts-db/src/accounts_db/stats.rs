use {
    crate::{
        accounts_index::{AccountsIndexRootsStats, ZeroLamport},
        append_vec::{
            APPEND_VEC_MMAPPED_FILES_DIRTY, APPEND_VEC_MMAPPED_FILES_OPEN,
            APPEND_VEC_OPEN_AS_FILE_IO,
        },
    },
    serde::{Deserialize, Serialize},
    solana_sdk::{account::ReadableAccount, timing::AtomicInterval},
    std::{
        num::Saturating,
        sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

#[macro_export]
macro_rules! format_field {
    ($output:ident, $name:expr, $field:expr) => {
        $output.push_str($name);
        let field_output = format!("{:?}", $field);
        field_output
            .split('\n')
            .for_each(|s| $output.push_str(&format!("    {s}\n")));
    };
}

#[macro_export]
macro_rules! format_vector {
    ($output:ident, $name:expr, $field:expr) => {
        $output.push_str($name);
        for (x, v) in $field.iter().enumerate() {
            let prefix = format!("    {x}:");
            format_field!($output, &prefix, v);
        }
        $output.push_str("    ]\n");
    };
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BankHashStats {
    pub num_updated_accounts: u64,
    pub num_removed_accounts: u64,
    pub num_lamports_stored: u64,
    pub total_data_len: u64,
    pub num_executable_accounts: u64,
}

impl BankHashStats {
    pub fn update<T: ReadableAccount + ZeroLamport>(&mut self, account: &T) {
        if account.is_zero_lamport() {
            self.num_removed_accounts += 1;
        } else {
            self.num_updated_accounts += 1;
        }
        self.total_data_len = self
            .total_data_len
            .wrapping_add(account.data().len() as u64);
        if account.executable() {
            self.num_executable_accounts += 1;
        }
        self.num_lamports_stored = self.num_lamports_stored.wrapping_add(account.lamports());
    }

    pub fn accumulate(&mut self, other: &BankHashStats) {
        self.num_updated_accounts += other.num_updated_accounts;
        self.num_removed_accounts += other.num_removed_accounts;
        self.total_data_len = self.total_data_len.wrapping_add(other.total_data_len);
        self.num_lamports_stored = self
            .num_lamports_stored
            .wrapping_add(other.num_lamports_stored);
        self.num_executable_accounts += other.num_executable_accounts;
    }
}

#[derive(Default)]
pub struct AccountsStats {
    pub delta_hash_scan_time_total_us: AtomicU64,
    pub delta_hash_accumulate_time_total_us: AtomicU64,
    pub delta_hash_num: AtomicU64,
    pub skipped_rewrites_num: AtomicUsize,
    pub last_store_report: AtomicInterval,
    pub store_hash_accounts: AtomicU64,
    pub calc_stored_meta: AtomicU64,
    pub store_accounts: AtomicU64,
    pub store_update_index: AtomicU64,
    pub store_handle_reclaims: AtomicU64,
    pub store_append_accounts: AtomicU64,
    pub stakes_cache_check_and_store_us: AtomicU64,
    pub store_num_accounts: AtomicU64,
    pub store_total_data: AtomicU64,
    pub create_store_count: AtomicU64,
    pub store_get_slot_store: AtomicU64,
    pub store_find_existing: AtomicU64,
    pub dropped_stores: AtomicU64,
    pub store_uncleaned_update: AtomicU64,
    pub handle_dead_keys_us: AtomicU64,
    pub purge_exact_us: AtomicU64,
    pub purge_exact_count: AtomicU64,
}

impl std::fmt::Debug for AccountsStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut output = String::from("AccountsStatus {\n");
        output.push_str(&format!(
            "    delta_hash_scan_time_total_us: {:?}\n",
            self.delta_hash_scan_time_total_us
        ));
        output.push_str(&format!(
            "    delta_hash_accumulate_time_total_us: {:?}\n",
            self.delta_hash_accumulate_time_total_us
        ));
        output.push_str(&format!("    delta_hash_num: {:?}\n", self.delta_hash_num));
        output.push_str(&format!(
            "    skipped_rewrites_num: {:?}\n",
            self.skipped_rewrites_num
        ));
        output.push_str(&format!(
            "    last_store_report: {:?}\n",
            self.last_store_report
        ));
        output.push_str(&format!(
            "    store_hash_accounts: {:?}\n",
            self.store_hash_accounts
        ));
        output.push_str(&format!(
            "    calc_stored_meta: {:?}\n",
            self.calc_stored_meta
        ));
        output.push_str(&format!("    store_accounts: {:?}\n", self.store_accounts));
        output.push_str(&format!(
            "    store_update_index: {:?}\n",
            self.store_update_index
        ));
        output.push_str(&format!(
            "    store_handle_reclaims: {:?}\n",
            self.store_handle_reclaims
        ));
        output.push_str(&format!(
            "    store_append_accounts: {:?}\n",
            self.store_append_accounts
        ));
        output.push_str(&format!(
            "    stakes_cache_check_and_store_us: {:?}\n",
            self.stakes_cache_check_and_store_us
        ));
        output.push_str(&format!(
            "    store_num_accounts: {:?}\n",
            self.store_num_accounts
        ));
        output.push_str(&format!(
            "    store_total_data: {:?}\n",
            self.store_total_data
        ));
        output.push_str(&format!(
            "    create_store_count: {:?}\n",
            self.create_store_count
        ));
        output.push_str(&format!(
            "    store_get_slot_store: {:?}\n",
            self.store_get_slot_store
        ));
        output.push_str(&format!(
            "    store_find_existing: {:?}\n",
            self.store_find_existing
        ));
        output.push_str(&format!("    dropped_stores: {:?}\n", self.dropped_stores));
        output.push_str(&format!(
            "    store_uncleaned_update: {:?}\n",
            self.store_uncleaned_update
        ));
        output.push_str(&format!(
            "    handle_dead_keys_us: {:?}\n",
            self.handle_dead_keys_us
        ));
        output.push_str(&format!("    purge_exact_us: {:?}\n", self.purge_exact_us));
        output.push_str(&format!(
            "    purge_exact_count: {:?}\n",
            self.purge_exact_count
        ));
        write!(f, "{}}}", output)
    }
}

#[derive(Default)]
pub struct PurgeStats {
    pub last_report: AtomicInterval,
    pub safety_checks_elapsed: AtomicU64,
    pub remove_cache_elapsed: AtomicU64,
    pub remove_storage_entries_elapsed: AtomicU64,
    pub drop_storage_entries_elapsed: AtomicU64,
    pub num_cached_slots_removed: AtomicUsize,
    pub num_stored_slots_removed: AtomicUsize,
    pub total_removed_storage_entries: AtomicUsize,
    pub total_removed_cached_bytes: AtomicU64,
    pub total_removed_stored_bytes: AtomicU64,
    pub scan_storages_elapsed: AtomicU64,
    pub purge_accounts_index_elapsed: AtomicU64,
    pub handle_reclaims_elapsed: AtomicU64,
}

impl std::fmt::Debug for PurgeStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut output = String::from("PurgeStats {\n");
        output.push_str(&format!("    last_report: {:?}\n", self.last_report));
        output.push_str(&format!(
            "    safety_checks_elapsed: {:?}\n",
            self.safety_checks_elapsed
        ));
        output.push_str(&format!(
            "    remove_cache_elapsed: {:?}\n",
            self.remove_cache_elapsed
        ));
        output.push_str(&format!(
            "    remove_storage_entries_elapsed: {:?}\n",
            self.remove_storage_entries_elapsed
        ));
        output.push_str(&format!(
            "    drop_storage_entries_elapsed: {:?}\n",
            self.drop_storage_entries_elapsed
        ));
        output.push_str(&format!(
            "    num_cached_slots_removed: {:?}\n",
            self.num_cached_slots_removed
        ));
        output.push_str(&format!(
            "    num_stored_slots_removed: {:?}\n",
            self.num_stored_slots_removed
        ));
        output.push_str(&format!(
            "    total_removed_storage_entries: {:?}\n",
            self.total_removed_storage_entries
        ));
        output.push_str(&format!(
            "    total_removed_cached_bytes: {:?}\n",
            self.total_removed_cached_bytes
        ));
        output.push_str(&format!(
            "    total_removed_stored_bytes: {:?}\n",
            self.total_removed_stored_bytes
        ));
        output.push_str(&format!(
            "    scan_storages_elapsed: {:?}\n",
            self.scan_storages_elapsed
        ));
        output.push_str(&format!(
            "    purge_accounts_index_elapsed: {:?}\n",
            self.purge_accounts_index_elapsed
        ));
        output.push_str(&format!(
            "    handle_reclaims_elapsed: {:?}\n",
            self.handle_reclaims_elapsed
        ));
        write!(f, "{}}}", output)
    }
}

impl PurgeStats {
    pub fn report(&self, metric_name: &'static str, report_interval_ms: Option<u64>) {
        let should_report = report_interval_ms
            .map(|report_interval_ms| self.last_report.should_update(report_interval_ms))
            .unwrap_or(true);

        if should_report {
            datapoint_info!(
                metric_name,
                (
                    "safety_checks_elapsed",
                    self.safety_checks_elapsed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "remove_cache_elapsed",
                    self.remove_cache_elapsed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "remove_storage_entries_elapsed",
                    self.remove_storage_entries_elapsed
                        .swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "drop_storage_entries_elapsed",
                    self.drop_storage_entries_elapsed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "num_cached_slots_removed",
                    self.num_cached_slots_removed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "num_stored_slots_removed",
                    self.num_stored_slots_removed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "total_removed_storage_entries",
                    self.total_removed_storage_entries
                        .swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "total_removed_cached_bytes",
                    self.total_removed_cached_bytes.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "total_removed_stored_bytes",
                    self.total_removed_stored_bytes.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "scan_storages_elapsed",
                    self.scan_storages_elapsed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "purge_accounts_index_elapsed",
                    self.purge_accounts_index_elapsed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "handle_reclaims_elapsed",
                    self.handle_reclaims_elapsed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
            );
        }
    }
}

#[derive(Default, Debug)]
pub struct StoreAccountsTiming {
    pub store_accounts_elapsed: u64,
    pub update_index_elapsed: u64,
    pub handle_reclaims_elapsed: u64,
}

impl StoreAccountsTiming {
    pub fn accumulate(&mut self, other: &Self) {
        self.store_accounts_elapsed += other.store_accounts_elapsed;
        self.update_index_elapsed += other.update_index_elapsed;
        self.handle_reclaims_elapsed += other.handle_reclaims_elapsed;
    }
}

#[derive(Debug, Default)]
pub struct FlushStats {
    pub num_flushed: Saturating<usize>,
    pub num_purged: Saturating<usize>,
    pub total_size: Saturating<u64>,
    pub store_accounts_timing: StoreAccountsTiming,
    pub store_accounts_total_us: Saturating<u64>,
}

impl FlushStats {
    pub fn accumulate(&mut self, other: &Self) {
        self.num_flushed += other.num_flushed;
        self.num_purged += other.num_purged;
        self.total_size += other.total_size;
        self.store_accounts_timing
            .accumulate(&other.store_accounts_timing);
        self.store_accounts_total_us += other.store_accounts_total_us;
    }
}

#[derive(Default)]
pub struct LatestAccountsIndexRootsStats {
    pub roots_len: AtomicUsize,
    pub uncleaned_roots_len: AtomicUsize,
    pub roots_range: AtomicU64,
    pub rooted_cleaned_count: AtomicUsize,
    pub unrooted_cleaned_count: AtomicUsize,
    pub clean_unref_from_storage_us: AtomicU64,
    pub clean_dead_slot_us: AtomicU64,
}

impl std::fmt::Debug for LatestAccountsIndexRootsStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut output = String::from("LatestAccountsIndexRootsStats {\n");
        output.push_str(&format!("    roots_len: {:?}\n", self.roots_len));
        output.push_str(&format!(
            "    uncleaned_roots_len: {:?}\n",
            self.uncleaned_roots_len
        ));
        output.push_str(&format!("    roots_range: {:?}\n", self.roots_range));
        output.push_str(&format!(
            "    rooted_cleaned_count: {:?}\n",
            self.rooted_cleaned_count
        ));
        output.push_str(&format!(
            "    unrooted_cleaned_count: {:?}\n",
            self.unrooted_cleaned_count
        ));
        output.push_str(&format!(
            "    clean_unref_from_storage_us: {:?}\n",
            self.clean_unref_from_storage_us
        ));
        output.push_str(&format!(
            "    clean_dead_slot_us: {:?}\n",
            self.clean_dead_slot_us
        ));
        write!(f, "{}}}", output)
    }
}

impl LatestAccountsIndexRootsStats {
    pub fn update(&self, accounts_index_roots_stats: &AccountsIndexRootsStats) {
        if let Some(value) = accounts_index_roots_stats.roots_len {
            self.roots_len.store(value, Ordering::Relaxed);
        }
        if let Some(value) = accounts_index_roots_stats.uncleaned_roots_len {
            self.uncleaned_roots_len.store(value, Ordering::Relaxed);
        }
        if let Some(value) = accounts_index_roots_stats.roots_range {
            self.roots_range.store(value, Ordering::Relaxed);
        }
        self.rooted_cleaned_count.fetch_add(
            accounts_index_roots_stats.rooted_cleaned_count,
            Ordering::Relaxed,
        );
        self.unrooted_cleaned_count.fetch_add(
            accounts_index_roots_stats.unrooted_cleaned_count,
            Ordering::Relaxed,
        );
        self.clean_unref_from_storage_us.fetch_add(
            accounts_index_roots_stats.clean_unref_from_storage_us,
            Ordering::Relaxed,
        );
        self.clean_dead_slot_us.fetch_add(
            accounts_index_roots_stats.clean_dead_slot_us,
            Ordering::Relaxed,
        );
    }

    pub fn report(&self) {
        datapoint_info!(
            "accounts_index_roots_len",
            (
                "roots_len",
                self.roots_len.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "uncleaned_roots_len",
                self.uncleaned_roots_len.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "roots_range_width",
                self.roots_range.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "unrooted_cleaned_count",
                self.unrooted_cleaned_count.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "rooted_cleaned_count",
                self.rooted_cleaned_count.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "clean_unref_from_storage_us",
                self.clean_unref_from_storage_us.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "clean_dead_slot_us",
                self.clean_dead_slot_us.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "append_vecs_open",
                APPEND_VEC_MMAPPED_FILES_OPEN.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "append_vecs_dirty",
                APPEND_VEC_MMAPPED_FILES_DIRTY.load(Ordering::Relaxed),
                i64
            ),
            (
                "append_vecs_open_as_file_io",
                APPEND_VEC_OPEN_AS_FILE_IO.load(Ordering::Relaxed),
                i64
            )
        );

        // Don't need to reset since this tracks the latest updates, not a cumulative total
    }
}

#[derive(Default)]
pub struct CleanAccountsStats {
    pub purge_stats: PurgeStats,
    pub latest_accounts_index_roots_stats: LatestAccountsIndexRootsStats,

    // stats held here and reported by clean_accounts
    pub clean_old_root_us: AtomicU64,
    pub clean_old_root_reclaim_us: AtomicU64,
    pub reset_uncleaned_roots_us: AtomicU64,
    pub remove_dead_accounts_remove_us: AtomicU64,
    pub remove_dead_accounts_shrink_us: AtomicU64,
    pub clean_stored_dead_slots_us: AtomicU64,
    pub uncleaned_roots_slot_list_1: AtomicU64,
    pub get_account_sizes_us: AtomicU64,
    pub slots_cleaned: AtomicU64,
}

impl CleanAccountsStats {
    pub fn report(&self) {
        self.purge_stats.report("clean_purge_slots_stats", None);
        self.latest_accounts_index_roots_stats.report();
    }
}

impl std::fmt::Debug for CleanAccountsStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut output = String::from("CleanAccountsStatus {\n");
        format_field!(output, "    purge_stats:", self.purge_stats);
        format_field!(
            output,
            "    latest_accounts_index_roots_stats:",
            self.latest_accounts_index_roots_stats
        );
        output.push_str(&format!(
            "    clean_old_root_us: {:?}\n",
            self.clean_old_root_us
        ));
        output.push_str(&format!(
            "    clean_old_root_reclaim_us: {:?}\n",
            self.clean_old_root_reclaim_us
        ));
        output.push_str(&format!(
            "    reset_uncleaned_roots_us: {:?}\n",
            self.reset_uncleaned_roots_us
        ));
        output.push_str(&format!(
            "    remove_dead_accounts_remove_us: {:?}\n",
            self.remove_dead_accounts_remove_us
        ));
        output.push_str(&format!(
            "    remove_dead_accounts_shrink_us: {:?}\n",
            self.remove_dead_accounts_shrink_us
        ));
        output.push_str(&format!(
            "    clean_stored_dead_slots_us: {:?}\n",
            self.clean_stored_dead_slots_us
        ));
        output.push_str(&format!(
            "    uncleaned_roots_slot_list_1: {:?}\n",
            self.uncleaned_roots_slot_list_1
        ));
        output.push_str(&format!(
            "    get_account_sizes_us: {:?}\n",
            self.get_account_sizes_us
        ));
        output.push_str(&format!("    slots_cleaned: {:?}\n", self.slots_cleaned));
        write!(f, "{}}}", output)
    }
}

#[derive(Default)]
pub struct ShrinkAncientStats {
    pub shrink_stats: ShrinkStats,
    pub ancient_append_vecs_shrunk: AtomicU64,
    pub total_us: AtomicU64,
    pub random_shrink: AtomicU64,
    pub slots_considered: AtomicU64,
    pub ancient_scanned: AtomicU64,
    pub bytes_ancient_created: AtomicU64,
    pub bytes_from_must_shrink: AtomicU64,
    pub bytes_from_smallest_storages: AtomicU64,
    pub bytes_from_newest_storages: AtomicU64,
    pub many_ref_slots_skipped: AtomicU64,
    pub slots_cannot_move_count: AtomicU64,
    pub many_refs_old_alive: AtomicU64,
    pub slots_eligible_to_shrink: AtomicU64,
    pub total_dead_bytes: AtomicU64,
    pub total_alive_bytes: AtomicU64,
}

impl std::fmt::Debug for ShrinkAncientStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut output = String::from("ShrinkAncientStatus {\n");
        format_field!(output, "    shrink_stats:", self.shrink_stats);
        output.push_str(&format!(
            "    ancient_append_vecs_shrunk: {:?}\n",
            self.ancient_append_vecs_shrunk
        ));
        output.push_str(&format!("    total_us: {:?}\n", self.total_us));
        output.push_str(&format!("    random_shrink: {:?}\n", self.random_shrink));
        output.push_str(&format!(
            "    slots_considered: {:?}\n",
            self.slots_considered
        ));
        output.push_str(&format!(
            "    ancient_scanned: {:?}\n",
            self.ancient_scanned
        ));
        output.push_str(&format!(
            "    bytes_ancient_created: {:?}\n",
            self.bytes_ancient_created
        ));
        output.push_str(&format!(
            "    bytes_from_must_shrink: {:?}\n",
            self.bytes_from_must_shrink
        ));
        output.push_str(&format!(
            "    bytes_from_smallest_storages: {:?}\n",
            self.bytes_from_smallest_storages
        ));
        output.push_str(&format!(
            "    bytes_from_newest_storages: {:?}\n",
            self.bytes_from_newest_storages
        ));
        output.push_str(&format!(
            "    many_ref_slots_skipped: {:?}\n",
            self.many_ref_slots_skipped
        ));
        output.push_str(&format!(
            "    slots_cannot_move_count: {:?}\n",
            self.slots_cannot_move_count
        ));
        output.push_str(&format!(
            "    many_refs_old_alive: {:?}\n",
            self.many_refs_old_alive
        ));
        output.push_str(&format!(
            "    slots_eligible_to_shrink: {:?}\n",
            self.slots_eligible_to_shrink
        ));
        output.push_str(&format!(
            "    total_dead_bytes: {:?}\n",
            self.total_dead_bytes
        ));
        output.push_str(&format!(
            "    total_alive_bytes: {:?}\n",
            self.total_alive_bytes
        ));
        write!(f, "{}}}", output)
    }
}

#[derive(Debug, Default)]
pub struct ShrinkStatsSub {
    pub store_accounts_timing: StoreAccountsTiming,
    pub rewrite_elapsed_us: Saturating<u64>,
    pub create_and_insert_store_elapsed_us: Saturating<u64>,
    pub unpackable_slots_count: Saturating<usize>,
    pub newest_alive_packed_count: Saturating<usize>,
}

impl ShrinkStatsSub {
    pub fn accumulate(&mut self, other: &Self) {
        self.store_accounts_timing
            .accumulate(&other.store_accounts_timing);
        self.rewrite_elapsed_us += other.rewrite_elapsed_us;
        self.create_and_insert_store_elapsed_us += other.create_and_insert_store_elapsed_us;
        self.unpackable_slots_count += other.unpackable_slots_count;
        self.newest_alive_packed_count += other.newest_alive_packed_count;
    }
}

#[derive(Default)]
pub struct ShrinkStats {
    pub last_report: AtomicInterval,
    pub num_slots_shrunk: AtomicUsize,
    pub storage_read_elapsed: AtomicU64,
    pub num_duplicated_accounts: AtomicU64,
    pub index_read_elapsed: AtomicU64,
    pub create_and_insert_store_elapsed: AtomicU64,
    pub store_accounts_elapsed: AtomicU64,
    pub update_index_elapsed: AtomicU64,
    pub handle_reclaims_elapsed: AtomicU64,
    pub remove_old_stores_shrink_us: AtomicU64,
    pub rewrite_elapsed: AtomicU64,
    pub unpackable_slots_count: AtomicU64,
    pub newest_alive_packed_count: AtomicU64,
    pub drop_storage_entries_elapsed: AtomicU64,
    pub accounts_removed: AtomicUsize,
    pub bytes_removed: AtomicU64,
    pub bytes_written: AtomicU64,
    pub skipped_shrink: AtomicU64,
    pub dead_accounts: AtomicU64,
    pub alive_accounts: AtomicU64,
    pub index_scan_returned_none: AtomicU64,
    pub index_scan_returned_some: AtomicU64,
    pub accounts_loaded: AtomicU64,
    pub initial_candidates_count: AtomicU64,
    pub purged_zero_lamports: AtomicU64,
    pub accounts_not_found_in_index: AtomicU64,
    pub num_ancient_slots_shrunk: AtomicU64,
    pub ancient_slots_added_to_shrink: AtomicU64,
    pub ancient_bytes_added_to_shrink: AtomicU64,
}

impl std::fmt::Debug for ShrinkStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut output = String::from("ShrinkStatus {\n");
        output.push_str(&format!("    last_report: {:?}\n", self.last_report));
        output.push_str(&format!(
            "    num_slots_shrunk: {:?}\n",
            self.num_slots_shrunk
        ));
        output.push_str(&format!(
            "    storage_read_elapsed: {:?}\n",
            self.storage_read_elapsed
        ));
        output.push_str(&format!(
            "    num_duplicated_accounts: {:?}\n",
            self.num_duplicated_accounts
        ));
        output.push_str(&format!(
            "    index_read_elapsed: {:?}\n",
            self.index_read_elapsed
        ));
        output.push_str(&format!(
            "    create_and_insert_store_elapsed: {:?}\n",
            self.create_and_insert_store_elapsed
        ));
        output.push_str(&format!(
            "    store_accounts_elapsed: {:?}\n",
            self.store_accounts_elapsed
        ));
        output.push_str(&format!(
            "    update_index_elapsed: {:?}\n",
            self.update_index_elapsed
        ));
        output.push_str(&format!(
            "    handle_reclaims_elapsed: {:?}\n",
            self.handle_reclaims_elapsed
        ));
        output.push_str(&format!(
            "    remove_old_stores_shrink_us: {:?}\n",
            self.remove_old_stores_shrink_us
        ));
        output.push_str(&format!(
            "    rewrite_elapsed: {:?}\n",
            self.rewrite_elapsed
        ));
        output.push_str(&format!(
            "    unpackable_slots_count: {:?}\n",
            self.unpackable_slots_count
        ));
        output.push_str(&format!(
            "    newest_alive_packed_count: {:?}\n",
            self.newest_alive_packed_count
        ));
        output.push_str(&format!(
            "    drop_storage_entries_elapsed: {:?}\n",
            self.drop_storage_entries_elapsed
        ));
        output.push_str(&format!(
            "    accounts_removed: {:?}\n",
            self.accounts_removed
        ));
        output.push_str(&format!("    bytes_removed: {:?}\n", self.bytes_removed));
        output.push_str(&format!("    bytes_written: {:?}\n", self.bytes_written));
        output.push_str(&format!("    skipped_shrink: {:?}\n", self.skipped_shrink));
        output.push_str(&format!("    dead_accounts: {:?}\n", self.dead_accounts));
        output.push_str(&format!("    alive_accounts: {:?}\n", self.alive_accounts));
        output.push_str(&format!(
            "    index_scan_returned_none: {:?}\n",
            self.index_scan_returned_none
        ));
        output.push_str(&format!(
            "    index_scan_returned_some: {:?}\n",
            self.index_scan_returned_some
        ));
        output.push_str(&format!(
            "    accounts_loaded: {:?}\n",
            self.accounts_loaded
        ));
        output.push_str(&format!(
            "    initial_candidates_count: {:?}\n",
            self.initial_candidates_count
        ));
        output.push_str(&format!(
            "    purged_zero_lamports: {:?}\n",
            self.purged_zero_lamports
        ));
        output.push_str(&format!(
            "    accounts_not_found_in_index: {:?}\n",
            self.accounts_not_found_in_index
        ));
        output.push_str(&format!(
            "    num_ancient_slots_shrunk: {:?}\n",
            self.num_ancient_slots_shrunk
        ));
        output.push_str(&format!(
            "    ancient_slots_added_to_shrink: {:?}\n",
            self.ancient_slots_added_to_shrink
        ));
        output.push_str(&format!(
            "    ancient_bytes_added_to_shrink: {:?}\n",
            self.ancient_bytes_added_to_shrink
        ));
        write!(f, "{}}}", output)
    }
}

impl ShrinkStats {
    pub fn report(&self) {
        if self.last_report.should_update(1000) {
            datapoint_info!(
                "shrink_stats",
                (
                    "ancient_slots_added_to_shrink",
                    self.ancient_slots_added_to_shrink
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "ancient_bytes_added_to_shrink",
                    self.ancient_bytes_added_to_shrink
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "num_slots_shrunk",
                    self.num_slots_shrunk.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "index_scan_returned_none",
                    self.index_scan_returned_none.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "index_scan_returned_some",
                    self.index_scan_returned_some.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "storage_read_elapsed",
                    self.storage_read_elapsed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "num_duplicated_accounts",
                    self.num_duplicated_accounts.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "index_read_elapsed",
                    self.index_read_elapsed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "create_and_insert_store_elapsed",
                    self.create_and_insert_store_elapsed
                        .swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "store_accounts_elapsed",
                    self.store_accounts_elapsed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "update_index_elapsed",
                    self.update_index_elapsed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "handle_reclaims_elapsed",
                    self.handle_reclaims_elapsed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "remove_old_stores_shrink_us",
                    self.remove_old_stores_shrink_us.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "rewrite_elapsed",
                    self.rewrite_elapsed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "drop_storage_entries_elapsed",
                    self.drop_storage_entries_elapsed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "accounts_removed",
                    self.accounts_removed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "bytes_removed",
                    self.bytes_removed.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "bytes_written",
                    self.bytes_written.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "skipped_shrink",
                    self.skipped_shrink.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "alive_accounts",
                    self.alive_accounts.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "dead_accounts",
                    self.dead_accounts.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "accounts_loaded",
                    self.accounts_loaded.swap(0, Ordering::Relaxed) as i64,
                    i64
                ),
                (
                    "purged_zero_lamports_count",
                    self.purged_zero_lamports.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "num_ancient_slots_shrunk",
                    self.num_ancient_slots_shrunk.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "accounts_not_found_in_index",
                    self.accounts_not_found_in_index.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "initial_candidates_count",
                    self.initial_candidates_count.swap(0, Ordering::Relaxed),
                    i64
                ),
            );
        }
    }
}

impl ShrinkAncientStats {
    pub fn report(&self) {
        datapoint_info!(
            "shrink_ancient_stats",
            (
                "num_slots_shrunk",
                self.shrink_stats
                    .num_slots_shrunk
                    .swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "index_scan_returned_none",
                self.shrink_stats
                    .index_scan_returned_none
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "index_scan_returned_some",
                self.shrink_stats
                    .index_scan_returned_some
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "storage_read_elapsed",
                self.shrink_stats
                    .storage_read_elapsed
                    .swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "num_duplicated_accounts",
                self.shrink_stats
                    .num_duplicated_accounts
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "index_read_elapsed",
                self.shrink_stats
                    .index_read_elapsed
                    .swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "create_and_insert_store_elapsed",
                self.shrink_stats
                    .create_and_insert_store_elapsed
                    .swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "store_accounts_elapsed",
                self.shrink_stats
                    .store_accounts_elapsed
                    .swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "update_index_elapsed",
                self.shrink_stats
                    .update_index_elapsed
                    .swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "handle_reclaims_elapsed",
                self.shrink_stats
                    .handle_reclaims_elapsed
                    .swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "remove_old_stores_shrink_us",
                self.shrink_stats
                    .remove_old_stores_shrink_us
                    .swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "rewrite_elapsed",
                self.shrink_stats.rewrite_elapsed.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "unpackable_slots_count",
                self.shrink_stats
                    .unpackable_slots_count
                    .swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "newest_alive_packed_count",
                self.shrink_stats
                    .newest_alive_packed_count
                    .swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "drop_storage_entries_elapsed",
                self.shrink_stats
                    .drop_storage_entries_elapsed
                    .swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "accounts_removed",
                self.shrink_stats
                    .accounts_removed
                    .swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "bytes_removed",
                self.shrink_stats.bytes_removed.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "bytes_written",
                self.shrink_stats.bytes_written.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "alive_accounts",
                self.shrink_stats.alive_accounts.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "dead_accounts",
                self.shrink_stats.dead_accounts.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "accounts_loaded",
                self.shrink_stats.accounts_loaded.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "ancient_append_vecs_shrunk",
                self.ancient_append_vecs_shrunk.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "random",
                self.random_shrink.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "slots_eligible_to_shrink",
                self.slots_eligible_to_shrink.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "total_dead_bytes",
                self.total_dead_bytes.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "total_alive_bytes",
                self.total_alive_bytes.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "slots_considered",
                self.slots_considered.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "ancient_scanned",
                self.ancient_scanned.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "total_us",
                self.total_us.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "bytes_ancient_created",
                self.bytes_ancient_created.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "bytes_from_must_shrink",
                self.bytes_from_must_shrink.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "bytes_from_smallest_storages",
                self.bytes_from_smallest_storages.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "bytes_from_newest_storages",
                self.bytes_from_newest_storages.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "many_ref_slots_skipped",
                self.many_ref_slots_skipped.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "slots_cannot_move_count",
                self.slots_cannot_move_count.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "many_refs_old_alive",
                self.many_refs_old_alive.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "purged_zero_lamports_count",
                self.shrink_stats
                    .purged_zero_lamports
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "accounts_not_found_in_index",
                self.shrink_stats
                    .accounts_not_found_in_index
                    .swap(0, Ordering::Relaxed),
                i64
            ),
        );
    }
}
