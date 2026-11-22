//! Service to send progress updates to the external scheduler.
//!

use {
    super::alpenglow_timing::{ALPENGLOW_PROGRESS_STEP, alpenglow_slot_progress},
    crate::banking_stage::consume_worker::ConsumeWorkerMetrics,
    agave_scheduler_bindings::ProgressMessage,
    agave_votor::slot_clock::SharedAlpenglowSlotClock,
    solana_clock::Slot,
    solana_cost_model::cost_tracker::{SharedAllocatedAccountsDataSize, SharedBlockCost},
    solana_poh::poh_recorder::SharedLeaderState,
    solana_runtime::{bank::BankId, leader_schedule_utils::last_of_consecutive_leader_slots},
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::JoinHandle,
        time::{Duration, Instant},
    },
};

const ALPENGLOW_PROGRESS_SPIN_DURATION: Duration = Duration::from_millis(1);

/// Spawns a thread to track and send progress updates.
pub fn spawn(
    exit: Arc<AtomicBool>,
    mut producer: shaq::spsc::Producer<ProgressMessage>,
    shared_leader_state: SharedLeaderState,
    worker_metrics: Vec<Arc<ConsumeWorkerMetrics>>,
    alpenglow_slot_clock: SharedAlpenglowSlotClock,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("solProgTrker".to_string())
        .spawn(move || {
            ProgressTracker::new(
                exit,
                shared_leader_state,
                worker_metrics,
                alpenglow_slot_clock,
            )
            .run(&mut producer);
        })
        .unwrap()
}

struct ProgressTracker {
    exit: Arc<AtomicBool>,
    shared_leader_state: SharedLeaderState,
    worker_metrics: Vec<(String, Arc<ConsumeWorkerMetrics>)>,
    alpenglow_slot_clock: SharedAlpenglowSlotClock,

    last_observed_bank_id: Option<BankId>,
    limit_and_shared_block_cost: Option<(u64, SharedBlockCost)>,
    limit_and_shared_allocated_accounts_data_size: Option<(u64, SharedAllocatedAccountsDataSize)>,
}

impl ProgressTracker {
    fn new(
        exit: Arc<AtomicBool>,
        shared_leader_state: SharedLeaderState,
        worker_metrics: Vec<Arc<ConsumeWorkerMetrics>>,
        alpenglow_slot_clock: SharedAlpenglowSlotClock,
    ) -> Self {
        Self {
            exit,
            shared_leader_state,
            worker_metrics: worker_metrics
                .into_iter()
                .enumerate()
                .map(|(id, metrics)| (id.to_string(), metrics))
                .collect(),
            alpenglow_slot_clock,

            last_observed_bank_id: None,
            limit_and_shared_block_cost: None,
            limit_and_shared_allocated_accounts_data_size: None,
        }
    }

    fn run(mut self, producer: &mut shaq::spsc::Producer<ProgressMessage>) {
        let mut last_published_progress = None;
        while !self.exit.load(Ordering::Relaxed) {
            if let Some(message) = self.produce_progress_message(Instant::now()) {
                let progress = (
                    message.leader_state,
                    message.current_slot,
                    message.current_slot_progress,
                    self.last_observed_bank_id,
                );
                if Some(progress) != last_published_progress {
                    if !self.publish(producer, message) {
                        break; // external scheduler is so far behind we could not publish a message.
                    }
                    last_published_progress = Some(progress);
                }
            }

            self.worker_metrics
                .iter()
                .for_each(|(id, metrics)| metrics.maybe_report_and_reset(id));

            self.wait_for_next_progress_boundary();
        }
    }

    fn wait_for_next_progress_boundary(&self) {
        let deadline = self.alpenglow_slot_clock.load().and_then(|slot_info| {
            next_alpenglow_progress_deadline(
                slot_info.started_at,
                slot_info.slot_duration,
                Instant::now(),
            )
        });
        let Some(deadline) = deadline else {
            std::thread::yield_now();
            return;
        };

        let sleep_until = deadline
            .checked_sub(ALPENGLOW_PROGRESS_SPIN_DURATION)
            .unwrap_or(deadline);
        if let Some(sleep_duration) = sleep_until.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep_duration);
        }
        while Instant::now() < deadline && !self.exit.load(Ordering::Relaxed) {
            std::thread::yield_now();
        }
    }

    /// returns true if a message was published
    fn publish(
        &mut self,
        producer: &mut shaq::spsc::Producer<ProgressMessage>,
        message: ProgressMessage,
    ) -> bool {
        producer.try_write(message).is_ok()
    }

    /// Gets current progress and formats into expected message type.
    fn produce_progress_message(&mut self, now: Instant) -> Option<ProgressMessage> {
        let leader_state = self.shared_leader_state.load();
        let (next_leader_range_start, next_leader_range_end) = leader_state
            .next_leader_slot_range()
            .unwrap_or((u64::MAX, u64::MAX));
        let progress_message = if let Some(working_bank) = leader_state.working_bank() {
            let bank_id = working_bank.bank_id();
            // If new bank grab the cost tracker lock to get limits and shared costs.
            // This avoids needing to lock except on bank switches.
            if self.last_observed_bank_id != Some(bank_id) {
                let cost_tracker = working_bank.read_cost_tracker().unwrap();
                self.limit_and_shared_block_cost = Some((
                    cost_tracker.get_block_limit(),
                    cost_tracker.shared_block_cost(),
                ));
                self.limit_and_shared_allocated_accounts_data_size = Some((
                    cost_tracker.get_allocated_data_size_limit(),
                    cost_tracker.shared_allocated_accounts_data_size(),
                ));
                self.last_observed_bank_id = Some(bank_id);
            }

            let current_slot_progress = self
                .alpenglow_slot_clock
                .load()
                .map(|slot_info| {
                    let duration_since_slot_start =
                        now.saturating_duration_since(slot_info.started_at);
                    alpenglow_bank_progress(
                        working_bank.slot(),
                        slot_info.slot,
                        duration_since_slot_start,
                        Duration::from_nanos_u128(working_bank.ns_per_slot),
                    )
                })
                .unwrap_or(0);

            ProgressMessage {
                leader_state: agave_scheduler_bindings::LEADER_READY,
                current_slot_progress,
                epoch: working_bank.epoch(),
                current_slot: working_bank.slot(),
                next_leader_slot: next_leader_range_start,
                leader_range_end: next_leader_range_end,
                remaining_cost_units: self.remaining_block_cost(),
                remaining_allocated_accounts_data_size: self
                    .remaining_allocated_accounts_data_size(),
                latest_blockhash: working_bank.last_blockhash().to_bytes(),
                target_bank_time_ms: target_bank_time_ms(working_bank.ns_per_slot),
            }
        } else {
            self.last_observed_bank_id = None;
            self.limit_and_shared_block_cost = None;
            self.limit_and_shared_allocated_accounts_data_size = None;
            let (current_slot, current_slot_progress, window_expired) = {
                let slot_info = self.alpenglow_slot_clock.load()?;
                let duration_since_slot_start = now.saturating_duration_since(slot_info.started_at);
                let remaining_slots =
                    last_of_consecutive_leader_slots(slot_info.slot) - slot_info.slot + 1;
                let remaining_window_duration = slot_info
                    .slot_duration
                    .saturating_mul(remaining_slots as u32);
                // The estimate is only valid within the anchored window.
                // Stop reporting LEADER_STARTING when the anchored window expires.
                let window_expired = duration_since_slot_start >= remaining_window_duration;
                let (slot, progress) = alpenglow_slot_progress(
                    slot_info.slot,
                    duration_since_slot_start,
                    slot_info.slot_duration,
                );
                (slot, progress, window_expired)
            };

            // No bank yet but we may already be inside our leader window.
            let leader_state = if !window_expired
                && (next_leader_range_start..=next_leader_range_end).contains(&current_slot)
            {
                agave_scheduler_bindings::LEADER_STARTING
            } else {
                agave_scheduler_bindings::NOT_LEADER
            };

            ProgressMessage {
                leader_state,
                current_slot_progress,
                epoch: 0,
                current_slot,
                next_leader_slot: next_leader_range_start,
                leader_range_end: next_leader_range_end,
                remaining_cost_units: 0,
                remaining_allocated_accounts_data_size: 0,
                latest_blockhash: [0; 32],
                target_bank_time_ms: 0,
            }
        };

        Some(progress_message)
    }

    /// If leader get the remaining block cost. Otherwise 0.
    fn remaining_block_cost(&self) -> u64 {
        self.limit_and_shared_block_cost
            .as_ref()
            .map(|(limit, shared_block_cost)| limit.saturating_sub(shared_block_cost.load()))
            .unwrap_or(0)
    }

    /// If leader get the remaining allocated accounts data size. Otherwise 0.
    fn remaining_allocated_accounts_data_size(&self) -> u64 {
        self.limit_and_shared_allocated_accounts_data_size
            .as_ref()
            .map(|(limit, shared_allocated_accounts_data_size)| {
                limit.saturating_sub(shared_allocated_accounts_data_size.load())
            })
            .unwrap_or(0)
    }
}

fn target_bank_time_ms(ns_per_slot: u128) -> u16 {
    let milliseconds = ns_per_slot.wrapping_div(1_000_000);
    u16::try_from(milliseconds).unwrap_or(u16::MAX)
}

fn next_alpenglow_progress_deadline(
    started_at: Instant,
    slot_duration: Duration,
    now: Instant,
) -> Option<Instant> {
    let progress_interval = slot_duration / u32::from(100 / ALPENGLOW_PROGRESS_STEP);
    if progress_interval.is_zero() {
        return None;
    }

    let completed_intervals = now
        .saturating_duration_since(started_at)
        .as_nanos()
        .saturating_div(progress_interval.as_nanos());
    let next_boundary_nanos = completed_intervals
        .checked_add(1)?
        .checked_mul(progress_interval.as_nanos())?;
    started_at.checked_add(Duration::from_nanos_u128(next_boundary_nanos))
}

fn alpenglow_bank_progress(
    bank_slot: Slot,
    window_start_slot: Slot,
    elapsed: Duration,
    slot_duration: Duration,
) -> u8 {
    let (clock_slot, clock_progress) =
        alpenglow_slot_progress(window_start_slot, elapsed, slot_duration);
    match bank_slot.cmp(&clock_slot) {
        std::cmp::Ordering::Less => 100,
        std::cmp::Ordering::Equal => clock_progress,
        std::cmp::Ordering::Greater => 0,
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, solana_clock::DEFAULT_TICKS_PER_SLOT,
        solana_epoch_schedule::MINIMUM_SLOTS_PER_EPOCH, solana_leader_schedule::SlotLeader,
        solana_poh::poh_recorder::LeaderState, solana_runtime::bank::Bank,
    };

    #[test]
    fn test_progress_tracker_produce_progress_message() {
        let now = Instant::now();
        let mut shared_leader_state = SharedLeaderState::new(0, None, None);
        let ticks_per_slot = DEFAULT_TICKS_PER_SLOT;
        let clock = SharedAlpenglowSlotClock::default();
        let slot_duration = Duration::from_millis(200);

        let mut progress_tracker = ProgressTracker::new(
            Arc::default(),
            shared_leader_state.clone(),
            vec![],
            clock.clone(),
        );

        assert!(progress_tracker.produce_progress_message(now).is_none());
        clock.update(0, now, slot_duration);
        let message = progress_tracker.produce_progress_message(now).unwrap();
        assert_eq!(message.leader_state, agave_scheduler_bindings::NOT_LEADER);
        assert_eq!(message.current_slot, 0);
        assert_eq!(message.current_slot_progress, 0);
        assert_eq!(message.next_leader_slot, u64::MAX);
        assert_eq!(message.leader_range_end, u64::MAX);
        assert_eq!(message.epoch, 0);
        assert_eq!(message.remaining_cost_units, 0);
        assert_eq!(message.remaining_allocated_accounts_data_size, 0);
        assert_eq!(message.latest_blockhash, [0; 32]);
        assert_eq!(message.target_bank_time_ms, 0);

        let expected_tick_height = 2 * ticks_per_slot;
        shared_leader_state.store(Arc::new(LeaderState::new(
            None,
            expected_tick_height,
            None,
            None,
        )));
        // Slot estimates follow the clock rather than the tick height.
        let message = progress_tracker.produce_progress_message(now).unwrap();
        assert_eq!(message.current_slot, 0);
        clock.update_leader(2, now, slot_duration);
        let message = progress_tracker.produce_progress_message(now).unwrap();
        assert_eq!(message.leader_state, agave_scheduler_bindings::NOT_LEADER);
        assert_eq!(message.current_slot, 2);
        assert_eq!(message.next_leader_slot, u64::MAX);
        assert_eq!(message.leader_range_end, u64::MAX);
        assert_eq!(message.current_slot_progress, 0);
        assert_eq!(message.epoch, 0);
        assert_eq!(message.latest_blockhash, [0; 32]);
        assert_eq!(message.target_bank_time_ms, 0);

        // Next leader slot is in the future - should be NOT_LEADER.
        shared_leader_state.store(Arc::new(LeaderState::new(
            None,
            expected_tick_height,
            Some(4 * ticks_per_slot),
            Some((4, 7)),
        )));
        let message = progress_tracker.produce_progress_message(now).unwrap();
        assert_eq!(message.leader_state, agave_scheduler_bindings::NOT_LEADER);
        assert_eq!(message.current_slot, 2);
        assert_eq!(message.next_leader_slot, 4);
        assert_eq!(message.leader_range_end, 7);
        assert_eq!(message.current_slot_progress, 0);
        assert_eq!(message.epoch, 0);
        assert_eq!(message.latest_blockhash, [0; 32]);
        assert_eq!(message.target_bank_time_ms, 0);

        // In leader slot but no bank yet - should be LEADER_STARTING.
        // The clock places us 5% into slot 4.
        let leader_first_tick = 4 * ticks_per_slot + 1;
        shared_leader_state.store(Arc::new(LeaderState::new(
            None,
            leader_first_tick, // tick_height >= leader_first_tick_height
            Some(leader_first_tick),
            Some((4, 7)),
        )));
        clock.update_leader(4, now - slot_duration / 20, slot_duration);
        let message = progress_tracker.produce_progress_message(now).unwrap();
        assert_eq!(
            message.leader_state,
            agave_scheduler_bindings::LEADER_STARTING
        );
        assert_eq!(message.current_slot, 4);
        assert_eq!(message.next_leader_slot, 4);
        assert_eq!(message.leader_range_end, 7);
        assert_eq!(message.current_slot_progress, 5);
        assert_eq!(message.epoch, 0);
        assert_eq!(message.latest_blockhash, [0; 32]);
        assert_eq!(message.target_bank_time_ms, 0);

        // A clock observation can also start in the middle of a leader window.
        let slot_5_boundary = 5 * ticks_per_slot;
        shared_leader_state.store(Arc::new(LeaderState::new(
            None,
            slot_5_boundary,
            Some(slot_5_boundary + 1),
            Some((5, 7)),
        )));
        clock.update_leader(5, now, slot_duration);
        let message = progress_tracker.produce_progress_message(now).unwrap();
        assert_eq!(message.current_slot, 5);
        assert_eq!(
            message.leader_state,
            agave_scheduler_bindings::LEADER_STARTING
        );
    }

    #[test]
    fn test_progress_tracker_produce_progress_message_with_working_bank() {
        let now = Instant::now();
        let mut shared_leader_state = SharedLeaderState::new(0, None, None);
        let ticks_per_slot = DEFAULT_TICKS_PER_SLOT;
        let clock = SharedAlpenglowSlotClock::default();
        let mut progress_tracker = ProgressTracker::new(
            Arc::default(),
            shared_leader_state.clone(),
            vec![],
            clock.clone(),
        );

        let (bank, _bank_forks) =
            Bank::new_for_tests(&solana_genesis_config::create_genesis_config(1).0)
                .wrap_with_bank_forks_for_tests();
        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            Some(4 * ticks_per_slot),
            Some((4, 7)),
        )));

        // With a working bank - should be LEADER_READY.
        assert!(!bank.is_complete());
        let message = progress_tracker.produce_progress_message(now).unwrap();
        assert_eq!(message.leader_state, agave_scheduler_bindings::LEADER_READY);
        assert_eq!(message.current_slot, bank.slot());
        assert_eq!(message.next_leader_slot, 4);
        assert_eq!(message.leader_range_end, 7);
        assert_eq!(message.current_slot_progress, 0);
        assert_eq!(message.epoch, bank.epoch());
        assert_eq!(
            message.remaining_allocated_accounts_data_size,
            bank.read_cost_tracker()
                .unwrap()
                .get_allocated_data_size_limit()
        );
        assert_eq!(message.latest_blockhash, bank.last_blockhash().to_bytes());
        assert_eq!(
            message.target_bank_time_ms,
            target_bank_time_ms(bank.ns_per_slot)
        );

        bank.fill_bank_with_ticks_for_tests();
        assert!(bank.is_complete());
        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            bank.tick_height(),
            Some(4 * ticks_per_slot),
            Some((4, 7)),
        )));
        // Completing PoH ticks alone does not advance clock-based progress.
        let message = progress_tracker.produce_progress_message(now).unwrap();
        assert_eq!(message.current_slot_progress, 0);
        clock.update_leader(
            bank.slot(),
            now - Duration::from_nanos_u128(bank.ns_per_slot),
            Duration::from_nanos_u128(bank.ns_per_slot),
        );
        let message = progress_tracker.produce_progress_message(now).unwrap();
        assert_eq!(message.leader_state, agave_scheduler_bindings::LEADER_READY);
        assert_eq!(message.current_slot, bank.slot());
        assert_eq!(message.next_leader_slot, 4);
        assert_eq!(message.leader_range_end, 7);
        assert_eq!(message.current_slot_progress, 100);
        assert_eq!(message.epoch, bank.epoch());
        assert_eq!(message.latest_blockhash, bank.last_blockhash().to_bytes());
        assert_eq!(
            message.target_bank_time_ms,
            target_bank_time_ms(bank.ns_per_slot)
        );

        // Child bank past the first epoch boundary - epoch should advance.
        let child_bank = Arc::new(Bank::new_from_parent(
            bank,
            SlotLeader::new_unique(),
            MINIMUM_SLOTS_PER_EPOCH,
        ));
        assert_eq!(child_bank.epoch(), 1);
        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(child_bank.clone()),
            child_bank.tick_height(),
            Some(MINIMUM_SLOTS_PER_EPOCH),
            Some((MINIMUM_SLOTS_PER_EPOCH, MINIMUM_SLOTS_PER_EPOCH + 3)),
        )));
        let message = progress_tracker.produce_progress_message(now).unwrap();
        assert_eq!(message.leader_state, agave_scheduler_bindings::LEADER_READY);
        assert_eq!(message.current_slot, child_bank.slot());
        assert_eq!(message.next_leader_slot, MINIMUM_SLOTS_PER_EPOCH);
        assert_eq!(message.leader_range_end, MINIMUM_SLOTS_PER_EPOCH + 3);
        assert_eq!(message.current_slot_progress, 0);
        assert_eq!(message.epoch, child_bank.epoch());
        assert_eq!(
            message.latest_blockhash,
            child_bank.last_blockhash().to_bytes()
        );
        assert_eq!(
            message.target_bank_time_ms,
            target_bank_time_ms(child_bank.ns_per_slot)
        );
    }

    #[test]
    fn test_next_alpenglow_progress_deadline() {
        let started_at = Instant::now();
        let slot_duration = Duration::from_millis(400);
        assert_eq!(
            next_alpenglow_progress_deadline(started_at, slot_duration, started_at),
            Some(started_at + Duration::from_millis(20))
        );
        assert_eq!(
            next_alpenglow_progress_deadline(
                started_at,
                slot_duration,
                started_at + Duration::from_millis(19)
            ),
            Some(started_at + Duration::from_millis(20))
        );
        assert_eq!(
            next_alpenglow_progress_deadline(
                started_at,
                slot_duration,
                started_at + Duration::from_millis(20)
            ),
            Some(started_at + Duration::from_millis(40))
        );
        assert_eq!(
            next_alpenglow_progress_deadline(
                started_at,
                Duration::from_millis(200),
                started_at + Duration::from_millis(413)
            ),
            Some(started_at + Duration::from_millis(420))
        );
        assert_eq!(
            next_alpenglow_progress_deadline(started_at, Duration::ZERO, started_at),
            None
        );
    }

    #[test]
    fn test_alpenglow_bank_progress() {
        let slot_duration = Duration::from_millis(400);
        assert_eq!(
            alpenglow_bank_progress(4, 4, Duration::from_millis(399), slot_duration),
            95
        );
        assert_eq!(
            alpenglow_bank_progress(4, 4, slot_duration, slot_duration),
            100
        );
        assert_eq!(
            alpenglow_bank_progress(5, 4, slot_duration, slot_duration),
            0
        );
        assert_eq!(
            alpenglow_bank_progress(7, 8, Duration::ZERO, slot_duration),
            100
        );
        assert_eq!(
            alpenglow_bank_progress(8, 4, Duration::from_millis(1_600), slot_duration),
            0
        );
    }

    #[test]
    fn test_alpenglow_progress_without_working_bank() {
        let now = Instant::now();
        let alpenglow_slot_clock = SharedAlpenglowSlotClock::default();
        let mut progress_tracker = ProgressTracker::new(
            Arc::default(),
            SharedLeaderState::new(0, None, Some((4, 7))),
            vec![],
            alpenglow_slot_clock.clone(),
        );

        assert!(progress_tracker.produce_progress_message(now).is_none());

        alpenglow_slot_clock.update(4, now, Duration::from_secs(10));
        let message = progress_tracker.produce_progress_message(now).unwrap();
        assert_eq!(
            message.leader_state,
            agave_scheduler_bindings::LEADER_STARTING
        );
        assert_eq!(message.current_slot, 4);
        assert_eq!(message.current_slot_progress, 0);
        assert_eq!(message.next_leader_slot, 4);
        assert_eq!(message.leader_range_end, 7);

        alpenglow_slot_clock.update_leader(
            4,
            now - Duration::from_secs(40),
            Duration::from_secs(10),
        );
        let message = progress_tracker.produce_progress_message(now).unwrap();
        assert_eq!(message.leader_state, agave_scheduler_bindings::NOT_LEADER);
        assert_eq!(message.current_slot, 7);
        assert_eq!(message.current_slot_progress, 100);

        alpenglow_slot_clock.update(8, now, Duration::from_secs(10));
        let message = progress_tracker.produce_progress_message(now).unwrap();
        assert_eq!(message.leader_state, agave_scheduler_bindings::NOT_LEADER);
        assert_eq!(message.current_slot, 8);
        assert_eq!(message.current_slot_progress, 0);
    }

    #[test]
    fn test_alpenglow_progress_stops_at_window_end() {
        let now = Instant::now();
        for (anchor_slot, slot_duration) in [
            (4, Duration::from_secs(60)),
            (5, Duration::from_secs(60)),
            (7, Duration::from_secs(60)),
            (4, Duration::ZERO),
        ] {
            let clock = SharedAlpenglowSlotClock::default();
            let mut tracker = ProgressTracker::new(
                Arc::default(),
                SharedLeaderState::new(0, None, Some((24, 27))),
                vec![],
                clock.clone(),
            );
            let window_duration = slot_duration * (8 - anchor_slot) as u32;
            clock.update(anchor_slot, now - window_duration, slot_duration);
            let message = tracker.produce_progress_message(now).unwrap();
            assert_eq!(message.leader_state, agave_scheduler_bindings::NOT_LEADER);
            assert_eq!(message.current_slot, 7);
            assert_eq!(message.current_slot_progress, 100);

            // A new observation replaces the capped progress estimate.
            clock.update(8, now, Duration::from_secs(60));
            let message = tracker.produce_progress_message(now).unwrap();
            assert_eq!(message.leader_state, agave_scheduler_bindings::NOT_LEADER);
            assert_eq!(message.current_slot, 8);
            assert_eq!(message.current_slot_progress, 0);
        }
    }

    #[test]
    fn test_alpenglow_progress_with_expired_clock_and_working_bank() {
        let now = Instant::now();
        let bank = Arc::new(Bank::new_for_tests(
            &solana_genesis_config::create_genesis_config(1).0,
        ));
        let clock = SharedAlpenglowSlotClock::default();
        clock.update(0, now - Duration::from_secs(4), Duration::from_secs(1));
        let mut shared_leader_state = SharedLeaderState::new(0, None, Some((0, 3)));
        shared_leader_state.store(Arc::new(LeaderState::new(
            Some(bank.clone()),
            0,
            None,
            Some((0, 3)),
        )));
        let mut tracker =
            ProgressTracker::new(Arc::default(), shared_leader_state.clone(), vec![], clock);

        let message = tracker.produce_progress_message(now).unwrap();
        assert_eq!(message.leader_state, agave_scheduler_bindings::LEADER_READY);
        assert_eq!(message.current_slot, bank.slot());
        assert_eq!(message.current_slot_progress, 100);

        shared_leader_state.store(Arc::new(LeaderState::new(None, 0, None, Some((4, 7)))));
        let message = tracker.produce_progress_message(now).unwrap();
        assert_eq!(message.leader_state, agave_scheduler_bindings::NOT_LEADER);
        assert_eq!(message.current_slot, 3);
        assert_eq!(message.current_slot_progress, 100);
    }

    #[test]
    fn test_progress_tracker_remaining_costs() {
        let mut progress_tracker = ProgressTracker::new(
            Arc::default(),
            SharedLeaderState::new(0, None, None),
            vec![],
            SharedAlpenglowSlotClock::default(),
        );

        // No bank - no block cost set (0).
        assert_eq!(0, progress_tracker.remaining_block_cost());
        assert_eq!(0, progress_tracker.remaining_allocated_accounts_data_size());

        let block_limit = 10_000;
        progress_tracker.limit_and_shared_block_cost = Some((block_limit, SharedBlockCost::new(0)));
        assert_eq!(block_limit, progress_tracker.remaining_block_cost());
        progress_tracker.limit_and_shared_block_cost =
            Some((block_limit, SharedBlockCost::new(block_limit / 2)));
        assert_eq!(block_limit / 2, progress_tracker.remaining_block_cost());

        let allocated_accounts_data_size_limit = 20_000;
        progress_tracker.limit_and_shared_allocated_accounts_data_size = Some((
            allocated_accounts_data_size_limit,
            SharedAllocatedAccountsDataSize::new(0),
        ));
        assert_eq!(
            allocated_accounts_data_size_limit,
            progress_tracker.remaining_allocated_accounts_data_size()
        );
        progress_tracker.limit_and_shared_allocated_accounts_data_size = Some((
            allocated_accounts_data_size_limit,
            SharedAllocatedAccountsDataSize::new(allocated_accounts_data_size_limit / 2),
        ));
        assert_eq!(
            allocated_accounts_data_size_limit / 2,
            progress_tracker.remaining_allocated_accounts_data_size()
        );
    }
}
