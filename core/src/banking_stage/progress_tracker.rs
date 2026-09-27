//! Service to send progress updates to the external scheduler.
//!

use {
    crate::banking_stage::consume_worker::ConsumeWorkerMetrics,
    agave_scheduler_bindings::ProgressMessage,
    agave_votor::slot_clock::SharedAlpenglowSlotClock,
    agave_votor_messages::migration::MigrationStatus,
    solana_clock::{BankId, Slot},
    solana_cost_model::cost_tracker::{SharedAllocatedAccountsDataSize, SharedBlockCost},
    solana_poh::poh_recorder::SharedLeaderState,
    solana_runtime::leader_schedule_utils::last_of_consecutive_leader_slots,
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::JoinHandle,
        time::{Duration, Instant},
    },
};

const ALPENGLOW_PROGRESS_STEP: u8 = 5;
const ALPENGLOW_PROGRESS_SPIN_DURATION: Duration = Duration::from_millis(1);

/// Spawns a thread to track and send progress updates.
pub fn spawn(
    exit: Arc<AtomicBool>,
    mut producer: shaq::spsc::Producer<ProgressMessage>,
    shared_leader_state: SharedLeaderState,
    worker_metrics: Vec<Arc<ConsumeWorkerMetrics>>,
    ticks_per_slot: u64,
    migration_status: Arc<MigrationStatus>,
    alpenglow_slot_clock: SharedAlpenglowSlotClock,
) -> JoinHandle<()> {
    std::thread::Builder::new()
        .name("solProgTrker".to_string())
        .spawn(move || {
            ProgressTracker::new(
                exit,
                shared_leader_state,
                worker_metrics,
                ticks_per_slot,
                migration_status,
                alpenglow_slot_clock,
            )
            .run(&mut producer);
        })
        .unwrap()
}

struct ProgressTracker {
    exit: Arc<AtomicBool>,
    shared_leader_state: SharedLeaderState,
    worker_metrics: Vec<Arc<ConsumeWorkerMetrics>>,
    ticks_per_slot: u64,
    migration_status: Arc<MigrationStatus>,
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
        ticks_per_slot: u64,
        migration_status: Arc<MigrationStatus>,
        alpenglow_slot_clock: SharedAlpenglowSlotClock,
    ) -> Self {
        Self {
            exit,
            shared_leader_state,
            worker_metrics,
            ticks_per_slot,
            migration_status,
            alpenglow_slot_clock,

            last_observed_bank_id: None,
            limit_and_shared_block_cost: None,
            limit_and_shared_allocated_accounts_data_size: None,
        }
    }

    fn run(mut self, producer: &mut shaq::spsc::Producer<ProgressMessage>) {
        let mut last_published_progress = None;
        while !self.exit.load(Ordering::Relaxed) {
            if let Some((message, tick_height)) = self.produce_progress_message() {
                let progress = (
                    tick_height,
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
                .for_each(|metrics| metrics.maybe_report_and_reset());

            self.wait_for_next_progress_boundary();
        }
    }

    fn wait_for_next_progress_boundary(&self) {
        let deadline = if self.migration_status.is_alpenglow_enabled() {
            self.alpenglow_slot_clock.load().and_then(|slot_info| {
                next_alpenglow_progress_deadline(
                    slot_info.started_at,
                    slot_info.slot_duration,
                    Instant::now(),
                )
            })
        } else {
            None
        };
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
    fn produce_progress_message(&mut self) -> Option<(ProgressMessage, u64)> {
        let leader_state = self.shared_leader_state.load();
        let tick_height = leader_state.tick_height();
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

            let current_slot_progress = if self.migration_status.is_alpenglow_enabled() {
                self.alpenglow_slot_clock
                    .load()
                    .map(|slot_info| {
                        alpenglow_bank_progress(
                            working_bank.slot(),
                            slot_info.slot,
                            slot_info.started_at.elapsed(),
                            Duration::from_nanos_u128(working_bank.ns_per_slot),
                        )
                    })
                    .unwrap_or(0)
            } else {
                progress(working_bank.slot(), tick_height, self.ticks_per_slot)
            };

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
            let (current_slot, current_slot_progress) =
                if self.migration_status.is_alpenglow_enabled() {
                    let slot_info = self.alpenglow_slot_clock.load()?;
                    alpenglow_slot_progress(
                        slot_info.slot,
                        slot_info.started_at.elapsed(),
                        slot_info.slot_duration,
                    )
                } else {
                    let current_slot = slot_from_tick_height(tick_height, self.ticks_per_slot);
                    (
                        current_slot,
                        progress(current_slot, tick_height, self.ticks_per_slot),
                    )
                };

            // No bank yet but we may already be inside our leader window.
            let leader_state =
                if (next_leader_range_start..=next_leader_range_end).contains(&current_slot) {
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

        Some((progress_message, tick_height))
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

fn alpenglow_progress(elapsed: Duration, slot_duration: Duration) -> u8 {
    if slot_duration.is_zero() {
        return 100;
    }

    let percentage = elapsed
        .as_nanos()
        .saturating_mul(100)
        .saturating_div(slot_duration.as_nanos())
        .min(100) as u8;
    (percentage / ALPENGLOW_PROGRESS_STEP) * ALPENGLOW_PROGRESS_STEP
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

fn alpenglow_slot_progress(
    window_start_slot: Slot,
    elapsed: Duration,
    slot_duration: Duration,
) -> (Slot, u8) {
    let window_end_slot = last_of_consecutive_leader_slots(window_start_slot);
    if slot_duration.is_zero() {
        return (window_end_slot, 100);
    }

    let elapsed_nanos = elapsed.as_nanos();
    let slot_duration_nanos = slot_duration.as_nanos();
    let elapsed_slots = elapsed_nanos / slot_duration_nanos;
    let window_slot_offset = u128::from(window_end_slot - window_start_slot);
    if elapsed_slots > window_slot_offset {
        return (window_end_slot, 100);
    }

    let current_slot = window_start_slot + elapsed_slots as Slot;
    let elapsed_in_slot = Duration::from_nanos_u128(elapsed_nanos % slot_duration_nanos);
    (
        current_slot,
        alpenglow_progress(elapsed_in_slot, slot_duration),
    )
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
        super::*, solana_clock::DEFAULT_TICKS_PER_SLOT,
        solana_epoch_schedule::MINIMUM_SLOTS_PER_EPOCH, solana_leader_schedule::SlotLeader,
        solana_poh::poh_recorder::LeaderState, solana_runtime::bank::Bank,
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
            Arc::default(),
            SharedAlpenglowSlotClock::default(),
        );

        let (message, tick_height) = progress_tracker.produce_progress_message().unwrap();
        assert_eq!(tick_height, 0);
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
        let (message, tick_height) = progress_tracker.produce_progress_message().unwrap();
        assert_eq!(tick_height, expected_tick_height);
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
        let (message, tick_height) = progress_tracker.produce_progress_message().unwrap();
        assert_eq!(tick_height, expected_tick_height);
        assert_eq!(message.leader_state, agave_scheduler_bindings::NOT_LEADER);
        assert_eq!(message.current_slot, 2);
        assert_eq!(message.next_leader_slot, 4);
        assert_eq!(message.leader_range_end, 7);
        assert_eq!(message.current_slot_progress, 0);
        assert_eq!(message.epoch, 0);
        assert_eq!(message.latest_blockhash, [0; 32]);
        assert_eq!(message.target_bank_time_ms, 0);

        // In leader slot but no bank yet - should be LEADER_STARTING.
        // leader_first_tick_height is at start of slot 4, and we're at tick_height
        // that puts us in slot 4.
        let leader_first_tick = 4 * ticks_per_slot + 1;
        shared_leader_state.store(Arc::new(LeaderState::new(
            None,
            leader_first_tick, // tick_height >= leader_first_tick_height
            Some(leader_first_tick),
            Some((4, 7)),
        )));
        let (message, tick_height) = progress_tracker.produce_progress_message().unwrap();
        assert_eq!(tick_height, leader_first_tick);
        assert_eq!(
            message.leader_state,
            agave_scheduler_bindings::LEADER_STARTING
        );
        assert_eq!(message.current_slot, 4);
        assert_eq!(message.next_leader_slot, 4);
        assert_eq!(message.leader_range_end, 7);
        assert_eq!(message.current_slot_progress, 1);
        assert_eq!(message.epoch, 0);
        assert_eq!(message.latest_blockhash, [0; 32]);
        assert_eq!(message.target_bank_time_ms, 0);

        // Slot boundary mid-window: tick_height one tick before leader_first_tick_height.
        let slot_5_boundary = 5 * ticks_per_slot;
        shared_leader_state.store(Arc::new(LeaderState::new(
            None,
            slot_5_boundary,
            Some(slot_5_boundary + 1),
            Some((5, 7)),
        )));
        let (message, _) = progress_tracker.produce_progress_message().unwrap();
        assert_eq!(message.current_slot, 5);
        assert_eq!(
            message.leader_state,
            agave_scheduler_bindings::LEADER_STARTING
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
        let (message, tick_height) = progress_tracker.produce_progress_message().unwrap();
        assert_eq!(tick_height, bank.tick_height());
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
        let (message, tick_height) = progress_tracker.produce_progress_message().unwrap();
        assert_eq!(tick_height, bank.tick_height());
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
        let (message, tick_height) = progress_tracker.produce_progress_message().unwrap();
        assert_eq!(tick_height, child_bank.tick_height());
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
    fn test_alpenglow_progress() {
        let slot_duration = Duration::from_millis(400);
        assert_eq!(alpenglow_progress(Duration::ZERO, slot_duration), 0);
        assert_eq!(
            alpenglow_progress(Duration::from_millis(19), slot_duration),
            0
        );
        assert_eq!(
            alpenglow_progress(Duration::from_millis(20), slot_duration),
            5
        );
        assert_eq!(
            alpenglow_progress(Duration::from_millis(399), slot_duration),
            95
        );
        assert_eq!(alpenglow_progress(slot_duration, slot_duration), 100);
        assert_eq!(
            alpenglow_progress(Duration::from_millis(800), slot_duration),
            100
        );
        assert_eq!(alpenglow_progress(Duration::ZERO, Duration::ZERO), 100);
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
    fn test_alpenglow_slot_progress() {
        let slot_duration = Duration::from_millis(400);
        assert_eq!(
            alpenglow_slot_progress(4, Duration::ZERO, slot_duration),
            (4, 0)
        );
        assert_eq!(
            alpenglow_slot_progress(4, Duration::from_millis(399), slot_duration),
            (4, 95)
        );
        assert_eq!(
            alpenglow_slot_progress(4, slot_duration, slot_duration),
            (5, 0)
        );
        assert_eq!(
            alpenglow_slot_progress(4, Duration::from_millis(800), slot_duration),
            (6, 0)
        );
        assert_eq!(
            alpenglow_slot_progress(4, Duration::from_millis(1_599), slot_duration),
            (7, 95)
        );
        assert_eq!(
            alpenglow_slot_progress(4, Duration::from_millis(1_600), slot_duration),
            (7, 100)
        );
        assert_eq!(
            alpenglow_slot_progress(4, Duration::from_millis(200), Duration::from_millis(200)),
            (5, 0)
        );
        assert_eq!(
            alpenglow_slot_progress(4, Duration::ZERO, Duration::ZERO),
            (7, 100)
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
        let alpenglow_slot_clock = SharedAlpenglowSlotClock::default();
        let mut progress_tracker = ProgressTracker::new(
            Arc::default(),
            SharedLeaderState::new(0, None, Some((4, 7))),
            vec![],
            DEFAULT_TICKS_PER_SLOT,
            Arc::new(MigrationStatus::post_migration_status()),
            alpenglow_slot_clock.clone(),
        );

        assert!(progress_tracker.produce_progress_message().is_none());

        alpenglow_slot_clock.update(4, Instant::now(), Duration::from_secs(10));
        let (message, tick_height) = progress_tracker.produce_progress_message().unwrap();
        assert_eq!(tick_height, 0);
        assert_eq!(
            message.leader_state,
            agave_scheduler_bindings::LEADER_STARTING
        );
        assert_eq!(message.current_slot, 4);
        assert_eq!(message.current_slot_progress, 0);
        assert_eq!(message.next_leader_slot, 4);
        assert_eq!(message.leader_range_end, 7);
    }

    #[test]
    fn test_progress_tracker_remaining_costs() {
        let mut progress_tracker = ProgressTracker::new(
            Arc::default(),
            SharedLeaderState::new(0, None, None),
            vec![],
            DEFAULT_TICKS_PER_SLOT,
            Arc::default(),
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
