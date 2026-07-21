use {
    solana_time_utils::AtomicInterval,
    std::{
        iter::Sum,
        num::Saturating,
        sync::atomic::{AtomicU64, AtomicUsize, Ordering},
        time::Duration,
    },
};

#[derive(Debug, Default)]
pub struct AccountsStats {
    pub last_store_report: AtomicInterval,
    pub stakes_cache_check_and_store_us: AtomicU64,
    pub create_store_count: AtomicU64,
    pub dropped_stores: AtomicU64,
    pub handle_dead_keys_us: AtomicU64,
    pub purge_exact_us: AtomicU64,
    pub purge_exact_count: AtomicU64,
}

/// Stats from storing accounts unfrozen (i.e. to the write cache)
#[derive(Debug, Default)]
pub struct StoreAccountsUnfrozenStats {
    pub last_report: AtomicInterval,
    /// time spent writing accounts to the write cache
    pub write_to_cache_us: AtomicU64,
    /// time spent updating the secondary indexes
    pub update_secondary_index_us: AtomicU64,
    /// initial number of accounts to be stored
    pub num_initial_accounts_to_store: AtomicU64,
    /// number of accounts actually stored
    pub num_accounts_stored: AtomicU64,
    /// number of accounts *not* stored because they were duplicates
    pub num_duplicate_accounts_skipped: AtomicU64,
    /// number of accounts *not* stored because they were ephemeral
    pub num_ephemeral_accounts_skipped: AtomicU64,
    /// number of accounts *not* stored because they had an ancestor with zero lamports
    pub num_ancestors_zero_lamport_skipped: AtomicU64,
    /// number of bytes stored, from only account data
    pub account_data_bytes_stored: AtomicU64,
}

impl StoreAccountsUnfrozenStats {
    const REPORT_INTERVAL_MS: u64 = Duration::from_secs(1).as_millis() as u64;

    pub fn report(&self) {
        let should_report = self.last_report.should_update(Self::REPORT_INTERVAL_MS);
        if !should_report {
            return;
        }

        datapoint_info!(
            "accounts_db_store_accounts_unfrozen",
            (
                "write_to_cache_us",
                self.write_to_cache_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "update_secondary_index_us",
                self.update_secondary_index_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "num_initial_accounts_to_store",
                self.num_initial_accounts_to_store
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "num_accounts_stored",
                self.num_accounts_stored.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "num_duplicate_accounts_skipped",
                self.num_duplicate_accounts_skipped
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "num_ephemeral_accounts_skipped",
                self.num_ephemeral_accounts_skipped
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "num_ancestors_zero_lamport_skipped",
                self.num_ancestors_zero_lamport_skipped
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "account_data_bytes_stored",
                self.account_data_bytes_stored.swap(0, Ordering::Relaxed),
                i64
            ),
        );
    }
}

/// Stats from storing accounts for shrink (i.e. moving accounts into new storage file)
#[derive(Debug, Default)]
pub struct StoreAccountsForShrinkStats {
    pub write_accounts_us: u64,
    pub update_index_us: u64,
    pub num_accounts_stored: u64,
}

impl StoreAccountsForShrinkStats {
    pub fn accumulate(&mut self, other: &Self) {
        self.write_accounts_us += other.write_accounts_us;
        self.update_index_us += other.update_index_us;
        self.num_accounts_stored += other.num_accounts_stored;
    }
}

/// Stats from storing accounts for squash (i.e. moving accounts between storage files)
#[derive(Debug, Default)]
pub struct StoreAccountsForSquashStats {
    pub store_accounts_for_shrink_stats: StoreAccountsForShrinkStats,
    pub flush_read_cache_us: u64,
}

impl StoreAccountsForSquashStats {
    pub fn accumulate(&mut self, other: &Self) {
        self.store_accounts_for_shrink_stats
            .accumulate(&other.store_accounts_for_shrink_stats);
        self.flush_read_cache_us += other.flush_read_cache_us;
    }
}

/// Stats from storing accounts for flush (i.e. flushing the write cache to a storage file)
#[derive(Debug, Default)]
pub struct StoreAccountsForFlushStats {
    pub write_accounts_us: u64,
    pub update_index_us: u64,
    pub handle_reclaims_us: u64,
    pub mark_zero_lamport_single_ref_accounts_us: u64,
    pub num_zero_lamport_single_ref_accounts_marked: u64,
    pub num_reclaims: u64,
    pub num_obsolete_slots_removed: u64,
    pub num_obsolete_bytes_removed: u64,
}

#[derive(Debug, Default)]
pub struct PurgeStats {
    pub last_report: AtomicInterval,
    pub safety_checks_elapsed: AtomicU64,
    pub remove_cache_elapsed: AtomicU64,
    pub remove_storage_entries_elapsed: AtomicU64,
    pub drop_storage_entries_elapsed: AtomicU64,
    pub num_cached_slots_removed: AtomicUsize,
    pub num_stored_slots_removed: AtomicUsize,
    pub total_removed_cached_bytes: AtomicU64,
    pub total_removed_stored_bytes: AtomicU64,
    pub scan_storages_elapsed: AtomicU64,
    pub purge_accounts_index_elapsed: AtomicU64,
    pub handle_reclaims_elapsed: AtomicU64,
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
                    self.safety_checks_elapsed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "remove_cache_elapsed",
                    self.remove_cache_elapsed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "remove_storage_entries_elapsed",
                    self.remove_storage_entries_elapsed
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "drop_storage_entries_elapsed",
                    self.drop_storage_entries_elapsed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "num_cached_slots_removed",
                    self.num_cached_slots_removed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "num_stored_slots_removed",
                    self.num_stored_slots_removed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "total_removed_cached_bytes",
                    self.total_removed_cached_bytes.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "total_removed_stored_bytes",
                    self.total_removed_stored_bytes.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "scan_storages_elapsed",
                    self.scan_storages_elapsed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "purge_accounts_index_elapsed",
                    self.purge_accounts_index_elapsed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "handle_reclaims_elapsed",
                    self.handle_reclaims_elapsed.swap(0, Ordering::Relaxed),
                    i64
                ),
            );
        }
    }
}

#[derive(Debug, Default)]
pub struct FlushStats {
    pub num_accounts_stored: Saturating<usize>,
    pub num_bytes_stored: Saturating<u64>,
    pub num_accounts_skipped: Saturating<usize>,
    pub num_bytes_skipped: Saturating<u64>,
    pub num_zero_lamport_accounts_skipped: Saturating<usize>,
    pub store_accounts_total_us: Saturating<u64>,
    pub write_accounts_us: Saturating<u64>,
    pub update_index_us: Saturating<u64>,
    pub handle_reclaims_us: Saturating<u64>,
    pub mark_zero_lamport_single_ref_accounts_us: Saturating<u64>,
    pub num_zero_lamport_single_ref_accounts_marked: Saturating<u64>,
    pub num_reclaims: Saturating<u64>,
    pub num_obsolete_slots_removed: Saturating<u64>,
    pub num_obsolete_bytes_removed: Saturating<u64>,
    pub select_pubkeys_us: Saturating<u64>,
    pub disk_index_write_through_us: Saturating<u64>,
}

impl FlushStats {
    pub fn accumulate_store_accounts_for_flush(
        &mut self,
        store_accounts_stats: StoreAccountsForFlushStats,
    ) {
        self.write_accounts_us += Saturating(store_accounts_stats.write_accounts_us);
        self.update_index_us += Saturating(store_accounts_stats.update_index_us);
        self.handle_reclaims_us += Saturating(store_accounts_stats.handle_reclaims_us);
        self.mark_zero_lamport_single_ref_accounts_us +=
            Saturating(store_accounts_stats.mark_zero_lamport_single_ref_accounts_us);
        self.num_zero_lamport_single_ref_accounts_marked +=
            Saturating(store_accounts_stats.num_zero_lamport_single_ref_accounts_marked);
        self.num_reclaims += Saturating(store_accounts_stats.num_reclaims);
        self.num_obsolete_slots_removed +=
            Saturating(store_accounts_stats.num_obsolete_slots_removed);
        self.num_obsolete_bytes_removed +=
            Saturating(store_accounts_stats.num_obsolete_bytes_removed);
    }

    pub fn accumulate(&mut self, other: &Self) {
        self.num_accounts_stored += other.num_accounts_stored;
        self.num_bytes_stored += other.num_bytes_stored;
        self.num_accounts_skipped += other.num_accounts_skipped;
        self.num_bytes_skipped += other.num_bytes_skipped;
        self.num_zero_lamport_accounts_skipped += other.num_zero_lamport_accounts_skipped;
        self.store_accounts_total_us += other.store_accounts_total_us;
        self.write_accounts_us += other.write_accounts_us;
        self.update_index_us += other.update_index_us;
        self.handle_reclaims_us += other.handle_reclaims_us;
        self.mark_zero_lamport_single_ref_accounts_us +=
            other.mark_zero_lamport_single_ref_accounts_us;
        self.num_zero_lamport_single_ref_accounts_marked +=
            other.num_zero_lamport_single_ref_accounts_marked;
        self.num_reclaims += other.num_reclaims;
        self.num_obsolete_slots_removed += other.num_obsolete_slots_removed;
        self.num_obsolete_bytes_removed += other.num_obsolete_bytes_removed;
        self.select_pubkeys_us += other.select_pubkeys_us;
        self.disk_index_write_through_us += other.disk_index_write_through_us;
    }
}

#[derive(Debug, Default)]
pub struct CleanAccountsStats {
    pub purge_stats: PurgeStats,
    pub clean_unref_from_storage_us: AtomicU64,

    // stats held here and reported by clean_accounts
    pub clean_old_root_us: AtomicU64,
    pub clean_old_root_reclaim_us: AtomicU64,
    pub remove_dead_accounts_remove_us: AtomicU64,
    pub remove_dead_accounts_shrink_us: AtomicU64,
    pub clean_stored_dead_slots_us: AtomicU64,
    pub get_account_sizes_us: AtomicU64,
    pub slots_cleaned: AtomicU64,
}

impl CleanAccountsStats {
    pub fn report(&self) {
        self.purge_stats.report("clean_purge_slots_stats", None);
    }
}

#[derive(Debug, Default)]
pub struct ShrinkAncientStats {
    pub shrink_stats: ShrinkStats,
    pub ancient_append_vecs_shrunk: AtomicU64,
    pub total_us: AtomicU64,
    pub select_slots_us: AtomicU64,
    pub random_shrink: AtomicU64,
    pub slots_considered: AtomicU64,
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
    pub slot: AtomicU64,
    pub ideal_storage_size: AtomicU64,
    pub flush_read_cache_us: AtomicU64,
}

#[derive(Debug, Default)]
pub struct ShrinkStatsSub {
    pub store_accounts_stats: StoreAccountsForShrinkStats,
    pub rewrite_elapsed_us: Saturating<u64>,
    pub create_and_insert_store_elapsed_us: Saturating<u64>,
    pub tombstone_carry_forward_us: Saturating<u64>,
    pub num_tombstones_carried_forward: Saturating<u64>,
}

#[derive(Debug, Default)]
pub struct SquashStatsSub {
    pub store_accounts_stats: StoreAccountsForSquashStats,
    pub rewrite_elapsed_us: Saturating<u64>,
    pub create_and_insert_store_elapsed_us: Saturating<u64>,
    pub unpackable_slots_count: Saturating<usize>,
    pub newest_alive_packed_count: Saturating<usize>,
}

impl SquashStatsSub {
    pub fn accumulate(&mut self, other: &Self) {
        self.store_accounts_stats
            .accumulate(&other.store_accounts_stats);
        self.rewrite_elapsed_us += other.rewrite_elapsed_us;
        self.create_and_insert_store_elapsed_us += other.create_and_insert_store_elapsed_us;
        self.unpackable_slots_count += other.unpackable_slots_count;
        self.newest_alive_packed_count += other.newest_alive_packed_count;
    }
}

#[derive(Debug, Default)]
pub struct ShrinkStats {
    pub last_report: AtomicInterval,
    pub num_slots_shrunk: AtomicUsize,
    pub storage_read_elapsed: AtomicU64,
    pub index_read_elapsed: AtomicU64,
    pub create_and_insert_store_elapsed: AtomicU64,
    pub write_accounts_us: AtomicU64,
    pub mark_zero_lamport_single_ref_accounts_us: AtomicU64,
    pub update_index_us: AtomicU64,
    pub num_accounts_stored: AtomicU64,
    pub num_zero_lamport_single_ref_accounts_marked: AtomicU64,
    pub remove_old_stores_shrink_us: AtomicU64,
    pub rewrite_elapsed: AtomicU64,
    pub tombstone_carry_forward_us: AtomicU64,
    /// number of zero-lamport accounts carried forward to the new storage as tombstones
    pub num_tombstones_carried_forward: AtomicU64,
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
    pub obsolete_accounts_filtered: AtomicU64,
    pub accounts_loaded: AtomicU64,
    pub initial_candidates_count: AtomicU64,
    pub purged_zero_lamports: AtomicU64,
    pub num_ancient_slots_shrunk: AtomicU64,
    pub ancient_slots_added_to_shrink: AtomicU64,
    pub ancient_bytes_added_to_shrink: AtomicU64,
    pub num_dead_slots_added_to_clean: AtomicU64,
    pub num_slots_with_zero_lamport_accounts_added_to_shrink: AtomicU64,
    pub marking_zero_dead_accounts_in_non_shrinkable_store: AtomicU64,
    pub num_zero_lamport_single_ref_accounts_found: AtomicU64,
}

impl ShrinkStats {
    pub fn accumulate_sub_stats(&self, stats_sub: ShrinkStatsSub, increment_count: bool) {
        if increment_count {
            self.num_slots_shrunk.fetch_add(1, Ordering::Relaxed);
        }
        self.create_and_insert_store_elapsed.fetch_add(
            stats_sub.create_and_insert_store_elapsed_us.0,
            Ordering::Relaxed,
        );
        self.rewrite_elapsed
            .fetch_add(stats_sub.rewrite_elapsed_us.0, Ordering::Relaxed);
        self.tombstone_carry_forward_us
            .fetch_add(stats_sub.tombstone_carry_forward_us.0, Ordering::Relaxed);
        self.num_tombstones_carried_forward.fetch_add(
            stats_sub.num_tombstones_carried_forward.0,
            Ordering::Relaxed,
        );
        self.accumulate_store_accounts_for_shrink_stats(stats_sub.store_accounts_stats);
    }

    pub fn accumulate_store_accounts_for_shrink_stats(
        &self,
        store_accounts_stats: StoreAccountsForShrinkStats,
    ) {
        self.write_accounts_us
            .fetch_add(store_accounts_stats.write_accounts_us, Ordering::Relaxed);
        self.update_index_us
            .fetch_add(store_accounts_stats.update_index_us, Ordering::Relaxed);
        self.num_accounts_stored
            .fetch_add(store_accounts_stats.num_accounts_stored, Ordering::Relaxed);
    }

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
                    self.num_slots_shrunk.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "obsolete_accounts_filtered",
                    self.obsolete_accounts_filtered.swap(0, Ordering::Relaxed),
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
                    self.storage_read_elapsed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "index_read_elapsed",
                    self.index_read_elapsed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "create_and_insert_store_elapsed",
                    self.create_and_insert_store_elapsed
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "write_accounts_us",
                    self.write_accounts_us.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "mark_zero_lamport_single_ref_accounts_us",
                    self.mark_zero_lamport_single_ref_accounts_us
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "update_index_us",
                    self.update_index_us.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "num_accounts_stored",
                    self.num_accounts_stored.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "num_zero_lamport_single_ref_accounts_marked",
                    self.num_zero_lamport_single_ref_accounts_marked
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "remove_old_stores_shrink_us",
                    self.remove_old_stores_shrink_us.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "rewrite_elapsed",
                    self.rewrite_elapsed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "tombstone_carry_forward_us",
                    self.tombstone_carry_forward_us.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "num_tombstones_carried_forward",
                    self.num_tombstones_carried_forward
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "drop_storage_entries_elapsed",
                    self.drop_storage_entries_elapsed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "accounts_removed",
                    self.accounts_removed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "bytes_removed",
                    self.bytes_removed.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "bytes_written",
                    self.bytes_written.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "skipped_shrink",
                    self.skipped_shrink.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "alive_accounts",
                    self.alive_accounts.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "dead_accounts",
                    self.dead_accounts.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "accounts_loaded",
                    self.accounts_loaded.swap(0, Ordering::Relaxed),
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
                    "initial_candidates_count",
                    self.initial_candidates_count.swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "num_dead_slots_added_to_clean",
                    self.num_dead_slots_added_to_clean
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "num_slots_with_zero_lamport_accounts_added_to_shrink",
                    self.num_slots_with_zero_lamport_accounts_added_to_shrink
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "marking_zero_dead_accounts_in_non_shrinkable_store",
                    self.marking_zero_dead_accounts_in_non_shrinkable_store
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
                (
                    "num_zero_lamport_single_ref_accounts_found",
                    self.num_zero_lamport_single_ref_accounts_found
                        .swap(0, Ordering::Relaxed),
                    i64
                ),
            );
        }
    }
}

impl ShrinkAncientStats {
    pub fn accumulate_sub_stats(&self, stats_sub: SquashStatsSub) {
        let shrink_stats = &self.shrink_stats;
        shrink_stats.create_and_insert_store_elapsed.fetch_add(
            stats_sub.create_and_insert_store_elapsed_us.0,
            Ordering::Relaxed,
        );
        shrink_stats
            .rewrite_elapsed
            .fetch_add(stats_sub.rewrite_elapsed_us.0, Ordering::Relaxed);
        shrink_stats
            .unpackable_slots_count
            .fetch_add(stats_sub.unpackable_slots_count.0 as u64, Ordering::Relaxed);
        shrink_stats.newest_alive_packed_count.fetch_add(
            stats_sub.newest_alive_packed_count.0 as u64,
            Ordering::Relaxed,
        );
        let StoreAccountsForSquashStats {
            store_accounts_for_shrink_stats,
            flush_read_cache_us,
        } = stats_sub.store_accounts_stats;
        self.flush_read_cache_us
            .fetch_add(flush_read_cache_us, Ordering::Relaxed);
        shrink_stats.accumulate_store_accounts_for_shrink_stats(store_accounts_for_shrink_stats);
    }

    pub fn report(&self) {
        datapoint_info!(
            "shrink_ancient_stats",
            (
                "num_slots_shrunk",
                self.shrink_stats
                    .num_slots_shrunk
                    .swap(0, Ordering::Relaxed),
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
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "index_read_elapsed",
                self.shrink_stats
                    .index_read_elapsed
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "create_and_insert_store_elapsed",
                self.shrink_stats
                    .create_and_insert_store_elapsed
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "flush_read_cache_us",
                self.flush_read_cache_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "write_accounts_us",
                self.shrink_stats
                    .write_accounts_us
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "mark_zero_lamport_single_ref_accounts_us",
                self.shrink_stats
                    .mark_zero_lamport_single_ref_accounts_us
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "update_index_us",
                self.shrink_stats.update_index_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "num_accounts_stored",
                self.shrink_stats
                    .num_accounts_stored
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "num_zero_lamport_single_ref_accounts_marked",
                self.shrink_stats
                    .num_zero_lamport_single_ref_accounts_marked
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "remove_old_stores_shrink_us",
                self.shrink_stats
                    .remove_old_stores_shrink_us
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "rewrite_elapsed",
                self.shrink_stats.rewrite_elapsed.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "unpackable_slots_count",
                self.shrink_stats
                    .unpackable_slots_count
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "newest_alive_packed_count",
                self.shrink_stats
                    .newest_alive_packed_count
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "drop_storage_entries_elapsed",
                self.shrink_stats
                    .drop_storage_entries_elapsed
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "accounts_removed",
                self.shrink_stats
                    .accounts_removed
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "bytes_removed",
                self.shrink_stats.bytes_removed.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "bytes_written",
                self.shrink_stats.bytes_written.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "alive_accounts",
                self.shrink_stats.alive_accounts.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "dead_accounts",
                self.shrink_stats.dead_accounts.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "accounts_loaded",
                self.shrink_stats.accounts_loaded.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "ancient_append_vecs_shrunk",
                self.ancient_append_vecs_shrunk.swap(0, Ordering::Relaxed),
                i64
            ),
            ("random", self.random_shrink.swap(0, Ordering::Relaxed), i64),
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
                self.slots_considered.swap(0, Ordering::Relaxed),
                i64
            ),
            ("total_us", self.total_us.swap(0, Ordering::Relaxed), i64),
            (
                "select_slots_us",
                self.select_slots_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "bytes_ancient_created",
                self.bytes_ancient_created.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "bytes_from_must_shrink",
                self.bytes_from_must_shrink.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "bytes_from_smallest_storages",
                self.bytes_from_smallest_storages.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "bytes_from_newest_storages",
                self.bytes_from_newest_storages.swap(0, Ordering::Relaxed),
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
            ("slot", self.slot.load(Ordering::Relaxed), i64),
            (
                "ideal_storage_size",
                self.ideal_storage_size.swap(0, Ordering::Relaxed),
                i64
            ),
        );
    }
}

#[derive(Debug, Default)]
pub struct ObsoleteAccountsStats {
    pub accounts_marked_obsolete: u64,
    pub slots_removed: u64,
}

impl Sum<Self> for ObsoleteAccountsStats {
    fn sum<I>(iter: I) -> Self
    where
        I: Iterator<Item = Self>,
    {
        iter.fold(Self::default(), |mut accumulated_stats, item| {
            accumulated_stats.accounts_marked_obsolete += item.accounts_marked_obsolete;
            accumulated_stats.slots_removed += item.slots_removed;
            accumulated_stats
        })
    }
}

/// Stats from calling write_accounts_to_cache()
///
/// Refer to StoreAccountsUnfrozenStats for docs on each field.
#[derive(Debug, Default)]
pub struct WriteAccountsToCacheStats {
    pub num_initial_accounts_to_store: u64,
    pub num_accounts_stored: u64,
    pub num_duplicate_accounts_skipped: u64,
    pub num_ephemeral_accounts_skipped: u64,
    pub num_ancestors_zero_lamport_skipped: u64,
    pub account_data_bytes_stored: u64,
}

#[derive(Debug, Default)]
pub struct LoadAccountsStats {
    pub num_loaded_from_write_cache: AtomicU64,
    pub num_loaded_from_read_cache: AtomicU64,
    pub num_loaded_from_index_storage: AtomicU64,
}

impl LoadAccountsStats {
    pub fn report(&self) {
        datapoint_info!(
            "accounts_db_load_accounts",
            (
                "num_loaded_from_write_cache",
                self.num_loaded_from_write_cache.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "num_loaded_from_read_cache",
                self.num_loaded_from_read_cache.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "num_loaded_from_index_storage",
                self.num_loaded_from_index_storage
                    .swap(0, Ordering::Relaxed),
                i64
            ),
        );
    }
}
