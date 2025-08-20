use {
    solana_clock::{
        DEFAULT_TICKS_PER_SLOT, FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET,
        HOLD_TRANSACTIONS_SLOT_OFFSET,
    },
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

#[derive(Debug, Clone)]
pub enum BufferedPacketsDecision {
    Consume(Arc<Bank>),
    Forward,
    ForwardAndHold,
    Hold,
}

impl BufferedPacketsDecision {
    /// Returns the `Bank` if the decision is `Consume`. Otherwise, returns `None`.
    pub fn bank(&self) -> Option<&Arc<Bank>> {
        match self {
            Self::Consume(bank) => Some(bank),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct DecisionMaker {
    shared_working_bank: SharedWorkingBank,
    shared_tick_height: SharedTickHeight,
    shared_leader_first_tick_height: SharedLeaderFirstTickHeight,
}

impl std::fmt::Debug for DecisionMaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionMaker")
            .field("shared_working_bank", &self.shared_working_bank.load())
            .field("shared_tick_height", &self.shared_tick_height.load())
            .field(
                "shared_leader_first_tick_height",
                &self.shared_leader_first_tick_height.load(),
            )
            .finish()
    }
}

impl DecisionMaker {
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

    pub(crate) fn make_consume_or_forward_decision(&self) -> BufferedPacketsDecision {
        // Check if there is an active working bank.
        if let Some(bank) = self.shared_working_bank.load() {
            BufferedPacketsDecision::Consume(bank)
        } else if let Some(first_leader_tick_height) = self.shared_leader_first_tick_height.load() {
            let current_tick_height = self.shared_tick_height.load();
            let ticks_until_leader = first_leader_tick_height.saturating_sub(current_tick_height);

            if ticks_until_leader
                <= (FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET - 1) * DEFAULT_TICKS_PER_SLOT
            {
                BufferedPacketsDecision::Hold
            } else if ticks_until_leader < HOLD_TRANSACTIONS_SLOT_OFFSET * DEFAULT_TICKS_PER_SLOT {
                BufferedPacketsDecision::ForwardAndHold
            } else {
                BufferedPacketsDecision::Forward
            }
        } else {
            BufferedPacketsDecision::Forward
        }
    }
}

impl From<&PohRecorder> for DecisionMaker {
    fn from(poh_recorder: &PohRecorder) -> Self {
        Self::new(
            poh_recorder.shared_working_bank(),
            poh_recorder.shared_tick_height(),
            poh_recorder.shared_leader_first_tick_height(),
        )
    }
}

#[derive(Debug)]
pub(crate) struct DecisionMakerWrapper {
    is_exited: Arc<AtomicBool>,
    decision_maker: DecisionMaker,
}

impl DecisionMakerWrapper {
    pub(crate) fn new(is_exited: Arc<AtomicBool>, decision_maker: DecisionMaker) -> Self {
        Self {
            is_exited,
            decision_maker,
        }
    }
}

impl BankingStageMonitor for DecisionMakerWrapper {
    fn status(&mut self) -> BankingStageStatus {
        if self.is_exited.load(Relaxed) {
            BankingStageStatus::Exited
        } else if matches!(
            self.decision_maker.make_consume_or_forward_decision(),
            BufferedPacketsDecision::Forward,
        ) {
            BankingStageStatus::Inactive
        } else {
            BankingStageStatus::Active
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, solana_ledger::genesis_utils::create_genesis_config, solana_runtime::bank::Bank,
    };

    #[test]
    fn test_buffered_packet_decision_bank() {
        let bank = Arc::new(Bank::default_for_tests());
        assert!(BufferedPacketsDecision::Consume(bank).bank().is_some());
        assert!(BufferedPacketsDecision::Forward.bank().is_none());
        assert!(BufferedPacketsDecision::ForwardAndHold.bank().is_none());
        assert!(BufferedPacketsDecision::Hold.bank().is_none());
    }

    #[test]
    fn test_make_consume_or_forward_decision() {
        let genesis_config = create_genesis_config(2).genesis_config;
        let (bank, _bank_forks) = Bank::new_no_wallclock_throttle_for_tests(&genesis_config);

        let mut shared_working_bank = SharedWorkingBank::empty();
        let shared_tick_height = SharedTickHeight::new(0);
        let mut shared_leader_first_tick_height = SharedLeaderFirstTickHeight::new(None);

        let decision_maker = DecisionMaker::new(
            shared_working_bank.clone(),
            shared_tick_height.clone(),
            shared_leader_first_tick_height.clone(),
        );

        // No active bank, no leader first tick height.
        assert_matches!(
            decision_maker.make_consume_or_forward_decision(),
            BufferedPacketsDecision::Forward
        );

        // Active bank.
        shared_working_bank.store(bank.clone());
        assert_matches!(
            decision_maker.make_consume_or_forward_decision(),
            BufferedPacketsDecision::Consume(_)
        );
        shared_working_bank.clear();

        // Will be leader shortly - Hold
        for next_leader_slot_offset in [0, 1].into_iter() {
            let next_leader_slot = bank.slot() + next_leader_slot_offset;
            shared_leader_first_tick_height.store(Some(next_leader_slot * DEFAULT_TICKS_PER_SLOT));

            let decision = decision_maker.make_consume_or_forward_decision();
            assert!(
                matches!(decision, BufferedPacketsDecision::Hold),
                "next_leader_slot_offset: {next_leader_slot_offset}",
            );
        }

        // Will be leader - ForwardAndHold
        for next_leader_slot_offset in [2, 19].into_iter() {
            let next_leader_slot = bank.slot() + next_leader_slot_offset;
            shared_leader_first_tick_height.store(Some(next_leader_slot * DEFAULT_TICKS_PER_SLOT));

            let decision = decision_maker.make_consume_or_forward_decision();
            assert!(
                matches!(decision, BufferedPacketsDecision::ForwardAndHold),
                "next_leader_slot_offset: {next_leader_slot_offset}",
            );
        }

        // Longer period until next leader - Forward
        let next_leader_slot = 20 + bank.slot();
        shared_leader_first_tick_height.store(Some(next_leader_slot * DEFAULT_TICKS_PER_SLOT));
        let decision = decision_maker.make_consume_or_forward_decision();
        assert!(
            matches!(decision, BufferedPacketsDecision::Forward),
            "next_leader_slot: {next_leader_slot}",
        );
    }
}
