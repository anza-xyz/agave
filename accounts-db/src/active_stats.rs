//! keep track of areas of the validator that are currently active
use std::sync::atomic::{AtomicUsize, Ordering};

/// counters of different areas of a validator which could be active
#[derive(Debug, Default)]
pub struct ActiveStats {
    clean: AtomicUsize,
    clean_collect_candidates: AtomicUsize,
    clean_sort_candidates: AtomicUsize,
    clean_scan_candidates: AtomicUsize,
    clean_old_accounts: AtomicUsize,
    clean_collect_store_counts: AtomicUsize,
    clean_calc_delete_deps: AtomicUsize,
    clean_filter_zero_lamport: AtomicUsize,
    clean_collect_reclaims: AtomicUsize,
    clean_purge_reclaims: AtomicUsize,
    clean_handle_reclaims: AtomicUsize,
    squash_ancient: AtomicUsize,
    shrink: AtomicUsize,
    hash: AtomicUsize,
    flush: AtomicUsize,
    hash_scan: AtomicUsize,
    hash_dedup: AtomicUsize,
    hash_merkle: AtomicUsize,
}

#[derive(Debug, Copy, Clone)]
pub enum ActiveStatItem {
    Clean,
    CleanCollectCandidates,
    CleanSortCandidates,
    CleanScanCandidates,
    CleanOldAccounts,
    CleanCollectStoreCounts,
    CleanCalcDeleteDeps,
    CleanFilterZeroLamport,
    CleanCollectReclaims,
    CleanPurgeReclaims,
    CleanHandleReclaims,
    Shrink,
    SquashAncient,
    Hash,
    Flush,
    HashScan,
    HashDeDup,
    HashMerkleTree,
}

/// sole purpose is to handle 'drop' so that stat is decremented when self is dropped
pub struct ActiveStatGuard<'a> {
    stats: &'a ActiveStats,
    item: ActiveStatItem,
}

impl<'a> Drop for ActiveStatGuard<'a> {
    fn drop(&mut self) {
        self.stats.update_and_log(self.item, |stat| {
            stat.fetch_sub(1, Ordering::Relaxed).wrapping_sub(1)
        });
    }
}

impl ActiveStats {
    #[must_use]
    /// create a stack object to set the state to increment stat initially and decrement on drop
    pub fn activate(&self, stat: ActiveStatItem) -> ActiveStatGuard<'_> {
        self.update_and_log(stat, |stat| {
            stat.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
        });
        ActiveStatGuard {
            stats: self,
            item: stat,
        }
    }
    /// update and log the change to the specified 'item'
    fn update_and_log(&self, item: ActiveStatItem, modify_stat: impl Fn(&AtomicUsize) -> usize) {
        let stat = match item {
            ActiveStatItem::Clean => &self.clean,
            ActiveStatItem::CleanCollectCandidates => &self.clean_collect_candidates,
            ActiveStatItem::CleanSortCandidates => &self.clean_sort_candidates,
            ActiveStatItem::CleanScanCandidates => &self.clean_scan_candidates,
            ActiveStatItem::CleanOldAccounts => &self.clean_old_accounts,
            ActiveStatItem::CleanCollectStoreCounts => &self.clean_collect_store_counts,
            ActiveStatItem::CleanCalcDeleteDeps => &self.clean_calc_delete_deps,
            ActiveStatItem::CleanFilterZeroLamport => &self.clean_filter_zero_lamport,
            ActiveStatItem::CleanCollectReclaims => &self.clean_collect_reclaims,
            ActiveStatItem::CleanPurgeReclaims => &self.clean_purge_reclaims,
            ActiveStatItem::CleanHandleReclaims => &self.clean_handle_reclaims,
            ActiveStatItem::Shrink => &self.shrink,
            ActiveStatItem::SquashAncient => &self.squash_ancient,
            ActiveStatItem::Hash => &self.hash,
            ActiveStatItem::Flush => &self.flush,
            ActiveStatItem::HashDeDup => &self.hash_dedup,
            ActiveStatItem::HashMerkleTree => &self.hash_merkle,
            ActiveStatItem::HashScan => &self.hash_scan,
        };
        let value = modify_stat(stat);
        match item {
            ActiveStatItem::Clean => datapoint_info!("accounts_db_active", ("clean", value, i64)),
            ActiveStatItem::CleanCollectCandidates => datapoint_info!(
                "accounts_db_active",
                ("clean_collect_candidates", value, i64),
            ),
            ActiveStatItem::CleanSortCandidates => {
                datapoint_info!("accounts_db_active", ("clean_sort_candidates", value, i64))
            }
            ActiveStatItem::CleanScanCandidates => {
                datapoint_info!("accounts_db_active", ("clean_scan_candidates", value, i64))
            }
            ActiveStatItem::CleanOldAccounts => {
                datapoint_info!("accounts_db_active", ("clean_old_accounts", value, i64))
            }
            ActiveStatItem::CleanCollectStoreCounts => {
                datapoint_info!(
                    "accounts_db_active",
                    ("clean_collect_store_counts", value, i64),
                )
            }
            ActiveStatItem::CleanCalcDeleteDeps => {
                datapoint_info!("accounts_db_active", ("clean_calc_delete_deps", value, i64))
            }
            ActiveStatItem::CleanFilterZeroLamport => datapoint_info!(
                "accounts_db_active",
                ("clean_filter_zero_lamport", value, i64),
            ),
            ActiveStatItem::CleanCollectReclaims => {
                datapoint_info!("accounts_db_active", ("clean_collect_reclaims", value, i64))
            }
            ActiveStatItem::CleanPurgeReclaims => {
                datapoint_info!("accounts_db_active", ("clean_purge_reclaims", value, i64))
            }
            ActiveStatItem::CleanHandleReclaims => {
                datapoint_info!("accounts_db_active", ("clean_handle_reclaims", value, i64))
            }
            ActiveStatItem::SquashAncient => {
                datapoint_info!("accounts_db_active", ("squash_ancient", value, i64))
            }
            ActiveStatItem::Shrink => {
                datapoint_info!("accounts_db_active", ("shrink", value, i64))
            }
            ActiveStatItem::Hash => datapoint_info!("accounts_db_active", ("hash", value, i64)),
            ActiveStatItem::Flush => datapoint_info!("accounts_db_active", ("flush", value, i64)),
            ActiveStatItem::HashDeDup => {
                datapoint_info!("accounts_db_active", ("hash_dedup", value, i64))
            }
            ActiveStatItem::HashMerkleTree => {
                datapoint_info!("accounts_db_active", ("hash_merkle_tree", value, i64))
            }
            ActiveStatItem::HashScan => {
                datapoint_info!("accounts_db_active", ("hash_scan", value, i64))
            }
        };
    }
}
