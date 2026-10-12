use {
    super::alpenglow_timing::alpenglow_slot_progress,
    agave_votor::slot_clock::SharedAlpenglowSlotClock,
    solana_clock::{FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET, HOLD_TRANSACTIONS_SLOT_OFFSET},
    solana_poh::poh_recorder::{LeaderState, SharedLeaderState},
    solana_runtime::bank::Bank,
    std::{sync::Arc, time::Instant},
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
    shared_leader_state: SharedLeaderState,
    alpenglow_slot_clock: SharedAlpenglowSlotClock,
}

impl std::fmt::Debug for DecisionMaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DecisionMaker").finish()
    }
}

impl DecisionMaker {
    pub fn new(
        shared_leader_state: SharedLeaderState,
        alpenglow_slot_clock: SharedAlpenglowSlotClock,
    ) -> Self {
        Self {
            shared_leader_state,
            alpenglow_slot_clock,
        }
    }

    pub(crate) fn make_consume_or_forward_decision(&self) -> BufferedPacketsDecision {
        self.make_consume_or_forward_decision_at(Instant::now())
    }

    fn make_consume_or_forward_decision_at(&self, now: Instant) -> BufferedPacketsDecision {
        let state = self.shared_leader_state.load();

        if let Some(working_bank) = state.working_bank() {
            BufferedPacketsDecision::Consume(working_bank.clone())
        } else {
            self.make_decision_alpenglow(&state, now)
        }
    }

    fn make_decision_alpenglow(
        &self,
        state: &LeaderState,
        now: Instant,
    ) -> BufferedPacketsDecision {
        let Some((leader_slot, last_leader_slot)) = state.next_leader_slot_range() else {
            return BufferedPacketsDecision::Forward;
        };
        let Some(slot_info) = self.alpenglow_slot_clock.load() else {
            return BufferedPacketsDecision::Forward;
        };
        // PoH tick height no longer advances after Alpenglow activation.
        let (current_slot, _) = alpenglow_slot_progress(
            slot_info.slot,
            now.saturating_duration_since(slot_info.started_at),
            slot_info.slot_duration,
        );
        if current_slot > last_leader_slot {
            return BufferedPacketsDecision::Forward;
        }
        let slots_until_leader = leader_slot.saturating_sub(current_slot);
        if slots_until_leader < FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET {
            BufferedPacketsDecision::Hold
        } else if slots_until_leader < HOLD_TRANSACTIONS_SLOT_OFFSET {
            BufferedPacketsDecision::ForwardAndHold
        } else {
            BufferedPacketsDecision::Forward
        }
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_runtime::bank::Bank};

    #[test]
    fn test_buffered_packet_decision_bank() {
        let bank = Arc::new(Bank::default_for_tests());
        assert!(BufferedPacketsDecision::Consume(bank).bank().is_some());
        assert!(BufferedPacketsDecision::Forward.bank().is_none());
        assert!(BufferedPacketsDecision::ForwardAndHold.bank().is_none());
        assert!(BufferedPacketsDecision::Hold.bank().is_none());
    }

    #[test]
    fn test_alpenglow_decision_with_stopped_ticks() {
        use std::time::Duration;

        let mut shared_leader_state = SharedLeaderState::new(0, Some((24, 27)));
        let clock = SharedAlpenglowSlotClock::default();
        let decision_maker = DecisionMaker::new(shared_leader_state.clone(), clock.clone());
        let started_at = Instant::now();
        let slot_duration = Duration::from_millis(200);

        // No clock observation yet.
        assert_matches!(
            decision_maker.make_consume_or_forward_decision_at(started_at),
            BufferedPacketsDecision::Forward
        );

        clock.update(4, started_at, slot_duration);
        // Exactly twenty slots away: do not buffer yet.
        assert_matches!(
            decision_maker.make_consume_or_forward_decision_at(started_at),
            BufferedPacketsDecision::Forward
        );
        // Progress within the slot does not change the buffering decision.
        assert_matches!(
            decision_maker.make_consume_or_forward_decision_at(started_at + slot_duration / 2),
            BufferedPacketsDecision::Forward
        );
        // Time advances while tick height stays at zero.
        assert_matches!(
            decision_maker.make_consume_or_forward_decision_at(started_at + slot_duration),
            BufferedPacketsDecision::ForwardAndHold
        );
        // A stale observation cannot advance past its leader window.
        assert_matches!(
            decision_maker
                .make_consume_or_forward_decision_at(started_at + Duration::from_secs(60)),
            BufferedPacketsDecision::ForwardAndHold
        );

        clock.update(20, started_at, slot_duration);
        assert_matches!(
            decision_maker.make_consume_or_forward_decision_at(started_at + slot_duration * 2),
            BufferedPacketsDecision::ForwardAndHold
        );
        // Exactly one slot away.
        assert_matches!(
            decision_maker.make_consume_or_forward_decision_at(started_at + slot_duration * 3),
            BufferedPacketsDecision::Hold
        );
        // The leader window has begun but no working bank is available.
        clock.update(24, started_at, slot_duration);
        assert_matches!(
            decision_maker.make_consume_or_forward_decision_at(started_at),
            BufferedPacketsDecision::Hold
        );

        // Keep buffering through the final slot of our leader window.
        assert_matches!(
            decision_maker.make_consume_or_forward_decision_at(started_at + slot_duration * 3),
            BufferedPacketsDecision::Hold
        );
        // A newer observation beyond our window must stop buffering.
        clock.update(28, started_at, slot_duration);
        assert_matches!(
            decision_maker.make_consume_or_forward_decision_at(started_at),
            BufferedPacketsDecision::Forward
        );

        shared_leader_state.store(Arc::new(LeaderState::new(None, 0, None)));
        assert_matches!(
            decision_maker.make_consume_or_forward_decision_at(started_at),
            BufferedPacketsDecision::Forward
        );

        // A working bank takes precedence over timing and schedule information.
        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(Arc::new(Bank::default_for_tests())),
            0,
            None,
        )));
        assert_matches!(
            decision_maker.make_consume_or_forward_decision_at(started_at),
            BufferedPacketsDecision::Consume(_)
        );
    }
}
