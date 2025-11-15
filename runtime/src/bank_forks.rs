//! The `bank_forks` module implements BankForks a DAG of checkpointed Banks

use {
    crate::{
        bank::{bank_hash_details, Bank, SquashTiming},
        bank_hash_cache::DumpedSlotSubscription,
        installed_scheduler_pool::{
            BankWithScheduler, InstalledSchedulerPoolArc, SchedulingContext,
        },
        snapshot_controller::SnapshotController,
    },
    arc_swap::ArcSwap,
    log::*,
    solana_clock::{BankId, Slot},
    solana_hash::Hash,
    solana_measure::measure::Measure,
    solana_program_runtime::loaded_programs::{BlockRelation, ForkGraph},
    solana_unified_scheduler_logic::SchedulingMode,
    std::{
        collections::{hash_map::Entry, HashMap, HashSet},
        ops::Index,
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc, RwLock,
        },
        time::Instant,
    },
};

pub const MAX_ROOT_DISTANCE_FOR_VOTE_ONLY: Slot = 400;
pub type AtomicSlot = AtomicU64;
#[derive(Clone)]
pub struct ReadOnlyAtomicSlot {
    slot: Arc<AtomicSlot>,
}

impl ReadOnlyAtomicSlot {
    pub fn get(&self) -> Slot {
        // The expectation is that an instance `ReadOnlyAtomicSlot` is on a different thread than
        // BankForks *and* this instance is being accessed *without* locking BankForks first.
        // Thus, to ensure atomic ordering correctness, we must use Acquire-Release semantics.
        self.slot.load(Ordering::Acquire)
    }
}

/// Convenience type since often root/working banks are fetched together.
#[derive(Clone)]
pub struct SharableBanks {
    root_bank: Arc<ArcSwap<Bank>>,
    working_bank: Arc<ArcSwap<Bank>>,
}

impl SharableBanks {
    pub fn root(&self) -> Arc<Bank> {
        self.root_bank.load_full()
    }

    pub fn working(&self) -> Arc<Bank> {
        self.working_bank.load_full()
    }

    pub fn load(&self) -> BankPair {
        BankPair {
            root_bank: self.root(),
            working_bank: self.working(),
        }
    }
}

pub struct BankPair {
    pub root_bank: Arc<Bank>,
    pub working_bank: Arc<Bank>,
}

#[derive(Debug, Default, Copy, Clone)]
struct SetRootMetrics {
    timings: SetRootTimings,
    total_parent_banks: i64,
    tx_count: i64,
    dropped_banks_len: i64,
    accounts_data_len: i64,
}

#[derive(Debug, Default, Copy, Clone)]
struct SetRootTimings {
    total_squash_time: SquashTiming,
    total_snapshot_ms: i64,
    prune_non_rooted_ms: i64,
    drop_parent_banks_ms: i64,
    prune_slots_ms: i64,
    prune_remove_ms: i64,
}

pub struct BankForks {
    banks: HashMap<Slot, BankWithScheduler>,
    descendants: HashMap<Slot, HashSet<Slot>>,
    root: Arc<AtomicSlot>,
    working_slot: Slot,
    sharable_banks: SharableBanks,
    in_vote_only_mode: Arc<AtomicBool>,
    highest_slot_at_startup: Slot,
    scheduler_pool: Option<InstalledSchedulerPoolArc>,
    dumped_slot_subscribers: Vec<DumpedSlotSubscription>,
}

impl Index<u64> for BankForks {
    type Output = Arc<Bank>;
    fn index(&self, bank_slot: Slot) -> &Self::Output {
        &self.banks[&bank_slot]
    }
}

impl BankForks {
    pub fn new_rw_arc(root_bank: Bank) -> Arc<RwLock<Self>> {
        let root_bank = Arc::new(root_bank);
        let root_slot = root_bank.slot();

        let mut banks = HashMap::new();
        banks.insert(
            root_slot,
            BankWithScheduler::new_without_scheduler(root_bank.clone()),
        );

        let parents = root_bank.parents();
        for parent in parents {
            if banks
                .insert(
                    parent.slot(),
                    BankWithScheduler::new_without_scheduler(parent.clone()),
                )
                .is_some()
            {
                // All ancestors have already been inserted by another fork
                break;
            }
        }

        let mut descendants = HashMap::<_, HashSet<_>>::new();
        descendants.entry(root_slot).or_default();
        for parent in root_bank.proper_ancestors() {
            descendants.entry(parent).or_default().insert(root_slot);
        }

        let bank_forks = Arc::new(RwLock::new(Self {
            root: Arc::new(AtomicSlot::new(root_slot)),
            working_slot: root_slot,
            sharable_banks: SharableBanks {
                root_bank: Arc::new(ArcSwap::from(root_bank.clone())),
                // working bank is initially the same as root - all banks are either the root
                // or its ancestors.
                working_bank: Arc::new(ArcSwap::from(root_bank.clone())),
            },
            banks,
            descendants,
            in_vote_only_mode: Arc::new(AtomicBool::new(false)),
            highest_slot_at_startup: 0,
            scheduler_pool: None,
            dumped_slot_subscribers: vec![],
        }));

        root_bank.set_fork_graph_in_program_cache(Arc::downgrade(&bank_forks));
        bank_forks
    }

    pub fn banks(&self) -> &HashMap<Slot, BankWithScheduler> {
        &self.banks
    }

    pub fn get_vote_only_mode_signal(&self) -> Arc<AtomicBool> {
        self.in_vote_only_mode.clone()
    }

    pub fn len(&self) -> usize {
        self.banks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.banks.is_empty()
    }

    /// Create a map of bank slot id to the set of ancestors for the bank slot.
    pub fn ancestors(&self) -> HashMap<Slot, HashSet<Slot>> {
        let root = self.root();
        self.banks
            .iter()
            .map(|(slot, bank)| {
                let ancestors = bank.proper_ancestors().filter(|k| *k >= root);
                (*slot, ancestors.collect())
            })
            .collect()
    }

    /// Create a map of bank slot id to the set of all of its descendants
    pub fn descendants(&self) -> HashMap<Slot, HashSet<Slot>> {
        self.descendants.clone()
    }

    pub fn frozen_banks(&self) -> impl Iterator<Item = (Slot, Arc<Bank>)> + '_ {
        self.banks
            .iter()
            .filter(|(_, b)| b.is_frozen())
            .map(|(&k, b)| (k, b.clone_without_scheduler()))
    }

    pub fn active_bank_slots(&self) -> Vec<Slot> {
        self.banks
            .iter()
            .filter(|(_, v)| !v.is_frozen())
            .map(|(k, _v)| *k)
            .collect()
    }

    pub fn get_with_scheduler(&self, bank_slot: Slot) -> Option<BankWithScheduler> {
        self.banks.get(&bank_slot).map(|b| b.clone_with_scheduler())
    }

    pub fn get(&self, bank_slot: Slot) -> Option<Arc<Bank>> {
        self.get_with_scheduler(bank_slot)
            .map(|b| b.clone_without_scheduler())
    }

    pub fn get_with_checked_hash(
        &self,
        (bank_slot, expected_hash): (Slot, Hash),
    ) -> Option<Arc<Bank>> {
        let maybe_bank = self.get(bank_slot);
        if let Some(bank) = &maybe_bank {
            assert_eq!(bank.hash(), expected_hash);
        }
        maybe_bank
    }

    pub fn bank_hash(&self, slot: Slot) -> Option<Hash> {
        self.get(slot).map(|bank| bank.hash())
    }

    pub fn sharable_banks(&self) -> SharableBanks {
        self.sharable_banks.clone()
    }

    pub fn root_bank(&self) -> Arc<Bank> {
        self.sharable_banks.root()
    }

    pub fn install_scheduler_pool(&mut self, pool: InstalledSchedulerPoolArc) {
        info!("Installed new scheduler_pool into bank_forks: {pool:?}");
        assert!(
            self.scheduler_pool.replace(pool).is_none(),
            "Reinstalling scheduler pool isn't supported"
        );
    }

    pub fn insert(&mut self, bank: Bank) -> BankWithScheduler {
        self.insert_with_scheduling_mode(SchedulingMode::BlockVerification, bank)
    }

    pub fn insert_with_scheduling_mode(
        &mut self,
        mode: SchedulingMode,
        mut bank: Bank,
    ) -> BankWithScheduler {
        if self.root.load(Ordering::Relaxed) < self.highest_slot_at_startup {
            bank.set_check_program_modification_slot(true);
        }

        let bank = Arc::new(bank);
        let bank = if let Some(scheduler_pool) = &self.scheduler_pool {
            Self::install_scheduler_into_bank(scheduler_pool, mode, bank)
        } else {
            BankWithScheduler::new_without_scheduler(bank)
        };
        let prev = self.banks.insert(bank.slot(), bank.clone_with_scheduler());
        assert!(prev.is_none());
        let slot = bank.slot();
        self.descendants.entry(slot).or_default();
        for parent in bank.proper_ancestors() {
            self.descendants.entry(parent).or_default().insert(slot);
        }

        // Update sharable working bank and cached slot.
        self.working_slot = self.find_highest_slot();
        self.sharable_banks.working_bank.store(self.working_bank());

        bank
    }

    fn install_scheduler_into_bank(
        scheduler_pool: &InstalledSchedulerPoolArc,
        mode: SchedulingMode,
        bank: Arc<Bank>,
    ) -> BankWithScheduler {
        let context = SchedulingContext::new_with_mode(mode, bank.clone());
        let scheduler = scheduler_pool.take_scheduler(context);
        let bank_with_scheduler = BankWithScheduler::new(bank, Some(scheduler));
        // Skip registering for block production. Both the tvu main loop in the replay stage
        // and PohRecorder don't support _concurrent block production_ at all. It's strongly
        // assumed that block is produced in singleton way and it's actually desired, while
        // ignoring the opportunity cost of (hopefully rare!) fork switching...
        if matches!(mode, SchedulingMode::BlockVerification) {
            scheduler_pool.register_timeout_listener(bank_with_scheduler.create_timeout_listener());
        }
        bank_with_scheduler
    }

    pub fn insert_from_ledger(&mut self, bank: Bank) -> BankWithScheduler {
        self.highest_slot_at_startup = std::cmp::max(self.highest_slot_at_startup, bank.slot());
        self.insert(bank)
    }

    pub fn remove(&mut self, slot: Slot) -> Option<BankWithScheduler> {
        let bank = self.banks.remove(&slot)?;
        for parent in bank.proper_ancestors() {
            let Entry::Occupied(mut entry) = self.descendants.entry(parent) else {
                panic!("this should not happen!");
            };
            entry.get_mut().remove(&slot);
            if entry.get().is_empty() && !self.banks.contains_key(&parent) {
                entry.remove_entry();
            }
        }
        let Entry::Occupied(entry) = self.descendants.entry(slot) else {
            panic!("this should not happen!");
        };
        if entry.get().is_empty() {
            entry.remove_entry();
        }

        // Update sharable working bank and cached slot.
        // The previous working bank (highest slot) may have been removed.
        self.working_slot = self.find_highest_slot();
        self.sharable_banks.working_bank.store(self.working_bank());

        Some(bank)
    }

    pub fn highest_slot(&self) -> Slot {
        self.working_slot
    }

    fn find_highest_slot(&self) -> Slot {
        self.banks.values().map(|bank| bank.slot()).max().unwrap()
    }

    pub fn working_bank(&self) -> Arc<Bank> {
        self.banks[&self.highest_slot()].clone_without_scheduler()
    }

    pub fn working_bank_with_scheduler(&self) -> BankWithScheduler {
        self.banks[&self.highest_slot()].clone_with_scheduler()
    }

    /// Register to be notified when a bank has been dumped (due to duplicate block handling)
    /// from bank_forks.
    pub fn register_dumped_slot_subscriber(&mut self, notifier: DumpedSlotSubscription) {
        self.dumped_slot_subscribers.push(notifier);
    }

    /// Clears associated banks from BankForks and notifies subscribers that a dump has occurred.
    pub fn dump_slots<'a, I>(&mut self, slots: I) -> (Vec<(Slot, BankId)>, Vec<BankWithScheduler>)
    where
        I: Iterator<Item = &'a Slot>,
    {
        // Notify subscribers. It is fine that the lock is immediately released, since the bank_forks
        // lock is held until the end of this function, so subscribers will not be able to interact
        // with bank_forks anyway.
        for subscriber in &self.dumped_slot_subscribers {
            let mut lock = subscriber.lock().unwrap();
            *lock = true;
        }

        slots
            .map(|slot| {
                // Clear the banks from BankForks
                let bank = self
                    .remove(*slot)
                    .expect("BankForks should not have been purged yet");
                bank_hash_details::write_bank_hash_details_file(&bank)
                    .map_err(|err| {
                        warn!("Unable to write bank hash details file: {err}");
                    })
                    .ok();
                ((*slot, bank.bank_id()), bank)
            })
            .unzip()
    }

    fn do_set_root_return_metrics(
        &mut self,
        root: Slot,
        snapshot_controller: Option<&SnapshotController>,
        highest_super_majority_root: Option<Slot>,
    ) -> (Vec<BankWithScheduler>, SetRootMetrics) {
        let old_epoch = self.sharable_banks.root().epoch();

        let root_bank = &self
            .get(root)
            .expect("root bank didn't exist in bank_forks");

        // To support `RootBankCache` (via `ReadOnlyAtomicSlot`) accessing `root` *without* locking
        // BankForks first *and* from a different thread, this store *must* be at least Release to
        // ensure atomic ordering correctness.
        self.root.store(root, Ordering::Release);
        self.sharable_banks.root_bank.store(Arc::clone(root_bank));

        let new_epoch = root_bank.epoch();
        if old_epoch != new_epoch {
            info!(
                "Root entering epoch: {new_epoch}, next_epoch_start_slot: {}, epoch_stakes: {:#?}",
                root_bank
                    .epoch_schedule()
                    .get_first_slot_in_epoch(new_epoch + 1),
                root_bank
                    .epoch_stakes(new_epoch)
                    .unwrap()
                    .node_id_to_vote_accounts()
            );
            // Now we have rooted a bank in a new epoch, there are no needs to
            // keep the epoch rewards cache for current epoch any longer.
            info!(
                "Clearing epoch rewards cache for epoch {old_epoch} after setting root to slot \
                 {root}"
            );
            root_bank.clear_epoch_rewards_cache();
        }
        let root_tx_count = root_bank
            .parents()
            .last()
            .map(|bank| bank.transaction_count())
            .unwrap_or(0);
        // Calculate the accounts hash at a fixed interval
        let mut banks = vec![root_bank];
        let parents = root_bank.parents();
        banks.extend(parents.iter());
        let total_parent_banks = banks.len();
        let (is_root_bank_squashed, mut squash_timing, total_snapshot_ms) =
            if let Some(snapshot_controller) = snapshot_controller {
                snapshot_controller.handle_new_roots(root, &banks)
            } else {
                (false, SquashTiming::default(), 0)
            };

        if !is_root_bank_squashed {
            squash_timing += root_bank.squash();
        }
        let new_tx_count = root_bank.transaction_count();
        let accounts_data_len = root_bank.load_accounts_data_size() as i64;
        let mut prune_time = Measure::start("set_root::prune");
        let (removed_banks, prune_slots_ms, prune_remove_ms) =
            self.prune_non_rooted(root, highest_super_majority_root);
        prune_time.stop();
        let dropped_banks_len = removed_banks.len();

        let mut drop_parent_banks_time = Measure::start("set_root::drop_banks");
        drop(parents);
        drop_parent_banks_time.stop();

        (
            removed_banks,
            SetRootMetrics {
                timings: SetRootTimings {
                    total_squash_time: squash_timing,
                    total_snapshot_ms: total_snapshot_ms as i64,
                    prune_non_rooted_ms: prune_time.as_ms() as i64,
                    drop_parent_banks_ms: drop_parent_banks_time.as_ms() as i64,
                    prune_slots_ms: prune_slots_ms as i64,
                    prune_remove_ms: prune_remove_ms as i64,
                },
                total_parent_banks: total_parent_banks as i64,
                tx_count: (new_tx_count - root_tx_count) as i64,
                dropped_banks_len: dropped_banks_len as i64,
                accounts_data_len,
            },
        )
    }

    pub fn prune_program_cache(&self, root: Slot) {
        if let Some(root_bank) = self.banks.get(&root) {
            root_bank.prune_program_cache(root, root_bank.epoch());
        }
    }

    pub fn set_root(
        &mut self,
        root: Slot,
        snapshot_controller: Option<&SnapshotController>,
        highest_super_majority_root: Option<Slot>,
    ) -> Vec<BankWithScheduler> {
        let program_cache_prune_start = Instant::now();
        let set_root_start = Instant::now();
        let (removed_banks, set_root_metrics) =
            self.do_set_root_return_metrics(root, snapshot_controller, highest_super_majority_root);
        datapoint_info!(
            "bank-forks_set_root",
            (
                "elapsed_ms",
                set_root_start.elapsed().as_millis() as usize,
                i64
            ),
            ("slot", root, i64),
            (
                "total_parent_banks",
                set_root_metrics.total_parent_banks,
                i64
            ),
            ("total_banks", self.banks.len(), i64),
            (
                "total_squash_cache_ms",
                set_root_metrics.timings.total_squash_time.squash_cache_ms,
                i64
            ),
            (
                "total_squash_accounts_ms",
                set_root_metrics
                    .timings
                    .total_squash_time
                    .squash_accounts_ms,
                i64
            ),
            (
                "total_squash_accounts_index_ms",
                set_root_metrics
                    .timings
                    .total_squash_time
                    .squash_accounts_index_ms,
                i64
            ),
            (
                "total_squash_accounts_cache_ms",
                set_root_metrics
                    .timings
                    .total_squash_time
                    .squash_accounts_cache_ms,
                i64
            ),
            (
                "total_snapshot_ms",
                set_root_metrics.timings.total_snapshot_ms,
                i64
            ),
            ("tx_count", set_root_metrics.tx_count, i64),
            (
                "prune_non_rooted_ms",
                set_root_metrics.timings.prune_non_rooted_ms,
                i64
            ),
            (
                "drop_parent_banks_ms",
                set_root_metrics.timings.drop_parent_banks_ms,
                i64
            ),
            (
                "prune_slots_ms",
                set_root_metrics.timings.prune_slots_ms,
                i64
            ),
            (
                "prune_remove_ms",
                set_root_metrics.timings.prune_remove_ms,
                i64
            ),
            (
                "program_cache_prune_ms",
                program_cache_prune_start.elapsed().as_millis() as i64,
                i64
            ),
            ("dropped_banks_len", set_root_metrics.dropped_banks_len, i64),
            ("accounts_data_len", set_root_metrics.accounts_data_len, i64),
        );
        removed_banks
    }

    pub fn root(&self) -> Slot {
        self.root.load(Ordering::Relaxed)
    }

    /// Gets a read-only wrapper to an atomic slot holding the root slot.
    pub fn get_atomic_root(&self) -> ReadOnlyAtomicSlot {
        ReadOnlyAtomicSlot {
            slot: self.root.clone(),
        }
    }

    /// After setting a new root, prune the banks that are no longer on rooted paths
    ///
    /// Given the following banks and slots...
    ///
    /// ```text
    /// slot 6                   * (G)
    ///                         /
    /// slot 5        (F)  *   /
    ///                    |  /
    /// slot 4    (E) *    | /
    ///               |    |/
    /// slot 3        |    * (D) <-- root, from set_root()
    ///               |    |
    /// slot 2    (C) *    |
    ///                \   |
    /// slot 1          \  * (B)
    ///                  \ |
    /// slot 0             * (A)  <-- highest confirmed root [1]
    /// ```
    ///
    /// ...where (D) is set as root, clean up (C) and (E), since they are not rooted.
    ///
    /// (A) is kept because it is greater-than-or-equal-to the highest confirmed root, and (D) is
    ///     one of its descendants
    /// (B) is kept for the same reason as (A)
    /// (C) is pruned since it is a lower slot than (D), but (D) is _not_ one of its descendants
    /// (D) is kept since it is the root
    /// (E) is pruned since it is not a descendant of (D)
    /// (F) is kept since it is a descendant of (D)
    /// (G) is kept for the same reason as (F)
    ///
    /// and in table form...
    ///
    /// ```text
    ///       |          |  is root a  | is a descendant ||
    ///  slot | is root? | descendant? |    of root?     || keep?
    /// ------+----------+-------------+-----------------++-------
    ///   (A) |     N    |      Y      |        N        ||   Y
    ///   (B) |     N    |      Y      |        N        ||   Y
    ///   (C) |     N    |      N      |        N        ||   N
    ///   (D) |     Y    |      N      |        N        ||   Y
    ///   (E) |     N    |      N      |        N        ||   N
    ///   (F) |     N    |      N      |        Y        ||   Y
    ///   (G) |     N    |      N      |        Y        ||   Y
    /// ```
    ///
    /// [1] RPC has the concept of commitment level, which is based on the highest confirmed root,
    /// i.e. the cluster-confirmed root.  This commitment is stronger than the local node's root.
    /// So (A) and (B) are kept to facilitate RPC at different commitment levels.  Everything below
    /// the highest confirmed root can be pruned.
    fn prune_non_rooted(
        &mut self,
        root: Slot,
        highest_super_majority_root: Option<Slot>,
    ) -> (Vec<BankWithScheduler>, u64, u64) {
        // We want to collect timing separately, and the 2nd collect requires
        // a unique borrow to self which is already borrowed by self.banks
        let mut prune_slots_time = Measure::start("prune_slots");
        let highest_super_majority_root = highest_super_majority_root.unwrap_or(root);
        let prune_slots: Vec<_> = self
            .banks
            .keys()
            .copied()
            .filter(|slot| {
                let keep = *slot == root
                    || self.descendants[&root].contains(slot)
                    || (*slot < root
                        && *slot >= highest_super_majority_root
                        && self.descendants[slot].contains(&root));
                !keep
            })
            .collect();
        prune_slots_time.stop();

        let mut prune_remove_time = Measure::start("prune_slots");
        let removed_banks = prune_slots
            .into_iter()
            .filter_map(|slot| self.remove(slot))
            .collect();
        prune_remove_time.stop();

        (
            removed_banks,
            prune_slots_time.as_ms(),
            prune_remove_time.as_ms(),
        )
    }
}

impl ForkGraph for BankForks {
    fn relationship(&self, a: Slot, b: Slot) -> BlockRelation {
        let known_slot_range = self.root()..=self.highest_slot();
        if known_slot_range.contains(&a) && known_slot_range.contains(&b) {
            {
                (a == b)
                    .then_some(BlockRelation::Equal)
                    .or_else(|| {
                        self.banks.get(&b).and_then(|bank| {
                            bank.ancestors
                                .contains_key(&a)
                                .then_some(BlockRelation::Ancestor)
                        })
                    })
                    .or_else(|| {
                        self.descendants.get(&b).and_then(|slots| {
                            slots.contains(&a).then_some(BlockRelation::Descendant)
                        })
                    })
                    .unwrap_or(BlockRelation::Unrelated)
            }
        } else {
            BlockRelation::Unknown
        }
    }
}

impl Drop for BankForks {
    fn drop(&mut self) {
        info!("BankForks::drop(): started...");
        self.banks.clear();

        if let Some(scheduler_pool) = self.scheduler_pool.take() {
            scheduler_pool.uninstalled_from_bank_forks();
        }
        info!("BankForks::drop(): ...finished");
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank::test_utils::update_vote_account_timestamp,
            genesis_utils::{
                create_genesis_config, create_genesis_config_with_leader, GenesisConfigInfo,
            },
        },
        assert_matches::assert_matches,
        solana_clock::UnixTimestamp,
        solana_epoch_schedule::EpochSchedule,
        solana_keypair::Keypair,
        solana_pubkey::Pubkey,
        solana_signer::Signer,
        solana_vote_program::vote_state::BlockTimestamp,
    };

    // This test verifies that BankForks::new_rw_arc() doesn't create a reference cycle.
    //
    // Before PR #1893, there was a cycle:
    //   Arc<RwLock<BankForks>> → Bank → ProgramCache → Arc<RwLock<BankForks>>
    //
    // This happened because new_rw_arc() called:
    //   root_bank.set_fork_graph_in_program_cache(bank_forks.clone())
    //
    // The fix changed it to use a Weak reference:
    //   root_bank.set_fork_graph_in_program_cache(Arc::downgrade(&bank_forks))
    //
    // Breaking the cycle:
    //   Arc<RwLock<BankForks>> → Bank → ProgramCache → Weak<RwLock<BankForks>>
    //
    // Without the fix: strong_count == 2 (the clone creates an extra strong ref)
    // With the fix: strong_count == 1 (only the returned Arc holds a strong ref)
    #[test]
    fn test_bank_forks_new_rw_arc_memory_leak() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank_forks = BankForks::new_rw_arc(Bank::new_for_tests(&genesis_config));
        assert_eq!(Arc::strong_count(&bank_forks), 1);
    }

    #[test]
    fn test_bank_forks_new() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut bank_forks = bank_forks.write().unwrap();
        let child_bank = Bank::new_from_parent(bank_forks[0].clone(), &Pubkey::default(), 1);
        child_bank.register_default_tick_for_test();
        bank_forks.insert(child_bank);
        assert_eq!(bank_forks[1u64].tick_height(), 1);
        assert_eq!(bank_forks.working_bank().tick_height(), 1);
    }

    #[test]
    fn test_bank_forks_descendants() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut bank_forks = bank_forks.write().unwrap();
        let bank0 = bank_forks[0].clone();
        let bank = Bank::new_from_parent(bank0.clone(), &Pubkey::default(), 1);
        bank_forks.insert(bank);
        let bank = Bank::new_from_parent(bank0, &Pubkey::default(), 2);
        bank_forks.insert(bank);
        let descendants = bank_forks.descendants();
        let children: HashSet<u64> = [1u64, 2u64].iter().copied().collect();
        assert_eq!(children, *descendants.get(&0).unwrap());
        assert!(descendants[&1].is_empty());
        assert!(descendants[&2].is_empty());
    }

    #[test]
    fn test_bank_forks_ancestors() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut bank_forks = bank_forks.write().unwrap();
        let bank0 = bank_forks[0].clone();
        let bank = Bank::new_from_parent(bank0.clone(), &Pubkey::default(), 1);
        bank_forks.insert(bank);
        let bank = Bank::new_from_parent(bank0, &Pubkey::default(), 2);
        bank_forks.insert(bank);
        let ancestors = bank_forks.ancestors();
        assert!(ancestors[&0].is_empty());
        let parents: Vec<u64> = ancestors[&1].iter().cloned().collect();
        assert_eq!(parents, vec![0]);
        let parents: Vec<u64> = ancestors[&2].iter().cloned().collect();
        assert_eq!(parents, vec![0]);
    }

    #[test]
    fn test_bank_forks_frozen_banks() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut bank_forks = bank_forks.write().unwrap();
        let bank0 = bank_forks[0].clone();
        let child_bank = Bank::new_from_parent(bank0, &Pubkey::default(), 1);
        bank_forks.insert(child_bank);

        let frozen_slots: HashSet<Slot> = bank_forks
            .frozen_banks()
            .map(|(slot, _bank)| slot)
            .collect();
        assert!(frozen_slots.contains(&0));
        assert!(!frozen_slots.contains(&1));
    }

    #[test]
    fn test_bank_forks_active_banks() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut bank_forks = bank_forks.write().unwrap();
        let bank0 = bank_forks[0].clone();
        let child_bank = Bank::new_from_parent(bank0, &Pubkey::default(), 1);
        bank_forks.insert(child_bank);
        assert_eq!(bank_forks.active_bank_slots(), vec![1]);
    }

    #[test]
    fn test_bank_forks_different_set_root() {
        agave_logger::setup();
        let leader_keypair = Keypair::new();
        let GenesisConfigInfo {
            mut genesis_config,
            voting_keypair,
            ..
        } = create_genesis_config_with_leader(10_000, &leader_keypair.pubkey(), 1_000);
        let slots_in_epoch = 32;
        genesis_config.epoch_schedule = EpochSchedule::new(slots_in_epoch);

        let bank0 = Bank::new_for_tests(&genesis_config);
        let bank_forks0 = BankForks::new_rw_arc(bank0);
        let mut bank_forks0 = bank_forks0.write().unwrap();
        bank_forks0.set_root(0, None, None);

        let bank1 = Bank::new_for_tests(&genesis_config);
        let bank_forks1 = BankForks::new_rw_arc(bank1);
        let mut bank_forks1 = bank_forks1.write().unwrap();

        let additional_timestamp_secs = 2;

        let num_slots = slots_in_epoch + 1; // Advance past first epoch boundary
        for slot in 1..num_slots {
            // Just after the epoch boundary, timestamp a vote that will shift
            // Clock::unix_timestamp from Bank::unix_timestamp_from_genesis()
            let update_timestamp_case = slot == slots_in_epoch;

            let child1 =
                Bank::new_from_parent(bank_forks0[slot - 1].clone(), &Pubkey::default(), slot);
            let child2 =
                Bank::new_from_parent(bank_forks1[slot - 1].clone(), &Pubkey::default(), slot);

            if update_timestamp_case {
                for child in &[&child1, &child2] {
                    let recent_timestamp: UnixTimestamp = child.unix_timestamp_from_genesis();
                    update_vote_account_timestamp(
                        BlockTimestamp {
                            slot: child.slot(),
                            timestamp: recent_timestamp + additional_timestamp_secs,
                        },
                        child,
                        &voting_keypair.pubkey(),
                    );
                }
            }

            // Set root in bank_forks0 to truncate the ancestor history
            bank_forks0.insert(child1);
            bank_forks0.set_root(slot, None, None);

            // Don't set root in bank_forks1 to keep the ancestor history
            bank_forks1.insert(child2);
        }
        let child1 = &bank_forks0.working_bank();
        let child2 = &bank_forks1.working_bank();

        child1.freeze();
        child2.freeze();

        info!("child0.ancestors: {:?}", child1.ancestors);
        info!("child1.ancestors: {:?}", child2.ancestors);
        assert_eq!(child1.hash(), child2.hash());
    }

    fn make_hash_map(data: Vec<(Slot, Vec<Slot>)>) -> HashMap<Slot, HashSet<Slot>> {
        data.into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect()
    }

    fn extend_bank_forks(bank_forks: Arc<RwLock<BankForks>>, parent_child_pairs: &[(Slot, Slot)]) {
        for (parent, child) in parent_child_pairs.iter() {
            let parent: Arc<Bank> = bank_forks.read().unwrap().banks[parent].clone();
            bank_forks.write().unwrap().insert(Bank::new_from_parent(
                parent,
                &Pubkey::default(),
                *child,
            ));
        }
    }

    #[test]
    fn test_bank_forks_with_set_root() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);

        let parent_child_pairs = vec![(0, 1), (1, 2), (0, 3), (3, 4)];
        extend_bank_forks(bank_forks.clone(), &parent_child_pairs);

        assert_eq!(
            bank_forks.read().unwrap().ancestors(),
            make_hash_map(vec![
                (0, vec![]),
                (1, vec![0]),
                (2, vec![0, 1]),
                (3, vec![0]),
                (4, vec![0, 3]),
            ])
        );
        assert_eq!(
            bank_forks.read().unwrap().descendants(),
            make_hash_map(vec![
                (0, vec![1, 2, 3, 4]),
                (1, vec![2]),
                (2, vec![]),
                (3, vec![4]),
                (4, vec![]),
            ])
        );
        bank_forks.write().unwrap().set_root(
            2,    // root
            None, // snapshot_controller
            None, // highest confirmed root
        );
        bank_forks.read().unwrap().get(2).unwrap().squash();
        assert_eq!(
            bank_forks.read().unwrap().ancestors(),
            make_hash_map(vec![(2, vec![]),])
        );
        assert_eq!(
            bank_forks.read().unwrap().descendants(),
            make_hash_map(vec![(0, vec![2]), (1, vec![2]), (2, vec![]),])
        );

        let parent_child_pairs = vec![(2, 5), (5, 6)];
        extend_bank_forks(bank_forks.clone(), &parent_child_pairs);
        assert_eq!(
            bank_forks.read().unwrap().ancestors(),
            make_hash_map(vec![(2, vec![]), (5, vec![2]), (6, vec![2, 5])])
        );
        assert_eq!(
            bank_forks.read().unwrap().descendants(),
            make_hash_map(vec![
                (0, vec![2]),
                (1, vec![2]),
                (2, vec![5, 6]),
                (5, vec![6]),
                (6, vec![])
            ])
        );
    }

    #[test]
    fn test_bank_forks_with_highest_super_majority_root() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        assert_eq!(bank.slot(), 0);
        let bank_forks = BankForks::new_rw_arc(bank);

        let parent_child_pairs = vec![(0, 1), (1, 2), (0, 3), (3, 4)];
        extend_bank_forks(bank_forks.clone(), &parent_child_pairs);

        assert_eq!(
            bank_forks.read().unwrap().ancestors(),
            make_hash_map(vec![
                (0, vec![]),
                (1, vec![0]),
                (2, vec![0, 1]),
                (3, vec![0]),
                (4, vec![0, 3]),
            ])
        );
        assert_eq!(
            bank_forks.read().unwrap().descendants(),
            make_hash_map(vec![
                (0, vec![1, 2, 3, 4]),
                (1, vec![2]),
                (2, vec![]),
                (3, vec![4]),
                (4, vec![]),
            ])
        );
        bank_forks.write().unwrap().set_root(
            2,
            None,    // snapshot_controller
            Some(1), // highest confirmed root
        );
        bank_forks.read().unwrap().get(2).unwrap().squash();
        assert_eq!(
            bank_forks.read().unwrap().ancestors(),
            make_hash_map(vec![(1, vec![]), (2, vec![]),])
        );
        assert_eq!(
            bank_forks.read().unwrap().descendants(),
            make_hash_map(vec![(0, vec![1, 2]), (1, vec![2]), (2, vec![]),])
        );

        let parent_child_pairs = vec![(2, 5), (5, 6)];
        extend_bank_forks(bank_forks.clone(), &parent_child_pairs);
        assert_eq!(
            bank_forks.read().unwrap().ancestors(),
            make_hash_map(vec![
                (1, vec![]),
                (2, vec![]),
                (5, vec![2]),
                (6, vec![2, 5])
            ])
        );
        assert_eq!(
            bank_forks.read().unwrap().descendants(),
            make_hash_map(vec![
                (0, vec![1, 2]),
                (1, vec![2]),
                (2, vec![5, 6]),
                (5, vec![6]),
                (6, vec![])
            ])
        );
    }

    #[test]
    fn test_fork_graph() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);

        let parent_child_pairs = vec![
            (0, 1),
            (1, 3),
            (3, 8),
            (0, 2),
            (2, 4),
            (4, 5),
            (5, 10),
            (4, 6),
            (6, 12),
        ];
        extend_bank_forks(bank_forks.clone(), &parent_child_pairs);

        // Fork graph created for the test
        //                   0
        //                 /   \
        //                1     2
        //                |     |
        //                3     4
        //                |     | \
        //                8     5  6
        //                      |   |
        //                      10  12
        let mut bank_forks = bank_forks.write().unwrap();
        assert_matches!(bank_forks.relationship(0, 3), BlockRelation::Ancestor);
        assert_matches!(bank_forks.relationship(0, 10), BlockRelation::Ancestor);
        assert_matches!(bank_forks.relationship(0, 12), BlockRelation::Ancestor);
        assert_matches!(bank_forks.relationship(1, 3), BlockRelation::Ancestor);
        assert_matches!(bank_forks.relationship(2, 10), BlockRelation::Ancestor);
        assert_matches!(bank_forks.relationship(2, 12), BlockRelation::Ancestor);
        assert_matches!(bank_forks.relationship(4, 10), BlockRelation::Ancestor);
        assert_matches!(bank_forks.relationship(4, 12), BlockRelation::Ancestor);
        assert_matches!(bank_forks.relationship(6, 10), BlockRelation::Unrelated);
        assert_matches!(bank_forks.relationship(5, 12), BlockRelation::Unrelated);
        assert_matches!(bank_forks.relationship(6, 12), BlockRelation::Ancestor);

        assert_matches!(bank_forks.relationship(6, 2), BlockRelation::Descendant);
        assert_matches!(bank_forks.relationship(10, 2), BlockRelation::Descendant);
        assert_matches!(bank_forks.relationship(8, 3), BlockRelation::Descendant);
        assert_matches!(bank_forks.relationship(6, 3), BlockRelation::Unrelated);
        assert_matches!(bank_forks.relationship(12, 2), BlockRelation::Descendant);
        assert_matches!(bank_forks.relationship(12, 1), BlockRelation::Unrelated);
        assert_matches!(bank_forks.relationship(1, 2), BlockRelation::Unrelated);

        assert_matches!(bank_forks.relationship(1, 13), BlockRelation::Unknown);
        assert_matches!(bank_forks.relationship(13, 2), BlockRelation::Unknown);
        bank_forks.set_root(
            2,
            None,    // snapshot_controller
            Some(1), // highest confirmed root
        );
        assert_matches!(bank_forks.relationship(1, 2), BlockRelation::Unknown);
        assert_matches!(bank_forks.relationship(2, 0), BlockRelation::Unknown);
    }
}
