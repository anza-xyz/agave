use {
    agave_scheduler_bindings::{LEADER_READY, LEADER_STARTING, NOT_LEADER, ProgressMessage},
    solana_clock::Slot,
    std::num::NonZeroUsize,
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
            current_slot_progress: 0,
            epoch: 0,
            current_slot: 100,
            next_leader_slot: 104,
            leader_range_end: 107,
            remaining_cost_units: 0,
            remaining_allocated_accounts_data_size: 0,
            latest_blockhash: [0; 32],
            target_bank_time_ms: 0,
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

        state.drain_progress(&mut consumer);
        assert_eq!(state, SchedulerState::LeaderReady { slot: 100 });
        assert!(consumer.try_read().is_none());

        state.drain_progress(&mut consumer);
        assert_eq!(state, SchedulerState::LeaderReady { slot: 100 });

        producer.try_write(progress).unwrap();
        producer.try_write(progress).unwrap();
    }
}
