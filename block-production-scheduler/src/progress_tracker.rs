use {
    agave_scheduler_bindings::{LEADER_READY, LEADER_STARTING, NOT_LEADER, ProgressMessage},
    core::{num::NonZeroUsize, time::Duration},
    solana_clock::Slot,
    std::time::Instant,
};

type ProgressReceiver = shaq::spsc::Consumer<ProgressMessage>;

/// Leader state from the latest progress message.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SchedulerState {
    NotLeader {
        current_slot: Slot,
        next_leader_slot: Slot,
    },
    LeaderStarting {
        slot: Slot,
    },
    LeaderReady {
        slot: Slot,
        slot_start: Instant,
        slot_duration: Duration,
        remaining_cost_units: u64,
    },
}

impl SchedulerState {
    pub(crate) fn new() -> Self {
        Self::NotLeader {
            current_slot: 0,
            next_leader_slot: Slot::MAX,
        }
    }

    pub(crate) fn drain_progress(&mut self, progress_messages: &mut ProgressReceiver) {
        if let Some(batch) = progress_messages.try_reserve_read_batch(NonZeroUsize::MAX) {
            let progress = &batch[batch.len().wrapping_sub(1)];
            let slot_duration = Duration::from_millis(u64::from(progress.target_bank_time_ms));
            let elapsed = slot_duration
                .saturating_mul(u32::from(progress.current_slot_progress))
                .checked_div(100)
                .unwrap();
            let now = Instant::now();
            *self = match progress.leader_state {
                NOT_LEADER => Self::NotLeader {
                    current_slot: progress.current_slot,
                    next_leader_slot: progress.next_leader_slot,
                },
                LEADER_STARTING => Self::LeaderStarting {
                    slot: progress.current_slot,
                },
                LEADER_READY => Self::LeaderReady {
                    slot: progress.current_slot,
                    slot_start: now.checked_sub(elapsed).unwrap_or(now),
                    slot_duration,
                    remaining_cost_units: progress.remaining_cost_units,
                },
                state => panic!("unknown leader state: {state}"),
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receives_latest_progress() {
        let (mut producer, mut consumer) = shaq::spsc::pair(2).unwrap();
        let mut state = SchedulerState::new();
        let progress = ProgressMessage {
            leader_state: LEADER_READY,
            current_slot_progress: 50,
            epoch: 0,
            current_slot: 100,
            next_leader_slot: 104,
            leader_range_end: 107,
            remaining_cost_units: 123,
            remaining_allocated_accounts_data_size: 0,
            latest_blockhash: [0; 32],
            target_bank_time_ms: 400,
        };
        producer
            .try_write(ProgressMessage {
                leader_state: NOT_LEADER,
                current_slot: 99,
                next_leader_slot: 100,
                ..progress
            })
            .unwrap();
        producer.try_write(progress).unwrap();

        let before = Instant::now();
        state.drain_progress(&mut consumer);
        let SchedulerState::LeaderReady { slot_start, .. } = state else {
            panic!("expected ready leader")
        };
        let observation = slot_start + Duration::from_millis(200);
        assert!(observation >= before && observation <= Instant::now());
        let expected = SchedulerState::LeaderReady {
            slot: 100,
            slot_start,
            slot_duration: Duration::from_millis(400),
            remaining_cost_units: 123,
        };
        assert_eq!(state, expected);
        assert!(consumer.try_read().is_none());

        state.drain_progress(&mut consumer);
        assert_eq!(state, expected);

        producer.try_write(progress).unwrap();
        producer.try_write(progress).unwrap();
    }
}
