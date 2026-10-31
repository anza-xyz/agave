use {
    solana_clock::Slot,
    solana_hash::Hash,
    solana_ledger::blockstore_processor::{
        AsyncVerificationProgress, ConfirmationProgress, ReplaySlotStats,
    },
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    std::{
        collections::HashMap,
        sync::{Arc, RwLock},
    },
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeadSlotReason {
    /// Dead and cannot be brought back to life by an `UpdateParent` marker.
    Hard,
    /// Replay execution failed, but an `UpdateParent` marker could still make
    /// the failed prefix obsolete.
    ReplayFailureBeforeUpdateParent,
    /// Replay rejected the `UpdateParent` marker exactly at its FEC-set
    /// boundary. Restarting from the marker can discard the stale prefix state.
    ReplayFailureAtUpdateParent(u64),
}

pub struct ForkProgress {
    pub dead_reason: Option<DeadSlotReason>,
    pub bank_hash: Option<Hash>,
    pub replay_stats: Arc<RwLock<ReplaySlotStats>>,
    pub replay_progress: Arc<RwLock<ConfirmationProgress>>,
    // These counters cover only blocks replayed since the latest restart.
    pub num_blocks_on_fork: u64,
    pub num_dropped_blocks_on_fork: u64,
}

impl ForkProgress {
    pub fn new(
        last_entry: Hash,
        num_blocks_on_fork: u64,
        num_dropped_blocks_on_fork: u64,
        async_verification: Option<AsyncVerificationProgress>,
    ) -> Self {
        Self {
            dead_reason: None,
            bank_hash: None,
            replay_stats: Arc::new(RwLock::new(ReplaySlotStats::default())),
            replay_progress: Arc::new(RwLock::new(
                ConfirmationProgress::new_with_async_verification(last_entry, async_verification),
            )),
            num_blocks_on_fork,
            num_dropped_blocks_on_fork,
        }
    }

    pub fn new_from_bank(
        bank: &Bank,
        num_blocks_on_fork: u64,
        num_dropped_blocks_on_fork: u64,
        async_verification: Option<AsyncVerificationProgress>,
    ) -> Self {
        let mut progress = Self::new(
            bank.last_blockhash(),
            num_blocks_on_fork,
            num_dropped_blocks_on_fork,
            async_verification,
        );
        if bank.is_frozen() {
            progress.bank_hash = Some(bank.hash());
        }
        progress
    }

    pub fn mark_dead(&mut self, reason: DeadSlotReason) {
        self.dead_reason = Some(reason);
    }
}

#[derive(Default)]
pub struct ProgressMap {
    progress_map: HashMap<Slot, ForkProgress>,
    /// Number of times a slot was switched from an alternate location. This
    /// persists while the slot's previous progress entry is being replaced.
    bank_switch_counts: HashMap<Slot, u64>,
}

impl std::ops::Deref for ProgressMap {
    type Target = HashMap<Slot, ForkProgress>;

    fn deref(&self) -> &Self::Target {
        &self.progress_map
    }
}

impl std::ops::DerefMut for ProgressMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.progress_map
    }
}

impl ProgressMap {
    pub fn insert(&mut self, slot: Slot, fork_progress: ForkProgress) {
        fork_progress
            .replay_stats
            .write()
            .unwrap()
            .num_bank_switches = self.get_num_bank_switches(slot);
        self.progress_map.insert(slot, fork_progress);
    }

    pub fn increment_num_bank_switches(&mut self, slot: Slot) {
        let count = self.bank_switch_counts.entry(slot).or_insert(0);
        *count = count.saturating_add(1);
    }

    fn get_num_bank_switches(&self, slot: Slot) -> u64 {
        self.bank_switch_counts.get(&slot).copied().unwrap_or(0)
    }

    pub fn handle_new_root(&mut self, bank_forks: &BankForks) {
        self.progress_map
            .retain(|slot, _| bank_forks.get(*slot).is_some());
        self.bank_switch_counts
            .retain(|slot, _| bank_forks.get(*slot).is_some());
    }
}
