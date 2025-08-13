use {
    solana_clock::{DEFAULT_TICKS_PER_SLOT, HOLD_TRANSACTIONS_SLOT_OFFSET},
    solana_poh::poh_recorder::{
        PohRecorder, SharedLeaderFirstTickHeight, SharedTickHeight, SharedWorkingBank,
    },
    solana_runtime::bank::Bank,
    solana_unified_scheduler_pool::{BankingStageMonitor, BankingStageStatus},
    std::sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Arc,
    },
};

pub const HOLD_TRANSACTIONS_TICK_WINDOW: i64 =
    HOLD_TRANSACTIONS_SLOT_OFFSET as i64 * DEFAULT_TICKS_PER_SLOT as i64;

#[derive(Debug, Clone)]
pub enum LeaderStatus {
    /// Currently leader with the given bank.
    Active(Arc<Bank>),
    /// The number of ticks until the next leader slot.
    /// This may be negative if the leader tick is in the past, but we do not
    /// have a bank ready yet.
    TicksUntilLeader(i64),
    WillNotBeLeader,
}

impl LeaderStatus {
    /// Returns the `Bank` if the status is `Active`. Otherwise, returns `None`.
    pub fn bank(&self) -> Option<&Arc<Bank>> {
        match self {
            Self::Active(bank) => Some(bank),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct LeaderStatusMonitor {
    shared_working_bank: SharedWorkingBank,
    shared_tick_height: SharedTickHeight,
    shared_leader_first_tick_height: SharedLeaderFirstTickHeight,
}

impl std::fmt::Debug for LeaderStatusMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaderStatusMonitor")
            .field("shared_working_bank", &self.shared_working_bank.load())
            .field("shared_tick_height", &self.shared_tick_height.load())
            .field(
                "shared_leader_first_tick_height",
                &self.shared_leader_first_tick_height.load(),
            )
            .finish()
    }
}

impl LeaderStatusMonitor {
    pub fn new(
        shared_working_bank: SharedWorkingBank,
        shared_tick_height: SharedTickHeight,
        shared_leader_first_tick_height: SharedLeaderFirstTickHeight,
    ) -> Self {
        Self {
            shared_working_bank,
            shared_tick_height,
            shared_leader_first_tick_height,
        }
    }

    pub(crate) fn status(&self) -> LeaderStatus {
        if let Some(bank) = self.shared_working_bank.load() {
            LeaderStatus::Active(bank)
        } else if let Some(first_leader_tick_height) = self.shared_leader_first_tick_height.load() {
            let current_tick_height = self.shared_tick_height.load();
            // SAFETY: A tick height overflowing i64 would be so far in the future that
            //         it can be reasonably assumed to never happen.
            let ticks_until_leader = first_leader_tick_height as i64 - current_tick_height as i64;
            LeaderStatus::TicksUntilLeader(ticks_until_leader)
        } else {
            LeaderStatus::WillNotBeLeader
        }
    }
}

impl From<&PohRecorder> for LeaderStatusMonitor {
    fn from(poh_recorder: &PohRecorder) -> Self {
        Self::new(
            poh_recorder.shared_working_bank(),
            poh_recorder.shared_tick_height(),
            poh_recorder.shared_leader_first_tick_height(),
        )
    }
}

#[derive(Debug)]
pub(crate) struct LeaderStatusMonitorWrapper {
    is_exited: Arc<AtomicBool>,
    leader_status_monitor: LeaderStatusMonitor,
}

impl LeaderStatusMonitorWrapper {
    pub(crate) fn new(
        is_exited: Arc<AtomicBool>,
        leader_status_monitor: LeaderStatusMonitor,
    ) -> Self {
        Self {
            is_exited,
            leader_status_monitor,
        }
    }
}

impl BankingStageMonitor for LeaderStatusMonitorWrapper {
    fn status(&mut self) -> BankingStageStatus {
        if self.is_exited.load(Relaxed) {
            BankingStageStatus::Exited
        } else {
            match self.leader_status_monitor.status() {
                LeaderStatus::Active(_) => BankingStageStatus::Active,
                LeaderStatus::TicksUntilLeader(ticks) => {
                    if ticks <= HOLD_TRANSACTIONS_SLOT_OFFSET as i64 {
                        BankingStageStatus::Active
                    } else {
                        BankingStageStatus::Inactive
                    }
                }
                LeaderStatus::WillNotBeLeader => BankingStageStatus::Inactive,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, solana_ledger::genesis_utils::create_genesis_config, solana_runtime::bank::Bank,
    };

    #[test]
    fn test_leader_status_bank() {
        let bank = Arc::new(Bank::default_for_tests());
        assert!(LeaderStatus::Active(bank).bank().is_some());
        assert!(LeaderStatus::TicksUntilLeader(1).bank().is_none());
        assert!(LeaderStatus::WillNotBeLeader.bank().is_none());
    }

    #[test]
    fn test_make_consume_or_forward_decision() {
        let genesis_config = create_genesis_config(2).genesis_config;
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);

        let mut shared_working_bank = SharedWorkingBank::empty();
        let shared_tick_height = SharedTickHeight::new(0);
        let mut shared_leader_first_tick_height = SharedLeaderFirstTickHeight::new(None);

        let leader_status_monitor = LeaderStatusMonitor::new(
            shared_working_bank.clone(),
            shared_tick_height.clone(),
            shared_leader_first_tick_height.clone(),
        );

        // No active bank, no leader first tick height.
        assert_matches!(
            leader_status_monitor.status(),
            LeaderStatus::WillNotBeLeader
        );

        // Active bank.
        shared_working_bank.store(bank.clone());
        assert_matches!(leader_status_monitor.status(), LeaderStatus::Active(_));
        shared_working_bank.clear();

        // Known ticks until leader.
        for next_leader_slot_offset in [0, 1, 2, 19].into_iter() {
            let next_leader_slot = bank.slot() + next_leader_slot_offset;
            shared_leader_first_tick_height.store(Some(next_leader_slot * DEFAULT_TICKS_PER_SLOT));

            let decision = leader_status_monitor.status();
            let expected_ticks = next_leader_slot_offset as i64 * DEFAULT_TICKS_PER_SLOT as i64;
            let LeaderStatus::TicksUntilLeader(ticks) = decision else {
                panic!("expected ticks until leader")
            };
            assert_eq!(ticks, expected_ticks);
        }
    }
}
