//! Service to send progress updates to the external scheduler.
//!

use {
    crate::banking_stage::consume_worker::ConsumeWorkerMetrics,
    agave_scheduler_bindings::ProgressMessage,
    solana_clock::Slot,
    solana_cost_model::cost_tracker::SharedBlockCost,
    solana_poh::poh_recorder::SharedLeaderState,
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        thread::JoinHandle,
    },
};

/// Spawns a thread to track and send progress updates.
pub fn spawn(
    exit: Arc<AtomicBool>,
    mut producer: shaq::Producer<ProgressMessage>,
    shared_leader_state: SharedLeaderState,
    worker_metrics: Vec<Arc<ConsumeWorkerMetrics>>,
    ticks_per_slot: u64,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("solProgTrker".to_string())
        .spawn(move || {
            ProgressTracker::new(exit, shared_leader_state, worker_metrics, ticks_per_slot)
                .run(&mut producer);
        })
        .unwrap()
}

struct ProgressTracker {
    exit: Arc<AtomicBool>,
    shared_leader_state: SharedLeaderState,
    worker_metrics: Vec<Arc<ConsumeWorkerMetrics>>,
    ticks_per_slot: u64,

    last_observed_leader_slot: Option<Slot>,
    limit_and_shared_block_cost: Option<(u64, SharedBlockCost)>,
}

impl ProgressTracker {
    fn new(
        exit: Arc<AtomicBool>,
        shared_leader_state: SharedLeaderState,
        worker_metrics: Vec<Arc<ConsumeWorkerMetrics>>,
        ticks_per_slot: u64,
    ) -> Self {
        Self {
            exit,
            shared_leader_state,
            worker_metrics,
            ticks_per_slot,

            last_observed_leader_slot: None,
            limit_and_shared_block_cost: None,
        }
    }

    fn run(mut self, producer: &mut shaq::Producer<ProgressMessage>) {
        let mut last_published_tick_height = u64::MAX;
        while !self.exit.load(Ordering::Relaxed) {
            let (message, tick_height) = self.produce_progress_message();
            if tick_height != last_published_tick_height {
                last_published_tick_height = tick_height;
                if !self.publish(producer, message) {
                    break; // external scheduler is so far behind we could not publish a message.
                }
            }

            self.worker_metrics
                .iter()
                .for_each(|metrics| metrics.maybe_report_and_reset());

            // Yield to other threads. Sleeping isn't that accurate and we want to avoid
            // missing updates and delaying progress messages to the external.
            std::thread::yield_now();
        }
    }

    /// returns true if a message was published
    fn publish(
        &mut self,
        producer: &mut shaq::Producer<ProgressMessage>,
        message: ProgressMessage,
    ) -> bool {
        producer.sync();
        if producer.try_write(message).is_ok() {
            producer.commit();
            true
        } else {
            false
        }
    }

    /// Gets current progress and formats into expected message type.
    /// Returns the tick height to avoid publishing the same message multiple times.
    fn produce_progress_message(&mut self) -> (ProgressMessage, u64) {
        let leader_state = self.shared_leader_state.load();
        let tick_height = leader_state.tick_height();
        let (next_leader_range_start, next_leader_range_end) = leader_state
            .next_leader_slot_range()
            .unwrap_or((u64::MAX, u64::MAX));
        let progress_message = if let Some(working_bank) = leader_state.working_bank() {
            // If new leader slot grab the cost tracker lock to get limit and shared cost.
            // This avoid needing to lock except on new leader slots.
            if self.last_observed_leader_slot != Some(working_bank.slot()) {
                let cost_tracker = working_bank.read_cost_tracker().unwrap();
                self.limit_and_shared_block_cost = Some((
                    cost_tracker.get_block_limit(),
                    cost_tracker.shared_block_cost(),
                ));
                self.last_observed_leader_slot = Some(working_bank.slot());
            }

            ProgressMessage {
                leader_state: agave_scheduler_bindings::IS_LEADER,
                current_slot: working_bank.slot(),
                next_leader_slot: next_leader_range_start,
                leader_range_end: next_leader_range_end,
                remaining_cost_units: self.remaining_block_cost(),
                current_slot_progress: progress(
                    working_bank.slot(),
                    tick_height,
                    self.ticks_per_slot,
                ),
            }
        } else {
            let current_slot = slot_from_tick_height(tick_height, self.ticks_per_slot);
            ProgressMessage {
                leader_state: agave_scheduler_bindings::IS_NOT_LEADER,
                current_slot,
                next_leader_slot: next_leader_range_start,
                leader_range_end: next_leader_range_end,
                remaining_cost_units: 0,
                current_slot_progress: progress(current_slot, tick_height, self.ticks_per_slot),
            }
        };

        (progress_message, tick_height)
    }

    /// If leader get the remaining block cost. Otherwise 0.
    fn remaining_block_cost(&self) -> u64 {
        self.limit_and_shared_block_cost
            .as_ref()
            .map(|(limit, shared_block_cost)| limit.saturating_sub(shared_block_cost.load()))
            .unwrap_or(0)
    }
}

/// Calculate progress through a slot based on tick-height.
fn progress(slot: Slot, tick_height: u64, ticks_per_slot: u64) -> u8 {
    debug_assert!(ticks_per_slot < u8::MAX as u64 && ticks_per_slot > 0);

    ((100 * tick_height.saturating_sub(slot * ticks_per_slot)) / ticks_per_slot) as u8
}

/// Calculate a slot based on tick-height - optimistic on boundaries.
/// i.e. tick_height 64 = slot 1 (with 0 progress) rather than slot 0
/// being complete.
fn slot_from_tick_height(tick_height: u64, ticks_per_slot: u64) -> u64 {
    tick_height / ticks_per_slot
}

#[cfg(test)]
mod tests {
    use {
        super::*, solana_clock::DEFAULT_TICKS_PER_SLOT, solana_poh::poh_recorder::LeaderState,
        solana_runtime::bank::Bank,
    };

    #[test]
    fn test_progress_tracker_produce_progress_message() {
        let mut shared_leader_state = SharedLeaderState::new(0, None, None);
        let ticks_per_slot = DEFAULT_TICKS_PER_SLOT;

        let mut progress_tracker = ProgressTracker::new(
            Arc::default(),
            shared_leader_state.clone(),
            vec![],
            ticks_per_slot,
        );

        let (message, tick_height) = progress_tracker.produce_progress_message();
        assert_eq!(tick_height, 0);
        assert_eq!(
            message.leader_state,
            agave_scheduler_bindings::IS_NOT_LEADER
        );
        assert_eq!(message.current_slot, 0);
        assert_eq!(message.current_slot_progress, 0);
        assert_eq!(message.next_leader_slot, u64::MAX);
        assert_eq!(message.leader_range_end, u64::MAX);

        let expected_tick_height = 2 * ticks_per_slot;
        shared_leader_state.store(Arc::new(LeaderState::new(
            None,
            expected_tick_height,
            None,
            None,
        )));
        let (message, tick_height) = progress_tracker.produce_progress_message();
        assert_eq!(tick_height, expected_tick_height);
        assert_eq!(
            message.leader_state,
            agave_scheduler_bindings::IS_NOT_LEADER
        );
        assert_eq!(message.current_slot, 2);
        assert_eq!(message.next_leader_slot, u64::MAX);
        assert_eq!(message.leader_range_end, u64::MAX);
        assert_eq!(message.current_slot_progress, 0);

        shared_leader_state.store(Arc::new(LeaderState::new(
            None,
            expected_tick_height,
            Some(4 * ticks_per_slot),
            Some((4, 7)),
        )));
        let (message, tick_height) = progress_tracker.produce_progress_message();
        assert_eq!(tick_height, expected_tick_height);
        assert_eq!(
            message.leader_state,
            agave_scheduler_bindings::IS_NOT_LEADER
        );
        assert_eq!(message.current_slot, 2);
        assert_eq!(message.next_leader_slot, 4);
        assert_eq!(message.leader_range_end, 7);
        assert_eq!(message.current_slot_progress, 0);

        let bank = Arc::new(Bank::new_for_tests(
            &solana_genesis_config::create_genesis_config(1).0,
        ));
        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            Some(4 * ticks_per_slot),
            Some((4, 7)),
        )));

        assert!(!bank.is_complete());
        let (message, tick_height) = progress_tracker.produce_progress_message();
        assert_eq!(tick_height, bank.tick_height());
        assert_eq!(message.leader_state, agave_scheduler_bindings::IS_LEADER);
        assert_eq!(message.current_slot, bank.slot());
        assert_eq!(message.next_leader_slot, 4);
        assert_eq!(message.leader_range_end, 7);
        assert_eq!(message.current_slot_progress, 0);

        bank.fill_bank_with_ticks_for_tests();
        assert!(bank.is_complete());
        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            Some(4 * ticks_per_slot),
            Some((4, 7)),
        )));
        let (message, tick_height) = progress_tracker.produce_progress_message();
        assert_eq!(tick_height, bank.tick_height());
        assert_eq!(message.leader_state, agave_scheduler_bindings::IS_LEADER);
        assert_eq!(message.current_slot, bank.slot());
        assert_eq!(message.next_leader_slot, 4);
        assert_eq!(message.leader_range_end, 7);
        assert_eq!(message.current_slot_progress, 100);
    }

    #[test]
    fn test_progress_tracker_remaining_block_cost() {
        let mut progress_tracker = ProgressTracker::new(
            Arc::default(),
            SharedLeaderState::new(0, None, None),
            vec![],
            DEFAULT_TICKS_PER_SLOT,
        );

        // No bank - no block cost set (0).
        assert_eq!(0, progress_tracker.remaining_block_cost());

        let block_limit = 10_000;
        progress_tracker.limit_and_shared_block_cost = Some((block_limit, SharedBlockCost::new(0)));
        assert_eq!(block_limit, progress_tracker.remaining_block_cost());
        progress_tracker.limit_and_shared_block_cost =
            Some((block_limit, SharedBlockCost::new(block_limit / 2)));
        assert_eq!(block_limit / 2, progress_tracker.remaining_block_cost());
    }

    #[test]
    fn test_progress() {
        let ticks_per_slot = DEFAULT_TICKS_PER_SLOT;
        assert_eq!(0, progress(0, 0, ticks_per_slot));
        assert_eq!(1, progress(0, 1, ticks_per_slot));
        assert_eq!(3, progress(0, 2, ticks_per_slot));
        assert_eq!(98, progress(0, ticks_per_slot - 1, ticks_per_slot));
        assert_eq!(100, progress(0, ticks_per_slot, ticks_per_slot));
        assert_eq!(0, progress(1, ticks_per_slot, ticks_per_slot));
        assert_eq!(3, progress(1, ticks_per_slot + 2, ticks_per_slot));
    }

    #[test]
    fn test_slot_from_tick_height() {
        let ticks_per_slot = DEFAULT_TICKS_PER_SLOT;
        assert_eq!(0, slot_from_tick_height(0, ticks_per_slot));
        assert_eq!(0, slot_from_tick_height(ticks_per_slot - 1, ticks_per_slot));
        assert_eq!(1, slot_from_tick_height(ticks_per_slot, ticks_per_slot));
        assert_eq!(1, slot_from_tick_height(ticks_per_slot + 1, ticks_per_slot));
        assert_eq!(
            1,
            slot_from_tick_height(2 * ticks_per_slot - 1, ticks_per_slot)
        );
        assert_eq!(2, slot_from_tick_height(2 * ticks_per_slot, ticks_per_slot));
        assert_eq!(
            2,
            slot_from_tick_height(2 * ticks_per_slot + 1, ticks_per_slot)
        );
    }
}
