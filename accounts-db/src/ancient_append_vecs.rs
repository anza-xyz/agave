//! helpers for squashing append vecs into ancient append vecs
//! an ancient append vec is:
//! 1. a slot that is older than an epoch old
//! 2. multiple 'slots' squashed into a single older (ie. ancient) slot for convenience and performance
//!
//! Otherwise, an ancient append vec is the same as any other append vec
use {
    crate::{
        account_storage::ShrinkInProgress,
        account_storage_entry::AccountStorageEntry,
        accounts_db::{
            AccountFromStorage, AccountsDb, AliveAccounts, GetUniqueAccountsResult, ShrinkCollect,
            stats::{ShrinkAncientStats, SquashStatsSub},
        },
        active_stats::ActiveStatItem,
        storable_accounts::{StorableAccounts, StorableAccountsBySlot},
        u64_align,
    },
    rayon::prelude::{IntoParallelRefIterator, ParallelIterator},
    solana_clock::Slot,
    solana_measure::measure_us,
    std::{
        collections::{HashMap, VecDeque},
        num::{NonZeroU64, Saturating},
        sync::{Arc, Mutex, atomic::Ordering},
    },
};

/// this many # of highest slot values should be treated as desirable to pack.
/// This gives us high slots to move packed accounts into.
const HIGH_SLOT_OFFSET: u64 = 100;

/// ancient packing algorithm tuning per pass
#[derive(Debug)]
struct PackedAncientStorageTuning {
    /// number of ancient slots we should aim to have. If we have more than this, combine further.
    max_ancient_slots: usize,
    /// # of bytes in an ideal ancient storage size
    ideal_storage_size: NonZeroU64,
    /// limit the max # of output storages to prevent packing from running too long
    max_resulting_storages: NonZeroU64,
}

/// info about a storage eligible to be combined into an ancient append vec.
/// Useful to help sort vecs of storages.
#[derive(Debug)]
struct SlotInfo {
    storage: Arc<AccountStorageEntry>,
    /// slot of storage
    slot: Slot,
    /// total bytes written in storage, before shrinking
    written_bytes: u64,
    /// # alive bytes in storage *after* shrinking
    alive_bytes: u64,
    /// this slot is a high slot #
    /// It is important to include some high slot #s so that we have new slots to try each time pack runs.
    is_high_slot: bool,
}

/// info for all storages in ancient slots
/// 'all_infos' contains all slots and storages that are ancient
#[derive(Default, Debug)]
struct AncientSlotInfos {
    /// info on all ancient storages
    all_infos: Vec<SlotInfo>,
    /// indexes to 'all_info' for storages that should be shrunk because alive ratio is too low.
    /// subset of all_infos
    shrink_indexes: Vec<usize>,
    /// total alive bytes across all slots
    total_alive_bytes: Saturating<u64>,
    /// slots that have dead accounts and thus the corresponding slot
    /// storages can be shrunk
    best_slots_to_shrink: VecDeque<(Slot, u64)>,
}

impl AncientSlotInfos {
    /// add info for 'storage'
    fn add(
        &mut self,
        slot: Slot,
        storage: Arc<AccountStorageEntry>,
        alive_bytes_after_shrink: u64,
        ideal_size: NonZeroU64,
        is_high_slot: bool,
        is_candidate_for_shrink: bool,
    ) {
        if alive_bytes_after_shrink > 0 {
            let written_bytes = storage.written_bytes();
            // two criteria we're shrinking by later:
            // 1. alive ratio so that we don't consume too much disk space with dead accounts
            // 2. # of active ancient roots, so that we don't consume too many open file handles

            if is_candidate_for_shrink {
                self.shrink_indexes.push(self.all_infos.len());
            } else {
                let already_ideal_size = u64::from(ideal_size) * 80 / 100;
                if alive_bytes_after_shrink > already_ideal_size {
                    // do not include this append vec at all. It is already ideal size and not a candidate for shrink.
                    return;
                }
            }
            self.all_infos.push(SlotInfo {
                slot,
                written_bytes,
                storage,
                alive_bytes: alive_bytes_after_shrink,
                is_high_slot,
            });
            self.total_alive_bytes += alive_bytes_after_shrink;
        }
    }

    /// modify 'self' to contain only the slot infos for the slots that should be combined
    /// (and in this process effectively shrunk)
    fn filter_ancient_slots(
        &mut self,
        tuning: &PackedAncientStorageTuning,
        stats: &ShrinkAncientStats,
    ) {
        // figure out which slots to combine
        self.choose_storages_to_shrink();
        // smallest files so we get the largest number of files to remove
        self.filter_by_smallest_capacity(tuning, stats);
    }

    // sort 'shrink_indexes' by most bytes saved, highest to lowest
    fn sort_shrink_indexes_by_bytes_saved(&mut self) {
        self.shrink_indexes.sort_unstable_by(|l, r| {
            let amount_shrunk = |index: &usize| {
                let item = &self.all_infos[*index];
                // alive_bytes assumes the accounts are aligned. `written_bytes` may
                // not be aligned for the last account. Therefore, we need to
                // align it.
                let aligned_written_bytes = u64_align!(item.written_bytes as usize) as u64;
                if aligned_written_bytes < item.alive_bytes {
                    // should not happen, but if it does, submit warn log it and continue
                    datapoint_warn!(
                        "aligned_written_bytes_less_than_alive_bytes",
                        ("aligned_written_bytes", aligned_written_bytes, i64),
                        ("alive_bytes", item.alive_bytes, i64)
                    );
                }
                item.written_bytes.saturating_sub(item.alive_bytes)
            };
            amount_shrunk(r).cmp(&amount_shrunk(l))
        });
    }

    /// record the storages to shrink in 'best_slots_to_shrink', most bytes saved to fewest
    fn choose_storages_to_shrink(&mut self) {
        // most bytes saved is more valuable to shrink
        self.sort_shrink_indexes_by_bytes_saved();

        self.best_slots_to_shrink = self
            .shrink_indexes
            .iter()
            .map(|info_index| {
                let info = &self.all_infos[*info_index];
                (info.slot, info.written_bytes)
            })
            .collect();
    }

    /// truncate 'all_infos' such that when the remaining entries in
    /// 'all_infos' are combined, the total number of storages <= 'max_storages'
    /// The idea is that 'all_infos' is sorted from smallest capacity to largest,
    /// but that isn't required for this function to be 'correct'.
    fn truncate_to_max_storages(
        &mut self,
        tuning: &PackedAncientStorageTuning,
        stats: &ShrinkAncientStats,
    ) {
        // these indexes into 'all_infos' are useless once we truncate 'all_infos', so make sure they're cleared out to avoid any issues
        self.shrink_indexes.clear();
        let total_storages = self.all_infos.len();
        let mut cumulative_bytes = Saturating(0u64);
        let low_threshold = tuning.max_ancient_slots * 50 / 100;
        let mut bytes_from_smallest_storages = 0;
        let mut bytes_from_newest_storages = 0;
        for (i, info) in self.all_infos.iter().enumerate() {
            cumulative_bytes += info.alive_bytes;
            let ancient_storages_required =
                cumulative_bytes.0.div_ceil(tuning.ideal_storage_size.get()) as usize;
            let storages_remaining = total_storages - i - 1;

            // if the remaining uncombined storages and the # of resulting
            // combined ancient storages are less than the threshold, then
            // we've gone too far, so get rid of this entry and all after it.
            // Every storage after this one is larger than the ones we've chosen.
            // if we ever get to more than `max_resulting_storages` required ancient storages, that is enough to stop for now.
            // It will take a lot of time for the pack algorithm to create that many, and that is bad for system performance.
            // This should be a limit that only affects extreme testing environments.
            // We do not stop including entries until we have dealt with all the high slot #s. This allows the algorithm to continue
            // to make progress each time it is called.
            if !info.is_high_slot
                && (storages_remaining + ancient_storages_required < low_threshold
                    || ancient_storages_required as u64 > u64::from(tuning.max_resulting_storages))
            {
                self.all_infos.truncate(i);
                break;
            }
            if info.is_high_slot {
                bytes_from_newest_storages += info.alive_bytes;
            } else {
                bytes_from_smallest_storages += info.alive_bytes;
            }
        }
        stats
            .bytes_from_smallest_storages
            .fetch_add(bytes_from_smallest_storages, Ordering::Relaxed);
        stats
            .bytes_from_newest_storages
            .fetch_add(bytes_from_newest_storages, Ordering::Relaxed);
    }

    /// remove entries from 'all_infos' such that combining
    /// the remaining entries into storages of 'ideal_storage_size'
    /// will get us below 'max_storages'
    /// The entries that are removed will be reconsidered the next time around.
    /// Combining too many storages costs i/o and cpu so the goal is to find the sweet spot so
    /// that we make progress in cleaning/shrinking/combining but that we don't cause unnecessary
    /// churn.
    fn filter_by_smallest_capacity(
        &mut self,
        tuning: &PackedAncientStorageTuning,
        stats: &ShrinkAncientStats,
    ) {
        let total_storages = self.all_infos.len();
        if total_storages <= tuning.max_ancient_slots {
            // currently fewer storages than max, so nothing to shrink
            self.shrink_indexes.clear();
            self.all_infos.clear();
            return;
        }

        // sort by:
        // 1. `high_slot`: we want to include new, high slots each time so that we try new slots
        //     each time alg runs and have several high target slots for packed storages.
        // 2. smallest capacity to largest so that we remove the most slots possible
        self.all_infos.sort_unstable_by(|l, r| {
            r.is_high_slot
                .cmp(&l.is_high_slot)
                .then_with(|| l.written_bytes.cmp(&r.written_bytes))
        });

        // remove any storages we don't need to combine this pass to achieve
        // # resulting storages <= 'max_storages'
        self.truncate_to_max_storages(tuning, stats);
    }
}

/// Used to hold the result of writing a single ancient storage
/// and results of writing multiple ancient storages
#[derive(Debug, Default)]
struct WriteAncientAccounts<'a> {
    /// 'ShrinkInProgress' instances created by starting a shrink operation
    shrinks_in_progress: HashMap<Slot, ShrinkInProgress<'a>>,

    metrics: SquashStatsSub,
}

impl AccountsDb {
    /// Combine account data from storages in 'sorted_slots' into packed storages.
    /// This keeps us from accumulating storages for each slot older than an epoch.
    /// After this function the number of alive roots is <= # alive roots when it was called.
    /// In practice, the # of alive roots after will be significantly less than # alive roots when called.
    /// Trying to reduce # roots and storages (one per root) required to store all the data in ancient slots
    pub(crate) fn combine_ancient_slots_packed(&self, sorted_slots: Vec<Slot>) {
        let tuning = PackedAncientStorageTuning {
            // Slots old enough to be ancient.
            max_ancient_slots: self.max_ancient_storages,
            ideal_storage_size: NonZeroU64::new(get_ancient_append_vec_capacity()).unwrap(),
            max_resulting_storages: NonZeroU64::new(10).unwrap(),
        };

        let _guard = self.active_stats.activate(ActiveStatItem::SquashAncient);

        let mut stats_sub = SquashStatsSub::default();

        let (_, total_us) = measure_us!(self.combine_ancient_slots_packed_internal(
            sorted_slots,
            tuning,
            &mut stats_sub
        ));

        self.shrink_ancient_stats.accumulate_sub_stats(stats_sub);
        self.shrink_ancient_stats
            .total_us
            .fetch_add(total_us, Ordering::Relaxed);

        self.shrink_ancient_stats.report();
    }

    fn combine_ancient_slots_packed_internal(
        &self,
        sorted_slots: Vec<Slot>,
        mut tuning: PackedAncientStorageTuning,
        metrics: &mut SquashStatsSub,
    ) {
        self.shrink_ancient_stats
            .slot
            .store(*sorted_slots.first().unwrap_or(&0), Ordering::Relaxed);
        self.shrink_ancient_stats
            .slots_considered
            .fetch_add(sorted_slots.len() as u64, Ordering::Relaxed);
        let mut ancient_slot_infos =
            self.collect_sort_filter_ancient_slots(sorted_slots, &mut tuning);
        self.shrink_ancient_stats
            .ideal_storage_size
            .store(tuning.ideal_storage_size.into(), Ordering::Relaxed);

        std::mem::swap(
            &mut *self.best_ancient_slots_to_shrink.write().unwrap(),
            &mut ancient_slot_infos.best_slots_to_shrink,
        );

        if ancient_slot_infos.all_infos.is_empty() {
            return; // nothing to do
        }
        let mut accounts_per_storage = self
            .get_unique_accounts_from_storage_for_combining_ancient_slots(
                &ancient_slot_infos.all_infos[..],
            );

        let mut accounts_to_combine = self.calc_accounts_to_combine(&mut accounts_per_storage);

        // Put the accounts from the LARGEST storages at the end.
        // This causes us to keep the accounts we're re-packing from already existing ancient storages together.
        // The alternative could cause us to mix newly ancient slots produced by flush (containing accounts touched more recently) with previously
        // packed ancient storages which over time contained enough dead accounts that the storage needed to be shrunk by being re-packed.
        // The end result of this sort should cause older, colder accounts (previously packed into large storages and then re-packed/shrunk) to
        // be re-packed together with other older/colder accounts.
        accounts_to_combine
            .accounts_to_combine
            .sort_unstable_by_key(|a| a.written_bytes);

        // Pack the accounts into storages of size 'ideal_storage_size'.
        let pack = PackedAncientStorage::pack(
            accounts_to_combine
                .accounts_to_combine
                .iter()
                .map(|shrink_collect| &shrink_collect.alive_accounts),
            tuning.ideal_storage_size,
        );

        if pack.len() > accounts_to_combine.target_slots_sorted.len() {
            // Not enough slots to contain the accounts we are trying to pack.
            return;
        }

        let write_ancient_accounts = self.write_packed_storages(&accounts_to_combine, pack);

        self.finish_combine_ancient_slots_packed_internal(
            accounts_to_combine,
            write_ancient_accounts,
            metrics,
        );
    }

    /// calculate all storage info for the storages in slots
    /// Then, apply 'tuning' to filter out slots we do NOT want to combine.
    fn collect_sort_filter_ancient_slots(
        &self,
        slots: Vec<Slot>,
        tuning: &mut PackedAncientStorageTuning,
    ) -> AncientSlotInfos {
        let mut ancient_slot_infos = self.calc_ancient_slot_info(slots, tuning);
        // ideal storage size is total alive bytes of ancient storages
        // divided by half of max ancient slots
        tuning.ideal_storage_size = NonZeroU64::new(
            (ancient_slot_infos.total_alive_bytes.0 * 2 / tuning.max_ancient_slots.max(1) as u64)
                .max(self.ancient_storage_ideal_size),
        )
        .unwrap();

        ancient_slot_infos.filter_ancient_slots(tuning, &self.shrink_ancient_stats);
        ancient_slot_infos
    }

    /// create append vec of size 'bytes'
    /// write 'accounts_to_write' into it
    /// return shrink_in_progress and some metrics
    fn write_ancient_accounts<'a, 'b: 'a>(
        &'b self,
        bytes: u64,
        accounts_to_write: impl StorableAccounts<'a>,
        write_ancient_accounts: &mut WriteAncientAccounts<'b>,
    ) {
        let target_slot = accounts_to_write.target_slot();
        let old_store = self
            .storage
            .get_slot_storage_entry_shrinking_in_progress_ok(target_slot)
            .expect("ancient shrink target slot must already have a storage");
        let (shrink_in_progress, create_and_insert_store_elapsed_us) =
            measure_us!(self.get_store_for_shrink(target_slot, old_store, bytes));
        let (store_accounts_stats, rewrite_elapsed_us) = measure_us!(
            self.store_accounts_for_squash(accounts_to_write, shrink_in_progress.new_storage())
        );

        // Count the bytes actually written into the packed storage
        self.shrink_ancient_stats
            .shrink_stats
            .bytes_written
            .fetch_add(
                shrink_in_progress.new_storage().written_bytes(),
                Ordering::Relaxed,
            );

        write_ancient_accounts.metrics.accumulate(&SquashStatsSub {
            store_accounts_stats,
            rewrite_elapsed_us: Saturating(rewrite_elapsed_us),
            create_and_insert_store_elapsed_us: Saturating(create_and_insert_store_elapsed_us),
        });

        write_ancient_accounts
            .shrinks_in_progress
            .insert(target_slot, shrink_in_progress);
    }
    /// go through all slots and populate 'SlotInfo', per slot
    /// This provides the list of possible ancient slots to sort, filter, and then combine.
    fn calc_ancient_slot_info(
        &self,
        slots: Vec<Slot>,
        tuning: &PackedAncientStorageTuning,
    ) -> AncientSlotInfos {
        let len = slots.len();
        let mut infos = AncientSlotInfos {
            shrink_indexes: Vec::with_capacity(len),
            all_infos: Vec::with_capacity(len),
            ..AncientSlotInfos::default()
        };
        let max_slot = slots.iter().max().cloned().unwrap_or_default();
        // heuristic to include some # of newly eligible ancient slots so that the pack algorithm always makes progress
        let high_slot_boundary = max_slot.saturating_sub(HIGH_SLOT_OFFSET);
        let is_high_slot = |slot| slot >= high_slot_boundary;
        for slot in &slots {
            if let Some(storage) = self.storage.get_slot_storage_entry(*slot) {
                let is_candidate_for_shrink = self.is_candidate_for_shrink(&storage);
                let alive_bytes_after_shrink = self.alive_bytes_after_shrink(&storage) as u64;
                infos.add(
                    *slot,
                    storage,
                    alive_bytes_after_shrink,
                    tuning.ideal_storage_size,
                    is_high_slot(*slot),
                    is_candidate_for_shrink,
                );
            }
        }
        let mut total_dead_bytes = 0;
        let mut total_alive_bytes = 0;
        infos.all_infos.iter().for_each(|info| {
            total_dead_bytes += info.written_bytes.saturating_sub(info.alive_bytes);
            total_alive_bytes += info.alive_bytes;
        });
        self.shrink_ancient_stats
            .total_dead_bytes
            .fetch_add(total_dead_bytes, Ordering::Relaxed);
        self.shrink_ancient_stats
            .total_alive_bytes
            .fetch_add(total_alive_bytes, Ordering::Relaxed);
        infos
    }

    /// write packed storages as described in 'accounts_to_combine'
    /// and 'packed_contents'
    fn write_packed_storages<'a, 'b>(
        &'a self,
        accounts_to_combine: &'b AccountsToCombine<'b>,
        packed_contents: Vec<PackedAncientStorage<'b>>,
    ) -> WriteAncientAccounts<'a> {
        let write_ancient_accounts = Mutex::new(WriteAncientAccounts::default());

        // ok if we have more slots, but NOT ok if we have fewer slots than we have contents
        assert!(accounts_to_combine.target_slots_sorted.len() >= packed_contents.len());
        // write packed storages containing contents from many original slots
        // iterate slots in highest to lowest
        let packer = accounts_to_combine
            .target_slots_sorted
            .iter()
            .rev()
            .zip(packed_contents)
            .collect::<Vec<_>>();

        // keep track of how many slots were shrunk away
        self.shrink_ancient_stats
            .ancient_append_vecs_shrunk
            .fetch_add(
                accounts_to_combine
                    .target_slots_sorted
                    .len()
                    .saturating_sub(packer.len()) as u64,
                Ordering::Relaxed,
            );

        self.thread_pool_background.install(|| {
            packer.par_iter().for_each(|(target_slot, pack)| {
                let mut write_ancient_accounts_local = WriteAncientAccounts::default();
                self.write_one_packed_storage(
                    pack,
                    **target_slot,
                    &mut write_ancient_accounts_local,
                );
                let mut write = write_ancient_accounts.lock().unwrap();
                write
                    .shrinks_in_progress
                    .extend(write_ancient_accounts_local.shrinks_in_progress);
                write
                    .metrics
                    .accumulate(&write_ancient_accounts_local.metrics);
            });
        });

        write_ancient_accounts.into_inner().unwrap()
    }

    /// for each slot in 'ancient_slots', collect all accounts in that slot
    /// return the collection of accounts by slot
    fn get_unique_accounts_from_storage_for_combining_ancient_slots<'a>(
        &self,
        ancient_slots: &'a [SlotInfo],
    ) -> Vec<(&'a SlotInfo, GetUniqueAccountsResult)> {
        let mut accounts_to_combine = Vec::with_capacity(ancient_slots.len());

        for info in ancient_slots {
            let unique_accounts = self.get_unique_accounts_from_storage_for_shrink(
                &info.storage,
                &self.shrink_ancient_stats.shrink_stats,
            );
            accounts_to_combine.push((info, unique_accounts));
        }

        accounts_to_combine
    }

    /// finish shrink operation on slots where a new storage was created
    /// drop root and storage for all original slots whose contents were combined into other storages
    fn finish_combine_ancient_slots_packed_internal(
        &self,
        accounts_to_combine: AccountsToCombine<'_>,
        mut write_ancient_accounts: WriteAncientAccounts,
        metrics: &mut SquashStatsSub,
    ) {
        let mut dropped_roots = Vec::with_capacity(accounts_to_combine.accounts_to_combine.len());
        for shrink_collect in accounts_to_combine.accounts_to_combine {
            let slot = shrink_collect.slot;

            // Ancient squash only runs on slots far older than the latest full snapshot, where
            // tombstones are purgeable and `shrink_collect` drops them rather than carrying them
            // forward. The squash write path has no tombstone handling, so a non-empty list here
            // would be silently lost; assert the invariant at the point that loss would occur.
            debug_assert!(
                shrink_collect.tombstones_to_carry_forward.is_empty(),
                "ancient squash reached a carry-forward tombstone at slot {slot}",
            );

            let shrink_in_progress = write_ancient_accounts.shrinks_in_progress.remove(&slot);

            let mut reopen = false;
            if shrink_in_progress.is_none() {
                dropped_roots.push(slot);
            } else {
                // Remember that we need to 'reopen' the storage for this
                // 'slot'. Note that it is not *safe* to reopen the storage for
                // the 'slot' here, because 'shrink_in_progress' is still alive.
                // Storage map may still point to the old storage and will be
                // updated to point to the new storage, after we drop
                // 'shrink_in_progress'.
                reopen = true;
            }

            self.remove_old_stores_shrink(
                slot,
                &self.shrink_ancient_stats.shrink_stats,
                shrink_in_progress,
                true,
            );

            // If the slot is dead, remove the need to shrink the storage as the storage entries will be purged.
            self.shrink_candidate_slots.lock().unwrap().remove(&slot);

            if reopen {
                // 'shrink_in_progress' is dead now. We can safely 'reopen' the new storage for 'slot'.
                self.reopen_storage_as_readonly_shrinking_in_progress_ok(slot);
            }
        }
        metrics.accumulate(&write_ancient_accounts.metrics);
    }

    /// given all accounts per ancient slot, in slots that we want to combine together: get a list
    /// of all the accounts that should be combined into new storages and the slots that should be
    /// used for the new storages.
    fn calc_accounts_to_combine<'a>(
        &self,
        accounts_per_storage: &'a mut [(&'a SlotInfo, GetUniqueAccountsResult)],
    ) -> AccountsToCombine<'a> {
        // `shrink_collect` all accounts in the storages we want to combine.
        let accounts_to_combine = accounts_per_storage
            .iter_mut()
            .map(|(info, unique_accounts)| {
                self.shrink_collect(
                    &info.storage,
                    unique_accounts,
                    &self.shrink_ancient_stats.shrink_stats,
                )
            })
            .collect::<Vec<_>>();

        let mut target_slots_sorted = accounts_to_combine
            .iter()
            .map(|shrink_collect| shrink_collect.slot)
            .collect::<Vec<_>>();

        target_slots_sorted.sort_unstable();
        AccountsToCombine {
            accounts_to_combine,
            target_slots_sorted,
        }
    }

    /// create packed storage and write contents of 'packed' to it.
    /// accumulate results in 'write_ancient_accounts'
    fn write_one_packed_storage<'a, 'b: 'a>(
        &'b self,
        packed: &'a PackedAncientStorage<'a>,
        target_slot: Slot,
        write_ancient_accounts: &mut WriteAncientAccounts<'b>,
    ) {
        let PackedAncientStorage {
            bytes: bytes_total,
            accounts: accounts_to_write,
        } = packed;
        let accounts_to_write = StorableAccountsBySlot::new(target_slot, accounts_to_write, self);

        self.shrink_ancient_stats
            .bytes_ancient_created
            .fetch_add(packed.bytes, Ordering::Relaxed);
        self.shrink_ancient_stats
            .shrink_stats
            .num_slots_shrunk
            .fetch_add(1, Ordering::Relaxed);
        self.write_ancient_accounts(*bytes_total, accounts_to_write, write_ancient_accounts)
    }
}

/// hold all alive accounts to be shrunk and/or combined
#[derive(Debug, Default)]
struct AccountsToCombine<'a> {
    /// All alive accounts to be combined into new storages, along with the slot the accounts are currently stored in
    accounts_to_combine: Vec<ShrinkCollect<AliveAccounts<'a>>>,
    /// slots that contain alive accounts that can be reused for the new storages
    /// sort order is lowest to highest
    target_slots_sorted: Vec<Slot>,
}

#[derive(Default)]
/// intended contents of a packed ancient storage
struct PackedAncientStorage<'a> {
    /// accounts to move into this storage, along with the slot the accounts are currently stored in
    accounts: Vec<(Slot, &'a [&'a AccountFromStorage])>,
    /// total bytes required to hold 'accounts'
    bytes: u64,
}

impl<'a> PackedAncientStorage<'a> {
    /// return a minimal set of 'PackedAncientStorage's to contain all 'accounts_to_combine' with
    /// the new storages having a size guided by 'ideal_size'
    fn pack(
        mut accounts_to_combine: impl Iterator<Item = &'a AliveAccounts<'a>>,
        ideal_size: NonZeroU64,
    ) -> Vec<PackedAncientStorage<'a>> {
        let mut result = Vec::default();
        let ideal_size: u64 = ideal_size.into();
        let ideal_size = ideal_size as usize;
        let mut current_alive_accounts = accounts_to_combine.next();
        // starting at first entry in current_alive_accounts
        let mut partial_inner_index = 0;
        // 0 bytes written so far from the current set of accounts
        let mut partial_bytes_written = Saturating(0);
        // pack a new storage each iteration of this outer loop
        loop {
            let mut bytes_total = 0usize;
            let mut accounts_to_write = Vec::default();

            // walk through each set of alive accounts to pack the current new storage up to ideal_size
            let mut full = false;
            while !full && current_alive_accounts.is_some() {
                let alive_accounts = current_alive_accounts.unwrap();
                if partial_inner_index >= alive_accounts.accounts.len() {
                    // current_alive_accounts have all been written, so advance to next set from accounts_to_combine
                    current_alive_accounts = accounts_to_combine.next();
                    // reset partial progress since we're starting over with a new set of alive accounts
                    partial_inner_index = 0;
                    partial_bytes_written = Saturating(0);
                    continue;
                }
                let bytes_remaining_this_slot =
                    alive_accounts.bytes.saturating_sub(partial_bytes_written.0);
                let bytes_total_with_this_slot =
                    bytes_total.saturating_add(bytes_remaining_this_slot);
                let mut partial_inner_index_max_exclusive;
                if bytes_total_with_this_slot <= ideal_size {
                    partial_inner_index_max_exclusive = alive_accounts.accounts.len();
                    bytes_total = bytes_total_with_this_slot;
                } else {
                    partial_inner_index_max_exclusive = partial_inner_index;
                    // adding all the alive accounts in this storage would exceed the ideal size, so we have to break these accounts up
                    // look at each account and stop when we exceed the ideal size
                    while partial_inner_index_max_exclusive < alive_accounts.accounts.len() {
                        let account = alive_accounts.accounts[partial_inner_index_max_exclusive];
                        let account_size = account.stored_size();
                        let new_size = bytes_total.saturating_add(account_size);
                        if new_size > ideal_size && bytes_total > 0 {
                            full = true;
                            // partial_inner_index_max_exclusive is the index of the first account that puts us over the ideal size
                            // so, save it for next time
                            break;
                        }
                        // this account fits
                        partial_bytes_written += account_size;
                        bytes_total = new_size;
                        partial_inner_index_max_exclusive += 1;
                    }
                }

                if partial_inner_index < partial_inner_index_max_exclusive {
                    // these accounts belong in the current packed storage we're working on
                    accounts_to_write.push((
                        alive_accounts.slot,
                        // maybe all alive accounts from the current or could be partial
                        &alive_accounts.accounts
                            [partial_inner_index..partial_inner_index_max_exclusive],
                    ));
                }
                // start next storage with the account we ended with
                // this could be the end of the current alive accounts or could be anywhere within that vec
                partial_inner_index = partial_inner_index_max_exclusive;
            }
            if accounts_to_write.is_empty() {
                // if we returned without any accounts to write, then we have exhausted source data and have packaged all the storages we need
                break;
            }
            // we know the full contents of this packed storage now
            result.push(PackedAncientStorage {
                bytes: bytes_total as u64,
                accounts: accounts_to_write,
            });
        }
        result
    }
}

/// capacity of an ancient append vec
#[allow(clippy::assertions_on_constants, dead_code)]
pub const fn get_ancient_append_vec_capacity() -> u64 {
    // There is a trade-off for selecting the ancient append vec size. Smaller non-ancient append vec are getting
    // combined into large ancient append vec. Too small size of ancient append vec will result in too many ancient append vec
    // memory mapped files. Too big size will make it difficult to clean and shrink them. Hence, we choose approximately
    // 128MB for the ancient append vec size.
    const RESULT: u64 = 128 * 1024 * 1024;

    use crate::append_vec::MAXIMUM_APPEND_VEC_FILE_SIZE;
    const _: () = assert!(
        RESULT < MAXIMUM_APPEND_VEC_FILE_SIZE,
        "ancient append vec size should be less than the maximum append vec size"
    );
    const PAGE_SIZE: u64 = 4 * 1024;
    const _: () = assert!(
        RESULT.is_multiple_of(PAGE_SIZE),
        "ancient append vec size should be a multiple of PAGE_SIZE"
    );

    RESULT
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            account_info::{AccountInfo, StorageLocation},
            accounts_db::{
                AccountsDbConfig,
                tests::{ACCOUNTS_DB_CONFIG_APPEND_VEC, append_single_account_with_default_hash},
            },
            accounts_index::{ReclaimsSlotList, UpsertReclaim},
            append_vec::{self, AppendVec},
            is_zero_lamport::IsZeroLamport as _,
            storable_accounts::StorableAccountsBySlot,
            utils::create_account_shared_data,
        },
        rand::{Rng, rng, seq::SliceRandom as _},
        solana_account::{AccountSharedData, ReadableAccount, WritableAccount, accounts_equal},
        solana_pubkey::Pubkey,
        std::{collections::HashSet, ops::Range},
        strum::IntoEnumIterator,
        strum_macros::EnumIter,
        test_case::test_case,
    };

    fn get_sample_storages(
        db: &AccountsDb,
        slots: usize,
        account_data_size: Option<u64>,
    ) -> (Vec<Arc<AccountStorageEntry>>, Range<u64>, Vec<SlotInfo>) {
        let slot1 = 1;
        let alive = true;
        create_storages_and_update_index(db, slot1, slots, alive, account_data_size);
        let original_stores = (0..slots)
            .filter_map(|slot| db.storage.get_slot_storage_entry((slot as Slot) + slot1))
            .collect::<Vec<_>>();
        let is_high_slot = false;
        let slot_infos = original_stores
            .iter()
            .map(|storage| SlotInfo {
                storage: Arc::clone(storage),
                slot: storage.slot(),
                written_bytes: 0,
                alive_bytes: 0,
                is_high_slot,
            })
            .collect();
        (original_stores, slot1..(slot1 + slots as Slot), slot_infos)
    }

    fn create_storages_and_update_index(
        db: &AccountsDb,
        starting_slot: Slot,
        num_slots: usize,
        alive: bool,
        account_data_size: Option<u64>,
    ) {
        if num_slots == 0 {
            return;
        }

        let account_data_size = account_data_size.unwrap_or(48);
        let file_size = account_data_size + 1_000_000;
        for i in 0..num_slots {
            let slot = starting_slot + i as Slot;
            let storage = db.create_store(slot, file_size);
            let pubkey = solana_pubkey::new_rand();
            let account = AccountSharedData::new(1, account_data_size as usize, &Pubkey::default());
            append_single_account_with_default_hash(&storage, &pubkey, &account, alive, None);
            db.storage.insert(Arc::new(storage));
        }

        let storage = db.storage.get_slot_storage_entry(starting_slot).unwrap();
        let created_accounts = db.get_unique_accounts_from_storage(&storage);
        assert_eq!(created_accounts.stored_accounts.len(), 1);

        if alive {
            populate_index(db, starting_slot..(starting_slot + (num_slots as Slot) + 1));
        }
    }

    fn populate_index(db: &AccountsDb, slots: Range<Slot>) {
        slots.into_iter().for_each(|slot| {
            if let Some(storage) = db.storage.get_slot_storage_entry(slot) {
                let mut reader = crate::append_vec::new_scan_accounts_reader();
                storage
                    .scan_accounts(&mut reader, None, |offset, account| {
                        let info = AccountInfo::new(
                            StorageLocation::AccountsFile(storage.id(), offset),
                            account.is_zero_lamport(),
                        );
                        db.accounts_index.upsert(
                            slot,
                            slot,
                            account.pubkey(),
                            info,
                            &mut ReclaimsSlotList::new(),
                            UpsertReclaim::IgnoreReclaims,
                        );
                    })
                    .expect("must scan accounts storage");
            }
        })
    }

    fn unique_to_accounts<'a>(
        one: impl Iterator<Item = &'a GetUniqueAccountsResult>,
        db: &AccountsDb,
        slot: Slot,
    ) -> Vec<(Pubkey, AccountSharedData)> {
        one.flat_map(|result| {
            result.stored_accounts.iter().map(|result| {
                (
                    *result.pubkey(),
                    get_account_from_account_from_storage(result, db, slot),
                )
            })
        })
        .collect()
    }

    fn compare_all_vec_accounts<'a>(
        one: impl Iterator<Item = &'a GetUniqueAccountsResult>,
        two: impl Iterator<Item = &'a GetUniqueAccountsResult>,
        db: &AccountsDb,
        slot: Slot,
    ) {
        compare_all_accounts(
            &unique_to_accounts(one, db, slot),
            &unique_to_accounts(two, db, slot),
        );
    }

    #[track_caller]
    fn compare_all_accounts(
        one: &[(Pubkey, AccountSharedData)],
        two: &[(Pubkey, AccountSharedData)],
    ) {
        let mut failures = 0;
        let mut two_indexes = (0..two.len()).collect::<Vec<_>>();
        one.iter().for_each(|(pubkey, account)| {
            for i in 0..two_indexes.len() {
                let pubkey2 = two[two_indexes[i]].0;
                if pubkey2 == *pubkey {
                    if !accounts_equal(account, &two[two_indexes[i]].1) {
                        failures += 1;
                    }
                    two_indexes.remove(i);
                    break;
                }
            }
        });
        // helper method to reduce the volume of logged data to help identify differences
        // modify this when you hit a failure
        let clean = |accounts: &[(Pubkey, AccountSharedData)]| {
            accounts
                .iter()
                .map(|(_pubkey, account)| account.lamports())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            failures,
            0,
            "one: {:?}, two: {:?}, two_indexes: {:?}",
            clean(one),
            clean(two),
            two_indexes,
        );
        assert!(
            two_indexes.is_empty(),
            "one: {one:?}, two: {two:?}, two_indexes: {two_indexes:?}"
        );
    }

    fn get_account_from_account_from_storage(
        account: &AccountFromStorage,
        db: &AccountsDb,
        slot: Slot,
    ) -> AccountSharedData {
        let storage = db
            .storage
            .get_slot_storage_entry_shrinking_in_progress_ok(slot)
            .unwrap();
        storage
            .accounts
            .get_account_shared_data(account.index_info.offset())
            .unwrap()
    }

    fn get_all_accounts(
        db: &AccountsDb,
        slots: impl Iterator<Item = Slot>,
    ) -> Vec<(Pubkey, AccountSharedData)> {
        slots
            .filter_map(|slot| {
                let storage = db.storage.get_slot_storage_entry(slot);
                storage.map(|storage| get_all_accounts_from_storages(std::iter::once(&storage)))
            })
            .flatten()
            .collect::<Vec<_>>()
    }

    fn get_all_accounts_from_storages<'a>(
        storages: impl Iterator<Item = &'a Arc<AccountStorageEntry>>,
    ) -> Vec<(Pubkey, AccountSharedData)> {
        let mut reader = crate::append_vec::new_scan_accounts_reader();
        storages
            .flat_map(|storage| {
                let mut vec = Vec::default();
                storage
                    .accounts
                    .scan_accounts(&mut reader, |_offset, account| {
                        vec.push((*account.pubkey(), create_account_shared_data(&account)));
                    })
                    .expect("must scan accounts storage");
                // make sure scan_pubkeys results match
                // Note that we assume traversals are both in the same order, but this doesn't have to be true.
                let mut compare = Vec::default();
                storage
                    .accounts
                    .scan_pubkeys(|k| {
                        compare.push(*k);
                    })
                    .expect("must scan accounts storage");
                assert_eq!(compare, vec.iter().map(|(k, _)| *k).collect::<Vec<_>>());
                vec
            })
            .collect::<Vec<_>>()
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_write_packed_storages_empty(accounts_db_config: AccountsDbConfig) {
        let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config);
        let (_storages, _slots, _infos) = get_sample_storages(&db, 0, None);
        let write_ancient_accounts =
            db.write_packed_storages(&AccountsToCombine::default(), Vec::default());
        assert!(write_ancient_accounts.shrinks_in_progress.is_empty());
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    #[should_panic(
        expected = "accounts_to_combine.target_slots_sorted.len() >= packed_contents.len()"
    )]
    fn test_write_packed_storages_too_few_slots(accounts_db_config: AccountsDbConfig) {
        let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config);
        let (storages, slots, _infos) = get_sample_storages(&db, 1, None);
        let accounts_to_combine = AccountsToCombine::default();
        let offset = 0;
        let account = storages
            .first()
            .unwrap()
            .accounts
            .get_stored_account_without_data_callback(offset, |account| {
                AccountFromStorage::new(offset, &account)
            })
            .unwrap();
        let accounts = [&account];

        let packed_contents = vec![PackedAncientStorage {
            bytes: 0,
            accounts: vec![(slots.start, &accounts[..])],
        }];
        db.write_packed_storages(&accounts_to_combine, packed_contents);
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_pack_ancient_storages_one_account_per_storage(accounts_db_config: AccountsDbConfig) {
        for num_slots in 0..4 {
            for (ideal_size, expected_storages) in [
                (1, num_slots),
                (get_ancient_append_vec_capacity(), 1.min(num_slots)),
            ] {
                let db =
                    AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
                let (storages, slots, _infos) = get_sample_storages(&db, num_slots, None);
                let original_results = storages
                    .iter()
                    .map(|store| db.get_unique_accounts_from_storage(store))
                    .collect::<Vec<_>>();

                let slots_vec = slots.collect::<Vec<_>>();
                let accounts_to_combine = original_results
                    .iter()
                    .zip(slots_vec.iter().cloned())
                    .map(|(accounts, slot)| AliveAccounts {
                        accounts: accounts.stored_accounts.iter().collect::<Vec<_>>(),
                        bytes: accounts
                            .stored_accounts
                            .iter()
                            .map(|account| AppendVec::calculate_stored_size(account.data_len()))
                            .sum(),
                        slot,
                    })
                    .collect::<Vec<_>>();

                let result = PackedAncientStorage::pack(
                    accounts_to_combine.iter(),
                    NonZeroU64::new(ideal_size).unwrap(),
                );
                let storages_needed = result.len();
                assert_eq!(storages_needed, expected_storages);
            }
        }
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_pack_ancient_storages_one_partial(accounts_db_config: AccountsDbConfig) {
        // n slots
        // m accounts per slot
        // divide into different ideal sizes so that we combine multiple slots sometimes and combine partial slots
        let total_accounts_per_storage = 10;
        let account_size = 184;
        for num_slots in 0..4 {
            for (ideal_size, expected_storages) in [
                (1, num_slots * total_accounts_per_storage),
                (account_size - 1, num_slots * total_accounts_per_storage),
                (account_size, num_slots * total_accounts_per_storage),
                (account_size + 1, num_slots * total_accounts_per_storage),
                (account_size * 2 - 1, num_slots * total_accounts_per_storage),
                (account_size * 2, num_slots * total_accounts_per_storage / 2),
                (get_ancient_append_vec_capacity(), 1.min(num_slots)),
            ] {
                let db =
                    AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
                let (storages, slots, _infos) = get_sample_storages(&db, num_slots, None);

                let account_template = storages
                    .first()
                    .and_then(|storage| storage.accounts.get_account_shared_data(0))
                    .unwrap_or_default();
                // add some accounts to each storage so we can make partial progress
                let mut lamports = 1000;
                let _pubkeys_and_accounts = storages
                    .iter()
                    .map(|storage| {
                        (0..(total_accounts_per_storage - 1))
                            .map(|_| {
                                let pk = solana_pubkey::new_rand();
                                let mut account = account_template.clone();
                                account.set_lamports(lamports);
                                lamports += 1;
                                append_single_account_with_default_hash(
                                    storage,
                                    &pk,
                                    &account,
                                    true,
                                    Some(&db.accounts_index),
                                );
                                (pk, account)
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();

                let original_results = storages
                    .iter()
                    .map(|store| (store.slot(), db.get_unique_accounts_from_storage(store)))
                    .collect::<Vec<_>>();
                let original_results_all_accounts = vec_unique_to_accounts(&original_results, &db);

                let slots_vec = slots.clone().collect::<Vec<_>>();
                let accounts_to_combine = original_results
                    .iter()
                    .zip(slots_vec.iter().cloned())
                    .map(|((_slot, accounts), slot)| AliveAccounts {
                        accounts: accounts.stored_accounts.iter().collect::<Vec<_>>(),
                        bytes: accounts
                            .stored_accounts
                            .iter()
                            .map(|account| AppendVec::calculate_stored_size(account.data_len()))
                            .sum(),
                        slot,
                    })
                    .collect::<Vec<_>>();

                let result = PackedAncientStorage::pack(
                    accounts_to_combine.iter(),
                    NonZeroU64::new(ideal_size).unwrap(),
                );
                let storages_needed = result.len();
                assert_eq!(
                    storages_needed, expected_storages,
                    "num_slots: {num_slots}, expected_storages: {expected_storages}, \
                     storages_needed: {storages_needed}, ideal_size: {ideal_size}"
                );
                compare_all_accounts(
                    &packed_to_compare(&result, &db)[..],
                    &original_results_all_accounts,
                );
            }
        }
    }

    fn packed_to_compare(
        packed: &[PackedAncientStorage],
        db: &AccountsDb,
    ) -> Vec<(Pubkey, AccountSharedData)> {
        packed
            .iter()
            .flat_map(|packed| {
                packed.accounts.iter().flat_map(|(slot, stored_metas)| {
                    stored_metas.iter().map(|stored_meta| {
                        (
                            *stored_meta.pubkey(),
                            get_account_from_account_from_storage(stored_meta, db, *slot),
                        )
                    })
                })
            })
            .collect::<Vec<_>>()
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_pack_ancient_storages_varying(accounts_db_config: AccountsDbConfig) {
        // n slots
        // different number of accounts in each slot
        // each account has different size
        // divide into different ideal sizes so that we combine multiple slots sometimes and combine partial slots
        // compare at end that all accounts are in result exactly once
        let total_accounts_per_storage = 10;
        let account_size = 184;
        for num_slots in 0..4 {
            for ideal_size in [
                1,
                account_size - 1,
                account_size,
                account_size + 1,
                account_size * 2 - 1,
                account_size * 2,
                get_ancient_append_vec_capacity(),
            ] {
                let db =
                    AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
                let (storages, slots, _infos) = get_sample_storages(&db, num_slots, None);

                let account_template = storages
                    .first()
                    .and_then(|storage| storage.accounts.get_account_shared_data(0))
                    .unwrap_or_default();
                // add some accounts to each storage so we can make partial progress
                let mut data_size = 450;
                // random # of extra accounts here
                let total_accounts_per_storage = rng().random_range(0..total_accounts_per_storage);
                let _pubkeys_and_accounts = storages
                    .iter()
                    .map(|storage| {
                        (0..(total_accounts_per_storage - 1))
                            .map(|_| {
                                let pk = solana_pubkey::new_rand();
                                let mut account = account_template.clone();
                                account.set_data_from_slice(
                                    &(0..data_size).map(|x| (x % 256) as u8).collect::<Vec<_>>(),
                                );
                                data_size += 1;
                                append_single_account_with_default_hash(
                                    storage,
                                    &pk,
                                    &account,
                                    true,
                                    Some(&db.accounts_index),
                                );
                                (pk, account)
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();

                let original_results = storages
                    .iter()
                    .map(|store| (store.slot(), db.get_unique_accounts_from_storage(store)))
                    .collect::<Vec<_>>();
                let original_results_all_accounts = vec_unique_to_accounts(&original_results, &db);

                let slots_vec = slots.clone().collect::<Vec<_>>();
                let accounts_to_combine = original_results
                    .iter()
                    .zip(slots_vec.iter().cloned())
                    .map(|((_slot, accounts), slot)| AliveAccounts {
                        accounts: accounts.stored_accounts.iter().collect::<Vec<_>>(),
                        bytes: accounts
                            .stored_accounts
                            .iter()
                            .map(|account| AppendVec::calculate_stored_size(account.data_len()))
                            .sum(),
                        slot,
                    })
                    .collect::<Vec<_>>();

                let result = PackedAncientStorage::pack(
                    accounts_to_combine.iter(),
                    NonZeroU64::new(ideal_size).unwrap(),
                );

                let largest_account_size = AppendVec::calculate_stored_size(data_size) as u64;
                // all packed storages should be close to ideal size
                result.iter().enumerate().for_each(|(i, packed)| {
                    if i + 1 < result.len() && ideal_size > largest_account_size {
                        // cannot assert this on the last packed storage - it may be small
                        // cannot assert this when the ideal size is too small to hold the largest account size
                        assert!(
                            packed.bytes >= ideal_size - largest_account_size,
                            "packed size too small: bytes: {}, ideal: {}, largest: {}",
                            packed.bytes,
                            ideal_size,
                            largest_account_size
                        );
                    }
                    assert!(packed.bytes > 0, "packed size of zero");
                    assert!(
                        packed.bytes <= ideal_size
                            || packed
                                .accounts
                                .iter()
                                .map(|(_slot, accounts)| accounts.len())
                                .sum::<usize>()
                                == 1,
                        "packed size too large: bytes: {}, ideal_size: {}, data_size: {}, \
                         num_slots: {}, # accounts: {}",
                        packed.bytes,
                        ideal_size,
                        data_size,
                        num_slots,
                        packed.accounts.len()
                    );
                });
                result.iter().for_each(|packed| {
                    assert_eq!(
                        packed.bytes,
                        packed
                            .accounts
                            .iter()
                            .map(|(_slot, accounts)| accounts
                                .iter()
                                .map(
                                    |account| AppendVec::calculate_stored_size(account.data_len())
                                        as u64
                                )
                                .sum::<u64>())
                            .sum::<u64>()
                    );
                });

                compare_all_accounts(
                    &packed_to_compare(&result, &db)[..],
                    &original_results_all_accounts,
                );
            }
        }
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_finish_combine_ancient_slots_packed_internal(accounts_db_config: AccountsDbConfig) {
        // n storages
        // 1 account each
        // nothing shrunk, so all storages and roots should be removed
        // or all slots shrunk so no roots or storages should be removed
        for in_shrink_candidate_slots in [false, true] {
            for all_slots_shrunk in [false, true] {
                for num_slots in 0..3 {
                    let db = AccountsDb::new_for_tests_with_config(
                        Vec::new(),
                        accounts_db_config.clone(),
                    );
                    let (storages, slots, infos) = get_sample_storages(&db, num_slots, None);
                    let mut accounts_per_storage = infos
                        .iter()
                        .zip(
                            storages
                                .iter()
                                .map(|store| db.get_unique_accounts_from_storage(store)),
                        )
                        .collect::<Vec<_>>();

                    let accounts_to_combine =
                        db.calc_accounts_to_combine(&mut accounts_per_storage);
                    let mut stats = SquashStatsSub::default();
                    let mut write_ancient_accounts = WriteAncientAccounts::default();

                    slots.clone().for_each(|slot| {
                        let storage = db.storage.get_slot_storage_entry(slot);
                        assert!(storage.is_some());
                        if in_shrink_candidate_slots {
                            db.shrink_candidate_slots.lock().unwrap().insert(slot);
                        }
                    });

                    if all_slots_shrunk {
                        // make it look like each of the slots was shrunk
                        slots.clone().for_each(|slot| {
                            let old_store = db
                                .storage
                                .get_slot_storage_entry_shrinking_in_progress_ok(slot)
                                .unwrap();
                            write_ancient_accounts
                                .shrinks_in_progress
                                .insert(slot, db.get_store_for_shrink(slot, old_store, 1));
                        });
                    }

                    db.finish_combine_ancient_slots_packed_internal(
                        accounts_to_combine,
                        write_ancient_accounts,
                        &mut stats,
                    );

                    slots.clone().for_each(|slot| {
                        assert!(!db.shrink_candidate_slots.lock().unwrap().contains(&slot));
                    });
                    slots.for_each(|slot| {
                        let storage = db.storage.get_slot_storage_entry(slot);
                        if all_slots_shrunk {
                            assert!(storage.is_some());
                            assert!(!storage.unwrap().has_accounts());
                        } else {
                            assert!(storage.is_none());
                        }
                    });
                }
            }
        }
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_get_unique_accounts_from_storage_for_combining_ancient_slots(
        accounts_db_config: AccountsDbConfig,
    ) {
        for num_slots in 0..3 {
            for reverse in [false, true] {
                let db =
                    AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
                let (storages, slots, mut infos) = get_sample_storages(&db, num_slots, None);
                let original_results = storages
                    .iter()
                    .map(|store| db.get_unique_accounts_from_storage(store))
                    .collect::<Vec<_>>();
                if reverse {
                    // reverse the contents for further testing
                    infos = infos.into_iter().rev().collect();
                }
                let results =
                    db.get_unique_accounts_from_storage_for_combining_ancient_slots(&infos);

                let all_accounts = get_all_accounts(&db, slots.clone());
                assert_eq!(all_accounts.len(), num_slots);

                compare_all_vec_accounts(
                    original_results.iter(),
                    results.iter().map(|(_, accounts)| accounts),
                    &db,
                    slots.start,
                );
                compare_all_accounts(
                    &all_accounts,
                    &unique_to_accounts(
                        results.iter().map(|(_, accounts)| accounts),
                        &db,
                        slots.start,
                    ),
                );

                let map = |info: &SlotInfo| {
                    (
                        info.storage.id(),
                        info.slot,
                        info.written_bytes,
                        info.alive_bytes,
                    )
                };
                assert_eq!(
                    infos.iter().map(map).collect::<Vec<_>>(),
                    results
                        .into_iter()
                        .map(|(info, _)| map(info))
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    fn assert_storage_info(info: &SlotInfo, storage: &AccountStorageEntry) {
        assert_eq!(storage.id(), info.storage.id());
        assert_eq!(storage.slot(), info.slot);
        assert_eq!(storage.written_bytes(), info.written_bytes);
        assert_eq!(storage.alive_bytes(), info.alive_bytes as usize);
    }

    #[derive(EnumIter, Debug, PartialEq, Eq)]
    enum TestCollectInfo {
        CollectSortFilterInfo,
        CalcAncientSlotInfo,
        Add,
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_calc_ancient_slot_info_one_alive_only(accounts_db_config: AccountsDbConfig) {
        let alive = true;
        let slots = 1;
        for method in TestCollectInfo::iter() {
            // 1_040_000 is big enough relative to page size to cause shrink ratio to be triggered
            for data_size in [None, Some(1_040_000)] {
                let db =
                    AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
                let slot1 = 1;
                create_storages_and_update_index(&db, slot1, slots, alive, data_size);
                let mut infos = AncientSlotInfos::default();
                let storage = db.storage.get_slot_storage_entry(slot1).unwrap();
                let alive_bytes_expected = storage.alive_bytes();
                let high_slot = false;
                let is_candidate_for_shrink = db.is_candidate_for_shrink(&storage);
                let mut tuning = PackedAncientStorageTuning {
                    max_ancient_slots: 0,
                    // irrelevant for what this test is trying to test, but necessary to avoid minimums
                    ideal_storage_size: NonZeroU64::new(get_ancient_append_vec_capacity()).unwrap(),
                    ..default_tuning()
                };
                match method {
                    TestCollectInfo::Add => {
                        // test lower level 'add'
                        infos.add(
                            slot1,
                            Arc::clone(&storage),
                            db.alive_bytes_after_shrink(&storage) as u64,
                            NonZeroU64::new(get_ancient_append_vec_capacity()).unwrap(),
                            high_slot,
                            is_candidate_for_shrink,
                        );
                    }
                    TestCollectInfo::CalcAncientSlotInfo => {
                        infos = db.calc_ancient_slot_info(vec![slot1], &tuning);
                    }
                    TestCollectInfo::CollectSortFilterInfo => {
                        infos = db.collect_sort_filter_ancient_slots(vec![slot1], &mut tuning);
                    }
                }
                assert_eq!(infos.all_infos.len(), 1, "{method:?}");
                assert_storage_info(infos.all_infos.first().unwrap(), &storage);

                assert!(infos.shrink_indexes.is_empty());
                assert_eq!(infos.total_alive_bytes.0, alive_bytes_expected as u64);
            }
        }
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_calc_ancient_slot_info_one_dead(accounts_db_config: AccountsDbConfig) {
        let alive = false;
        let slots = 1;
        for call_add in [false, true] {
            let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
            let slot1 = 1;
            create_storages_and_update_index(&db, slot1, slots, alive, None);
            let mut infos = AncientSlotInfos::default();
            let storage = db.storage.get_slot_storage_entry(slot1).unwrap();
            let high_slot = false;
            let is_candidate_for_shrink = db.is_candidate_for_shrink(&storage);
            if call_add {
                infos.add(
                    slot1,
                    Arc::clone(&storage),
                    db.alive_bytes_after_shrink(&storage) as u64,
                    NonZeroU64::new(get_ancient_append_vec_capacity()).unwrap(),
                    high_slot,
                    is_candidate_for_shrink,
                );
            } else {
                let tuning = PackedAncientStorageTuning {
                    max_ancient_slots: 0,
                    // irrelevant for what this test is trying to test, but necessary to avoid minimums
                    ideal_storage_size: NonZeroU64::new(get_ancient_append_vec_capacity()).unwrap(),
                    ..default_tuning()
                };
                infos = db.calc_ancient_slot_info(vec![slot1], &tuning);
            }
            assert!(infos.all_infos.is_empty());
            assert!(infos.shrink_indexes.is_empty());
            assert_eq!(infos.total_alive_bytes.0, 0);
        }
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_calc_ancient_slot_info_several(accounts_db_config: AccountsDbConfig) {
        let tuning = PackedAncientStorageTuning {
            max_ancient_slots: 0,
            // irrelevant for what this test is trying to test, but necessary to avoid minimums
            ideal_storage_size: NonZeroU64::new(get_ancient_append_vec_capacity()).unwrap(),
            ..default_tuning()
        };
        for alive in [true, false] {
            for slots in 0..4 {
                // 1_040_000 is big enough relative to page size to cause shrink ratio to be triggered
                for data_size in [None, Some(1_040_000)] {
                    let db = AccountsDb::new_for_tests_with_config(
                        Vec::new(),
                        accounts_db_config.clone(),
                    );
                    let slot1 = 1;
                    create_storages_and_update_index(&db, slot1, slots, alive, data_size);
                    let slot_vec = (slot1..(slot1 + slots as Slot)).collect::<Vec<_>>();
                    let storages = slot_vec
                        .iter()
                        .map(|slot| db.storage.get_slot_storage_entry(*slot).unwrap())
                        .collect::<Vec<_>>();
                    let alive_bytes_expected = storages
                        .iter()
                        .map(|storage| storage.alive_bytes() as u64)
                        .sum::<u64>();
                    let infos = db.calc_ancient_slot_info(slot_vec.clone(), &tuning);
                    if !alive {
                        assert!(infos.all_infos.is_empty());
                        assert!(infos.shrink_indexes.is_empty());
                        assert_eq!(infos.total_alive_bytes.0, 0);
                    } else {
                        assert_eq!(infos.all_infos.len(), slots);
                        storages
                            .iter()
                            .zip(infos.all_infos.iter())
                            .for_each(|(storage, info)| {
                                assert_storage_info(info, storage);
                                assert!(infos.shrink_indexes.is_empty());
                                assert_eq!(infos.total_alive_bytes.0, alive_bytes_expected);
                            });
                    }
                }
            }
        }
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_calc_ancient_slot_info_one_alive_one_dead(accounts_db_config: AccountsDbConfig) {
        let tuning = PackedAncientStorageTuning {
            ideal_storage_size: NonZeroU64::new(get_ancient_append_vec_capacity()).unwrap(),
            ..default_tuning()
        };
        for method in TestCollectInfo::iter() {
            for slot1_is_alive in [false, true] {
                let alives = [false /*dummy*/, slot1_is_alive, !slot1_is_alive];
                let slots = 2;
                // 1_040_000 is big enough relative to page size to cause shrink ratio to be triggered
                for data_size in [None, Some(1_040_000)] {
                    let db = AccountsDb::new_for_tests_with_config(
                        Vec::new(),
                        accounts_db_config.clone(),
                    );
                    let slot1 = 1;
                    let alive = true;
                    create_storages_and_update_index(&db, slot1, slots, alive, data_size);
                    assert_eq!(slot1, 1); // make sure index into alives will be correct
                    assert_eq!(alives[slot1 as usize], slot1_is_alive);
                    let slot_vec = (slot1..(slot1 + slots as Slot)).collect::<Vec<_>>();
                    let storages = slot_vec
                        .iter()
                        .map(|slot| db.storage.get_slot_storage_entry(*slot).unwrap())
                        .collect::<Vec<_>>();
                    storages.iter().for_each(|storage| {
                        let slot = storage.slot();
                        let alive = alives[slot as usize];
                        if !alive {
                            // make this storage not alive
                            storage.remove_accounts(storage.written_bytes() as usize, 1);
                        }
                    });
                    let alive_storages = storages
                        .iter()
                        .filter(|storage| alives[storage.slot() as usize])
                        .collect::<Vec<_>>();
                    let alive_bytes_expected = alive_storages
                        .iter()
                        .map(|storage| storage.alive_bytes() as u64)
                        .sum::<u64>();

                    let infos = match method {
                        TestCollectInfo::CalcAncientSlotInfo => {
                            db.calc_ancient_slot_info(slot_vec.clone(), &tuning)
                        }
                        TestCollectInfo::Add => {
                            continue; // unsupportable
                        }
                        TestCollectInfo::CollectSortFilterInfo => {
                            let mut tuning = PackedAncientStorageTuning {
                                max_ancient_slots: 0,
                                // irrelevant
                                ideal_storage_size: NonZeroU64::new(
                                    get_ancient_append_vec_capacity(),
                                )
                                .unwrap(),
                                ..default_tuning()
                            };
                            db.collect_sort_filter_ancient_slots(slot_vec.clone(), &mut tuning)
                        }
                    };
                    assert_eq!(infos.all_infos.len(), 1, "method: {method:?}");
                    alive_storages.iter().zip(infos.all_infos.iter()).for_each(
                        |(storage, info)| {
                            assert_storage_info(info, storage);
                            assert!(infos.shrink_indexes.is_empty());
                            assert_eq!(infos.total_alive_bytes.0, alive_bytes_expected);
                        },
                    );
                }
            }
        }
    }

    fn create_test_infos(db: &AccountsDb, count: usize) -> AncientSlotInfos {
        let slot1 = 1;
        let alive = true;
        create_storages_and_update_index(db, slot1, 1, alive, None);
        let storage = db.storage.get_slot_storage_entry(slot1).unwrap();
        AncientSlotInfos {
            all_infos: (0..count)
                .map(|index| SlotInfo {
                    storage: Arc::clone(&storage),
                    slot: index as Slot,
                    written_bytes: 1,
                    alive_bytes: 1,
                    is_high_slot: false,
                })
                .collect(),
            shrink_indexes: (0..count).collect(),
            ..AncientSlotInfos::default()
        }
    }

    #[derive(EnumIter, Debug, PartialEq, Eq)]
    enum TestSmallestCapacity {
        FilterAncientSlots,
        FilterBySmallestCapacity,
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_filter_by_smallest_capacity_empty(accounts_db_config: AccountsDbConfig) {
        for method in TestSmallestCapacity::iter() {
            for max_storages in 1..3 {
                let db =
                    AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
                // requesting N max storage, has 1 storage, N >= 1 so nothing to do
                let ideal_storage_size_large = get_ancient_append_vec_capacity();
                let mut infos = create_test_infos(&db, 1);
                let tuning = PackedAncientStorageTuning {
                    max_ancient_slots: max_storages,
                    ideal_storage_size: NonZeroU64::new(ideal_storage_size_large).unwrap(),
                    ..default_tuning()
                };
                match method {
                    TestSmallestCapacity::FilterAncientSlots => {
                        infos.shrink_indexes.clear();
                        infos.filter_ancient_slots(&tuning, &ShrinkAncientStats::default());
                    }
                    TestSmallestCapacity::FilterBySmallestCapacity => {
                        infos.filter_by_smallest_capacity(&tuning, &ShrinkAncientStats::default());
                    }
                }
                assert!(infos.all_infos.is_empty());
            }
        }
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_filter_by_smallest_capacity_sort(accounts_db_config: AccountsDbConfig) {
        // max is 6
        // 7 storages
        // storage[last] is big enough to cause us to need another storage
        // so, storage[0..=4] can be combined into 1, resulting in 3 remaining storages, which is
        // the goal, so we only have to combine the first 5 to hit the goal
        for method in TestSmallestCapacity::iter() {
            let ideal_storage_size_large = get_ancient_append_vec_capacity();
            for reorder in [false, true] {
                let db =
                    AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
                let mut infos = create_test_infos(&db, 7);
                infos
                    .all_infos
                    .iter_mut()
                    .enumerate()
                    .for_each(|(i, info)| info.written_bytes = 1 + i as u64);
                if reorder {
                    infos.all_infos.last_mut().unwrap().written_bytes = 0; // sort to beginning
                }
                infos.all_infos.last_mut().unwrap().alive_bytes = ideal_storage_size_large;
                // if we use max_storages = 3 or 4, then the low limit is 1 or 2. To get below 2 requires a result of 1, which packs everyone.
                // This isn't what we want for this test. So, we make max_storages big enough that we can get to something reasonable (like 3)
                // for a low mark.
                let max_storages = 6;

                let tuning = PackedAncientStorageTuning {
                    max_ancient_slots: max_storages,
                    ideal_storage_size: NonZeroU64::new(ideal_storage_size_large).unwrap(),
                    ..default_tuning()
                };
                match method {
                    TestSmallestCapacity::FilterBySmallestCapacity => {
                        infos.filter_by_smallest_capacity(&tuning, &ShrinkAncientStats::default());
                    }
                    TestSmallestCapacity::FilterAncientSlots => {
                        infos.shrink_indexes.clear();
                        infos.filter_ancient_slots(&tuning, &ShrinkAncientStats::default());
                    }
                }
                assert_eq!(
                    infos
                        .all_infos
                        .iter()
                        .map(|info| info.slot)
                        .collect::<Vec<_>>(),
                    if reorder {
                        vec![6, 0, 1, 2, 3, 4]
                    } else {
                        vec![0, 1, 2, 3, 4]
                    },
                    "reorder: {reorder}, method: {method:?}"
                );
            }
        }
    }

    /// Test that we always include the high slots when filtering which ancient infos to pack
    ///
    /// If we have *more* high slots than max resulting storages set in the tuning parameters,
    /// we should still have all the high slots after calling `filter_by_smallest_capacity().
    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_filter_by_smallest_capacity_high_slot_more(accounts_db_config: AccountsDbConfig) {
        let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config);
        let tuning = default_tuning();

        // Ensure we have more storages with high slots than the 'max resulting storages'.
        let num_high_slots = tuning.max_resulting_storages.get() * 2;
        let num_ancient_storages = num_high_slots * 3;
        let mut infos = create_test_infos(&db, num_ancient_storages as usize);
        infos
            .all_infos
            .sort_unstable_by_key(|slot_info| slot_info.slot);
        infos
            .all_infos
            .iter_mut()
            .rev()
            .take(num_high_slots as usize)
            .for_each(|slot_info| {
                slot_info.is_high_slot = true;
            });
        let slots_expected: Vec<_> = infos
            .all_infos
            .iter()
            .filter_map(|slot_info| slot_info.is_high_slot.then_some(slot_info.slot))
            .collect();

        // shuffle the infos so they actually need to be sorted
        infos.all_infos.shuffle(&mut rng());
        infos.filter_by_smallest_capacity(&tuning, &ShrinkAncientStats::default());

        infos
            .all_infos
            .sort_unstable_by_key(|slot_info| slot_info.slot);
        let slots_actual: Vec<_> = infos
            .all_infos
            .iter()
            .map(|slot_info| slot_info.slot)
            .collect();
        assert_eq!(infos.all_infos.len() as u64, num_high_slots);
        assert_eq!(slots_actual, slots_expected);
    }

    /// Test that we always include the high slots when filtering which ancient infos to pack
    ///
    /// If we have *less* high slots than max resulting storages set in the tuning parameters,
    /// we should still have all the high slots after calling `filter_by_smallest_capacity().
    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_filter_by_smallest_capacity_high_slot_less(accounts_db_config: AccountsDbConfig) {
        let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config);
        let tuning = default_tuning();

        // Ensure we have less storages with high slots than the 'max resulting storages'.
        let num_high_slots = tuning.max_resulting_storages.get() / 2;
        let num_ancient_storages = num_high_slots * 5;
        let mut infos = create_test_infos(&db, num_ancient_storages as usize);
        infos
            .all_infos
            .sort_unstable_by_key(|slot_info| slot_info.slot);
        infos
            .all_infos
            .iter_mut()
            .rev()
            .take(num_high_slots as usize)
            .for_each(|slot_info| {
                slot_info.is_high_slot = true;
            });
        let high_slots: Vec<_> = infos
            .all_infos
            .iter()
            .filter_map(|slot_info| slot_info.is_high_slot.then_some(slot_info.slot))
            .collect();

        // shuffle the infos so they actually need to be sorted
        infos.all_infos.shuffle(&mut rng());
        infos.filter_by_smallest_capacity(&tuning, &ShrinkAncientStats::default());

        infos
            .all_infos
            .sort_unstable_by_key(|slot_info| slot_info.slot);
        let slots_actual: HashSet<_> = infos
            .all_infos
            .iter()
            .map(|slot_info| slot_info.slot)
            .collect();
        assert_eq!(
            infos.all_infos.len() as u64,
            tuning.max_resulting_storages.get(),
        );
        assert!(
            high_slots
                .iter()
                .all(|high_slot| slots_actual.contains(high_slot))
        );
    }

    fn test(filter: bool, infos: &mut AncientSlotInfos, tuning: &PackedAncientStorageTuning) {
        if filter {
            infos.filter_by_smallest_capacity(tuning, &ShrinkAncientStats::default());
        } else {
            infos.truncate_to_max_storages(tuning, &ShrinkAncientStats::default());
        }
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_truncate_to_max_storages(accounts_db_config: AccountsDbConfig) {
        for filter in [false, true] {
            let ideal_storage_size_large = get_ancient_append_vec_capacity();
            let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
            let mut infos = create_test_infos(&db, 1);
            let max_storages = 1;
            // 1 storage, 1 max, but 1 storage does not fill the entire new combined storage, so truncate nothing
            let tuning = PackedAncientStorageTuning {
                max_ancient_slots: max_storages,
                ideal_storage_size: NonZeroU64::new(ideal_storage_size_large).unwrap(),
                ..default_tuning()
            };
            test(filter, &mut infos, &tuning);
            assert_eq!(infos.all_infos.len(), usize::from(!filter));

            let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
            let mut infos = create_test_infos(&db, 1);
            let max_storages = 1;
            let tuning = PackedAncientStorageTuning {
                max_ancient_slots: max_storages,
                ideal_storage_size: NonZeroU64::new(ideal_storage_size_large).unwrap(),
                ..default_tuning()
            };
            infos.all_infos[0].alive_bytes = ideal_storage_size_large + 1; // too big for 1 ideal storage
            // 1 storage, 1 max, but 1 overflows the entire new combined storage, so truncate nothing
            test(filter, &mut infos, &tuning);
            assert_eq!(infos.all_infos.len(), usize::from(!filter));

            let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
            let mut infos = create_test_infos(&db, 1);
            let max_storages = 2;
            let tuning = PackedAncientStorageTuning {
                max_ancient_slots: max_storages,
                ideal_storage_size: NonZeroU64::new(ideal_storage_size_large).unwrap(),
                ..default_tuning()
            };
            // all truncated because these infos will fit into the # storages
            test(filter, &mut infos, &tuning);

            if filter {
                assert!(infos.all_infos.is_empty());
            } else {
                // no short circuit, so truncate to shrink below low water
                assert_eq!(
                    infos
                        .all_infos
                        .iter()
                        .map(|info| info.slot)
                        .collect::<Vec<_>>(),
                    vec![0]
                );
            }

            let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
            let mut infos = create_test_infos(&db, 1);
            infos.all_infos[0].alive_bytes = ideal_storage_size_large + 1;
            let max_storages = 2;
            let tuning = PackedAncientStorageTuning {
                max_ancient_slots: max_storages,
                ideal_storage_size: NonZeroU64::new(ideal_storage_size_large).unwrap(),
                ..default_tuning()
            };

            // none truncated because the one storage calculates to be larger than 1 ideal storage, so we need to
            // combine
            test(filter, &mut infos, &tuning);
            assert_eq!(
                infos
                    .all_infos
                    .iter()
                    .map(|info| info.slot)
                    .collect::<Vec<_>>(),
                if filter { Vec::default() } else { vec![0] }
            );

            // both need to be combined to reach '1'
            let max_storages = 1;
            for ideal_storage_size in [1, 2] {
                let tuning = PackedAncientStorageTuning {
                    max_ancient_slots: max_storages,
                    ideal_storage_size: NonZeroU64::new(ideal_storage_size).unwrap(),
                    ..default_tuning()
                };
                let db =
                    AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
                let mut infos = create_test_infos(&db, 2);
                test(filter, &mut infos, &tuning);
                assert_eq!(infos.all_infos.len(), 2);
            }

            // max is 4
            // 5 storages
            // storage[4] is big enough to cause us to need another storage
            // so, storage[0..=2] can be combined into 1, resulting in 3 remaining storages, which is
            // the goal, so we only have to combine the first 3 to hit the goal
            let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
            let mut infos = create_test_infos(&db, 5);
            infos.all_infos[4].alive_bytes = ideal_storage_size_large;
            let max_storages = 4;
            let tuning = PackedAncientStorageTuning {
                max_ancient_slots: max_storages,
                ideal_storage_size: NonZeroU64::new(ideal_storage_size_large).unwrap(),
                ..default_tuning()
            };

            test(filter, &mut infos, &tuning);
            assert_eq!(
                infos
                    .all_infos
                    .iter()
                    .map(|info| info.slot)
                    .collect::<Vec<_>>(),
                vec![0, 1, 2, 3, 4]
            );
        }
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_calc_ancient_slot_info_one_shrink_one_not(accounts_db_config: AccountsDbConfig) {
        let mut tuning = PackedAncientStorageTuning {
            max_ancient_slots: 0,
            // irrelevant for what this test is trying to test, but necessary to avoid minimums
            ideal_storage_size: NonZeroU64::new(get_ancient_append_vec_capacity()).unwrap(),
            ..default_tuning()
        };
        for method in TestCollectInfo::iter() {
            for slot1_shrink in [false, true] {
                let shrinks = [false /*dummy*/, slot1_shrink, !slot1_shrink];
                let slots = 2;
                // 1_040_000 is big enough relative to page size to cause shrink ratio to be triggered
                let data_sizes = shrinks
                    .iter()
                    .map(|shrink| (!shrink).then_some(1_040_000))
                    .collect::<Vec<_>>();
                let db =
                    AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
                let slot1 = 1;
                let alive = true;
                create_storages_and_update_index(&db, slot1, 1, alive, data_sizes[1]);
                create_storages_and_update_index(
                    &db,
                    slot1 + 1,
                    1,
                    true,
                    data_sizes[(slot1 + 1) as usize],
                );

                assert_eq!(slot1, 1); // make sure index into shrinks will be correct
                assert_eq!(shrinks[slot1 as usize], slot1_shrink);
                let slot_vec = (slot1..(slot1 + slots as Slot)).collect::<Vec<_>>();
                let storages = slot_vec
                    .iter()
                    .map(|slot| {
                        let storage = db.storage.get_slot_storage_entry(*slot).unwrap();
                        assert_eq!(*slot, storage.slot());
                        storage
                    })
                    .collect::<Vec<_>>();
                for storage in &storages {
                    append_single_account_with_default_hash(
                        storage,
                        &Pubkey::new_unique(),
                        &AccountSharedData::default(),
                        false,
                        None,
                    );
                }
                let alive_bytes_expected = storages
                    .iter()
                    .map(|storage| storage.alive_bytes() as u64)
                    .sum::<u64>();
                let infos = match method {
                    TestCollectInfo::CalcAncientSlotInfo => {
                        db.calc_ancient_slot_info(slot_vec.clone(), &tuning)
                    }
                    TestCollectInfo::Add => {
                        continue; // unsupportable
                    }
                    TestCollectInfo::CollectSortFilterInfo => {
                        // note this can sort infos.all_infos
                        db.collect_sort_filter_ancient_slots(slot_vec.clone(), &mut tuning)
                    }
                };

                assert_eq!(infos.all_infos.len(), 2, "{method:?}");
                storages.iter().for_each(|storage| {
                    assert!(
                        infos
                            .all_infos
                            .iter()
                            .any(|info| info.slot == storage.slot())
                    );
                });
                assert_eq!(
                    infos.shrink_indexes,
                    match method {
                        TestCollectInfo::CollectSortFilterInfo => Vec::default(),
                        TestCollectInfo::CalcAncientSlotInfo => vec![0, 1],
                        _ => shrinks
                            .iter()
                            .skip(1)
                            .enumerate()
                            .filter_map(|(i, shrink)| shrink.then_some(i))
                            .collect::<Vec<_>>(),
                    }
                );
                assert_eq!(infos.total_alive_bytes.0, alive_bytes_expected);
            }
        }
    }

    fn default_tuning() -> PackedAncientStorageTuning {
        PackedAncientStorageTuning {
            max_ancient_slots: 0,
            ideal_storage_size: NonZeroU64::new(1).unwrap(),
            max_resulting_storages: NonZeroU64::new(10).unwrap(),
        }
    }

    #[derive(EnumIter, Debug, PartialEq, Eq)]
    enum TestWriteAncient {
        OnePackedStorage,
        AncientAccounts,
        PackedStorages,
    }

    pub fn build_refs_accounts_from_storage_with_slot(
        accounts: &[(Slot, Vec<AccountFromStorage>)],
    ) -> Vec<(Slot, Vec<&AccountFromStorage>)> {
        accounts
            .iter()
            .map(|(slot, accounts)| (*slot, accounts.iter().collect()))
            .collect::<Vec<_>>()
    }

    pub fn build_refs_accounts_from_storage_with_slot2<'a>(
        accounts: &'a [(Slot, Vec<&'a AccountFromStorage>)],
    ) -> Vec<(Slot, &'a [&'a AccountFromStorage])> {
        accounts
            .iter()
            .map(|(slot, accounts)| (*slot, &accounts[..]))
            .collect::<Vec<_>>()
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_write_ancient_accounts(accounts_db_config: AccountsDbConfig) {
        for data_size in [None, Some(10_000_000)] {
            for method in TestWriteAncient::iter() {
                for num_slots in 0..4 {
                    for combine_into in 0..=num_slots {
                        if combine_into == num_slots && num_slots > 0 {
                            // invalid combination when num_slots > 0, but required to hit num_slots=0, combine_into=0
                            continue;
                        }
                        let db = AccountsDb::new_for_tests_with_config(
                            Vec::new(),
                            accounts_db_config.clone(),
                        );
                        let (storages, slots, _infos) =
                            get_sample_storages(&db, num_slots, data_size);

                        let initial_accounts = get_all_accounts(&db, slots.clone());

                        let accounts_byval = storages
                            .iter()
                            .map(|storage| {
                                let mut accounts = Vec::default();
                                storage
                                    .accounts
                                    .scan_accounts_without_data(|offset, account| {
                                        accounts.push(AccountFromStorage::new(offset, &account));
                                    })
                                    .expect("must scan accounts storage");
                                (storage.slot(), accounts)
                            })
                            .collect::<Vec<_>>();

                        // reshape the data
                        let accounts_byval2 =
                            build_refs_accounts_from_storage_with_slot(&accounts_byval);
                        let accounts =
                            build_refs_accounts_from_storage_with_slot2(&accounts_byval2);

                        let target_slot = slots.clone().nth(combine_into).unwrap_or(slots.start);
                        let accounts_to_write =
                            StorableAccountsBySlot::new(target_slot, &accounts, &db);

                        let bytes = storages
                            .iter()
                            .map(|storage| storage.written_bytes())
                            .sum::<u64>();
                        assert_eq!(
                            bytes,
                            initial_accounts
                                .iter()
                                .map(|(_, account)| AppendVec::calculate_stored_size(
                                    account.data().len()
                                ) as u64)
                                .sum::<u64>()
                        );

                        if num_slots > 0 {
                            let mut write_ancient_accounts = WriteAncientAccounts::default();

                            match method {
                                TestWriteAncient::AncientAccounts => db.write_ancient_accounts(
                                    bytes,
                                    accounts_to_write,
                                    &mut write_ancient_accounts,
                                ),

                                TestWriteAncient::OnePackedStorage => {
                                    let packed = PackedAncientStorage { accounts, bytes };
                                    db.write_one_packed_storage(
                                        &packed,
                                        target_slot,
                                        &mut write_ancient_accounts,
                                    );
                                }
                                TestWriteAncient::PackedStorages => {
                                    let packed = PackedAncientStorage { accounts, bytes };

                                    let accounts_to_combine = AccountsToCombine {
                                        // target slots are supposed to be read in reverse order, so test that
                                        target_slots_sorted: vec![
                                            Slot::MAX, // this asserts if it gets used
                                            Slot::MAX,
                                            target_slot,
                                        ],
                                        ..AccountsToCombine::default()
                                    };

                                    write_ancient_accounts = db
                                        .write_packed_storages(&accounts_to_combine, vec![packed]);
                                }
                            };
                            let mut result = write_ancient_accounts.shrinks_in_progress;
                            let one = result.drain().collect::<Vec<_>>();
                            assert_eq!(1, one.len());
                            assert_eq!(target_slot, one.first().unwrap().0);
                            assert_eq!(
                                one.first().unwrap().1.old_storage().id(),
                                storages[combine_into].id()
                            );
                            let mut reader = append_vec::new_scan_accounts_reader();

                            // make sure the single new append vec contains all the same accounts
                            let mut two = Vec::default();
                            one.first()
                                .unwrap()
                                .1
                                .new_storage()
                                .accounts
                                .scan_accounts(&mut reader, |_offset, meta| {
                                    two.push((*meta.pubkey(), create_account_shared_data(&meta)));
                                })
                                .expect("must scan accounts storage");

                            compare_all_accounts(&initial_accounts, &two[..]);
                        }
                        let all_accounts = get_all_accounts(&db, target_slot..(target_slot + 1));

                        compare_all_accounts(&initial_accounts, &all_accounts);
                    }
                }
            }
        }
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_sort_shrink_indexes_by_bytes_saved(accounts_db_config: AccountsDbConfig) {
        let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config);
        let slot1 = 1;
        let alive = true;
        create_storages_and_update_index(&db, slot1, 1, alive, None);
        let storage = db.storage.get_slot_storage_entry(slot1).unwrap();
        // ignored
        let slot = 0;

        // info1 is first, equal, last
        for info1_written_bytes in [0, 1, 2] {
            let info1 = SlotInfo {
                storage: storage.clone(),
                slot,
                written_bytes: info1_written_bytes,
                alive_bytes: 0,
                is_high_slot: false,
            };
            let info2 = SlotInfo {
                storage: storage.clone(),
                slot,
                written_bytes: 2,
                alive_bytes: 1,
                is_high_slot: false,
            };
            let mut infos = AncientSlotInfos {
                all_infos: vec![info1, info2],
                shrink_indexes: vec![0, 1],
                ..AncientSlotInfos::default()
            };
            infos.sort_shrink_indexes_by_bytes_saved();
            let first = &infos.all_infos[infos.shrink_indexes[0]];
            let second = &infos.all_infos[infos.shrink_indexes[1]];
            let first_capacity = first.written_bytes - first.alive_bytes;
            let second_capacity = second.written_bytes - second.alive_bytes;
            assert!(first_capacity >= second_capacity);
        }
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_combine_ancient_slots_packed_internal(accounts_db_config: AccountsDbConfig) {
        let alive = true;
        for num_slots in 0..4 {
            for max_ancient_slots in 0..4 {
                let db =
                    AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
                let slot1 = 1;
                create_storages_and_update_index(&db, slot1, num_slots, alive, None);
                let original_stores = (0..num_slots)
                    .filter_map(|slot| db.storage.get_slot_storage_entry((slot as Slot) + slot1))
                    .collect::<Vec<_>>();
                let original_results = original_stores
                    .iter()
                    .map(|store| (store.slot(), db.get_unique_accounts_from_storage(store)))
                    .collect::<Vec<_>>();
                let original_results_all_accounts = vec_unique_to_accounts(&original_results, &db);

                let tuning = PackedAncientStorageTuning {
                    max_ancient_slots,
                    ideal_storage_size: NonZeroU64::new(get_ancient_append_vec_capacity()).unwrap(),
                    ..default_tuning()
                };
                db.combine_ancient_slots_packed_internal(
                    (0..num_slots).map(|slot| (slot as Slot) + slot1).collect(),
                    tuning,
                    &mut SquashStatsSub::default(),
                );
                let storage = db.storage.get_slot_storage_entry(slot1);
                if num_slots == 0 {
                    assert!(storage.is_none());
                    continue;
                }
                // any of the several slots could have been chosen to be reused
                let active_slots = (0..num_slots)
                    .filter_map(|slot| db.storage.get_slot_storage_entry((slot as Slot) + slot1))
                    .count();
                let mut expected_slots = (max_ancient_slots / 2).min(num_slots);
                if max_ancient_slots >= num_slots {
                    expected_slots = num_slots;
                } else if max_ancient_slots == 0 || num_slots > 0 && expected_slots == 0 {
                    expected_slots = 1;
                }
                assert_eq!(
                    active_slots, expected_slots,
                    "slots: {num_slots}, max_ancient_slots: {max_ancient_slots}, alive: {alive}"
                );
                assert_eq!(
                    expected_slots,
                    db.storage.all_slots().len(),
                    "slots: {num_slots}, max_ancient_slots: {max_ancient_slots}"
                );

                let stores = (0..num_slots)
                    .filter_map(|slot| db.storage.get_slot_storage_entry((slot as Slot) + slot1))
                    .collect::<Vec<_>>();
                let results = stores
                    .iter()
                    .map(|store| (store.slot(), db.get_unique_accounts_from_storage(store)))
                    .collect::<Vec<_>>();
                let all_accounts = get_all_accounts(&db, slot1..(slot1 + num_slots as Slot));
                compare_all_accounts(&original_results_all_accounts, &all_accounts);
                compare_all_accounts(
                    &vec_unique_to_accounts(&results, &db),
                    &get_all_accounts(&db, slot1..(slot1 + num_slots as Slot)),
                );
            }
        }
    }

    fn vec_unique_to_accounts(
        one: &[(Slot, GetUniqueAccountsResult)],
        db: &AccountsDb,
    ) -> Vec<(Pubkey, AccountSharedData)> {
        one.iter()
            .flat_map(|(slot, result)| {
                result.stored_accounts.iter().map(|result| {
                    (
                        *result.pubkey(),
                        get_account_from_account_from_storage(result, db, *slot),
                    )
                })
            })
            .collect()
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_combine_packed_ancient_slots_simple(accounts_db_config: AccountsDbConfig) {
        for alive in [false, true] {
            let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config.clone());
            let slot1 = 1;
            let num_normal_slots = 0;
            create_storages_and_update_index(&db, slot1, num_normal_slots + 1, alive, None);
            let storage = db.storage.get_slot_storage_entry(slot1).unwrap();
            let created_accounts = db.get_unique_accounts_from_storage(&storage);

            db.combine_ancient_slots_packed(vec![slot1]);
            assert!(db.storage.get_slot_storage_entry(slot1).is_some());
            let after_store = db.storage.get_slot_storage_entry(slot1).unwrap();
            let GetUniqueAccountsResult {
                stored_accounts: after_stored_accounts,
                written_bytes: after_written_bytes,
                ..
            } = db.get_unique_accounts_from_storage(&after_store);
            assert_eq!(created_accounts.written_bytes, after_written_bytes);
            assert_eq!(created_accounts.stored_accounts.len(), 1);
            // always 1 account: either we leave the append vec alone if it is all dead
            // or we create a new one and copy into it if account is alive
            assert_eq!(after_stored_accounts.len(), 1);
        }
    }

    /// combines ALL possible slots in `sorted_slots`
    fn combine_ancient_slots_packed_for_tests(db: &AccountsDb, sorted_slots: Vec<Slot>) {
        // combine normal append vec(s) into packed ancient append vec
        let tuning = PackedAncientStorageTuning {
            max_ancient_slots: 0,
            ideal_storage_size: NonZeroU64::new(get_ancient_append_vec_capacity()).unwrap(),
            ..default_tuning()
        };

        let mut stats_sub = SquashStatsSub::default();
        db.combine_ancient_slots_packed_internal(sorted_slots, tuning, &mut stats_sub);
    }

    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_shrink_packed_ancient(accounts_db_config: AccountsDbConfig) {
        // NOTE: The recycler has been removed.  Creating this many extra storages is no longer
        // necessary, but also does no harm either.
        const MAX_RECYCLE_STORES: usize = 1000;
        // When we pack ancient append vecs, the packed append vecs are recycled first if possible. This means they aren't dropped directly.
        // This test tests that we are releasing Arc refcounts for storages when we pack them into ancient append vecs.
        let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config);
        let initial_slot = 0;
        // create append vecs that we'll fill the recycler with when we pack them into 1 packed append vec
        create_storages_and_update_index(&db, initial_slot, MAX_RECYCLE_STORES, true, None);
        let max_slot_inclusive = initial_slot + (MAX_RECYCLE_STORES as Slot) - 1;
        let range = initial_slot..(max_slot_inclusive + 1);
        // storages with Arc::strong_count > 1 cannot be pulled out of the
        // recycling bin, so hold refcounts so these storages are never reused
        // by the actual test code
        let _storages_hold_to_prevent_recycling = range
            .filter_map(|slot| db.storage.get_slot_storage_entry(slot))
            .collect::<Vec<_>>();

        // fill up the recycler with storages
        combine_ancient_slots_packed_for_tests(&db, (initial_slot..=max_slot_inclusive).collect());

        let mut starting_slot = max_slot_inclusive + 1;
        for num_normal_slots in 1..4 {
            let mut storages = vec![];
            // build an ancient append vec at slot 'ancient_slot'
            let ancient_slot = starting_slot;
            create_storages_and_update_index(&db, ancient_slot, num_normal_slots, true, None);
            let max_slot_inclusive = ancient_slot + (num_normal_slots as Slot);
            let range = ancient_slot..(max_slot_inclusive + 1);
            storages.extend(
                range
                    .clone()
                    .filter_map(|slot| db.storage.get_slot_storage_entry(slot)),
            );

            let initial_accounts = get_all_accounts(&db, range);
            compare_all_accounts(
                &initial_accounts,
                &get_all_accounts(&db, ancient_slot..(max_slot_inclusive + 1)),
            );

            combine_ancient_slots_packed_for_tests(
                &db,
                (ancient_slot..=max_slot_inclusive).collect(),
            );

            compare_all_accounts(
                &initial_accounts,
                &get_all_accounts(&db, ancient_slot..(max_slot_inclusive + 1)),
            );
            // verify only `storages` is holding a refcount to each storage we packed
            storages
                .iter()
                .for_each(|storage| assert_eq!(Arc::strong_count(storage), 1));

            // create a 2nd ancient append vec at 'next_slot'
            let next_slot = max_slot_inclusive + 1;
            create_storages_and_update_index(&db, next_slot, num_normal_slots, true, None);
            let max_slot_inclusive = next_slot + (num_normal_slots as Slot);
            let range_all = ancient_slot..(max_slot_inclusive + 1);
            let range = next_slot..(max_slot_inclusive + 1);
            storages = vec![];
            storages.extend(
                range
                    .clone()
                    .filter_map(|slot| db.storage.get_slot_storage_entry(slot)),
            );
            let initial_accounts_all = get_all_accounts(&db, range_all.clone());
            let initial_accounts = get_all_accounts(&db, range.clone());
            compare_all_accounts(
                &initial_accounts_all,
                &get_all_accounts(&db, range_all.clone()),
            );
            compare_all_accounts(&initial_accounts, &get_all_accounts(&db, range.clone()));

            combine_ancient_slots_packed_for_tests(&db, range.clone().collect());

            compare_all_accounts(&initial_accounts_all, &get_all_accounts(&db, range_all));
            compare_all_accounts(&initial_accounts, &get_all_accounts(&db, range));

            // verify only `storages` is holding a refcount to each storage we packed
            storages
                .iter()
                .for_each(|storage| assert_eq!(Arc::strong_count(storage), 1));

            starting_slot = max_slot_inclusive + 1;
        }
    }

    /// The purpose of this test is to ensure the correct control flow
    /// of calculating and using the value of the tuning parameter
    /// `ideal_storage_size`.
    #[test_case(ACCOUNTS_DB_CONFIG_APPEND_VEC)]
    fn test_ideal_storage_size_updated_before_used(accounts_db_config: AccountsDbConfig) {
        let mut tuning = PackedAncientStorageTuning {
            max_ancient_slots: 100,
            ..default_tuning()
        };
        let data_size = 1_000_000;
        let num_slots = tuning.max_ancient_slots;
        let db = AccountsDb::new_for_tests_with_config(Vec::new(), accounts_db_config);
        let slot1 = 1;
        let alive = true;
        create_storages_and_update_index(&db, slot1, num_slots, alive, Some(data_size));
        let non_ancient_slot = slot1 + (2 * tuning.max_ancient_slots) as u64;
        create_storages_and_update_index(&db, non_ancient_slot, 1, true, Some(data_size));
        let mut slot_vec = (slot1..(slot1 + num_slots as Slot)).collect::<Vec<_>>();
        slot_vec.push(non_ancient_slot);
        for slot in &slot_vec {
            // reduce the storage's alive bytes to ensure it is a shrink candidate
            db.storage
                .get_slot_storage_entry(*slot)
                .unwrap()
                .num_alive_bytes
                .fetch_sub(1, Ordering::Release);
        }
        let infos = db.collect_sort_filter_ancient_slots(slot_vec.clone(), &mut tuning);
        let ideal_storage_size = tuning.ideal_storage_size.get();
        let max_resulting_storages = tuning.max_resulting_storages.get();
        let expected_all_infos_len = max_resulting_storages * ideal_storage_size / data_size;
        assert_eq!(infos.all_infos.len(), expected_all_infos_len as usize);
    }
}
