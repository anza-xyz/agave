#![cfg(feature = "agave-unstable-api")]
#![cfg(unix)]

use {
    crate::{
        check_response::TransactionState, in_flight_tracker::InFlightTracker,
        progress_tracker::SchedulerState, schedule::SchedulerScratch,
        transaction_container::TransactionContainer,
    },
    agave_reserved_account_keys::ReservedAccountKeys,
    agave_scheduler_bindings::{
        CheckWorkerToPackMessage, PackToCheckWorkerMessage, ProgressMessage, TpuToPackMessage,
    },
    agave_scheduler_handshake::{
        ClientCheckWorkerSession, ClientHandshakeError, ClientLogon, ClientSession,
        ClientWorkerSession, client,
    },
    agave_scheduling_utils::{
        cost_pacer::CostPacer, thread_aware_account_locks::ThreadAwareAccountLocks,
    },
    core::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    },
    rts_alloc::Allocator,
    solana_clock::Slot,
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::{collections::HashSet, path::PathBuf, time::Instant},
};

mod check_response;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "used by upcoming execution dispatch and response handling"
    )
)]
mod in_flight_tracker;
mod progress_tracker;
mod resolved_transaction;
mod schedule;
mod tpu_ingress;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "additional container operations are used by upcoming execution handling"
    )
)]
mod transaction_container;

#[cfg(test)]
mod tests;

/// Configuration for a scheduler running in a thread or a separate process.
#[derive(Debug, Clone)]
pub struct Config {
    /// Path to Agave's scheduler handshake socket.
    pub ipc_path: PathBuf,
    /// Timeout for handshake reads and writes.
    pub handshake_timeout: Duration,
    /// Number of execution workers requested from Agave.
    pub worker_count: usize,
    /// Number of check workers requested from Agave.
    pub check_worker_count: usize,
    /// Minimum shared allocator size in bytes.
    pub allocator_size: usize,
    /// Number of allocator handles requested by this scheduler.
    pub allocator_handles: usize,
    /// Minimum TPU-to-scheduler queue capacity in messages.
    pub tpu_to_pack_capacity: usize,
    /// Minimum progress queue capacity in messages.
    pub progress_tracker_capacity: usize,
    /// Minimum scheduler-to-execution-worker queue capacity in messages.
    pub pack_to_worker_capacity: usize,
    /// Outstanding estimated CU target per worker, including pending batches.
    /// The last assigned transaction may cross this target.
    pub max_cost_units_per_worker: u64,
    /// Estimated CU target per execution batch, checked after adding each transaction.
    pub max_cost_units_per_batch: u64,
    /// Target serialized entry bytes per execution batch, including entry overhead.
    pub target_entry_bytes_per_batch: u64,
    /// Minimum execution-worker-to-scheduler queue capacity in messages.
    pub worker_to_pack_capacity: usize,
    /// Minimum scheduler-to-check-worker queue capacity in messages.
    pub pack_to_check_worker_capacity: usize,
    /// Minimum check-worker-to-scheduler queue capacity in messages.
    pub check_worker_to_pack_capacity: usize,
    /// Maximum number of checked transactions retained for scheduling.
    pub transaction_state_capacity: usize,
    /// Time before slot end by which pacing releases the full cost budget.
    pub execution_margin: Duration,
}

struct Scheduler {
    state: SchedulerState,
    scheduling_slot: Option<Slot>,
    cost_pacer: Option<CostPacer>,
    /// Estimated CUs dispatched in the current scheduling slot, used for pacing.
    scheduled_cost: u64,
    execution_margin: Duration,
    allocator: Allocator,
    tpu_receiver: shaq::spsc::Consumer<TpuToPackMessage>,
    progress_receiver: shaq::spsc::Consumer<ProgressMessage>,
    check_sender: shaq::mpmc::Producer<PackToCheckWorkerMessage>,
    outstanding_check_packets: usize,
    transactions: TransactionContainer<TransactionState>,
    scheduling_scratch: SchedulerScratch,
    reserved_account_keys: HashSet<Pubkey, PubkeyHasherBuilder>,
    check_receiver: shaq::mpmc::Consumer<CheckWorkerToPackMessage>,
    workers: Vec<ClientWorkerSession>,
    in_flight: InFlightTracker,
    account_locks: ThreadAwareAccountLocks,
    max_cost_units_per_worker: u64,
    max_cost_units_per_batch: u64,
    target_entry_bytes_per_batch: u64,
}

impl Scheduler {
    fn new(
        session: ClientSession,
        transaction_state_capacity: usize,
        execution_margin: Duration,
        max_cost_units_per_worker: u64,
        max_cost_units_per_batch: u64,
        target_entry_bytes_per_batch: u64,
    ) -> Result<Self, ClientHandshakeError> {
        let ClientSession {
            allocator,
            tpu_to_pack,
            progress_tracker,
            workers,
            check_workers,
            ..
        } = session;
        let ClientCheckWorkerSession {
            pack_to_check_worker,
            check_worker_to_pack,
        } = check_workers.ok_or(ClientHandshakeError::ProtocolViolation)?;

        Ok(Self {
            state: SchedulerState::new(),
            scheduling_slot: None,
            cost_pacer: None,
            scheduled_cost: 0,
            execution_margin,
            allocator,
            tpu_receiver: tpu_to_pack,
            progress_receiver: progress_tracker,
            check_sender: pack_to_check_worker,
            outstanding_check_packets: 0,
            transactions: TransactionContainer::with_capacity(transaction_state_capacity),
            scheduling_scratch: SchedulerScratch::new(transaction_state_capacity),
            reserved_account_keys: ReservedAccountKeys::default().active.into_iter().collect(),
            check_receiver: check_worker_to_pack,
            in_flight: InFlightTracker::new(workers.len()),
            account_locks: ThreadAwareAccountLocks::new(workers.len()),
            max_cost_units_per_worker,
            max_cost_units_per_batch,
            target_entry_bytes_per_batch,
            workers,
        })
    }

    fn run_iteration(&mut self) {
        self.handle_leader_progress();
        self.handle_slot_change();
        self.handle_check_worker_responses();
        self.handle_tpu_ingress();
        let budget = self.pacing_budget(Instant::now(), self.scheduled_cost);
        self.schedule(budget);
        core::hint::spin_loop();
    }

    fn handle_leader_progress(&mut self) {
        self.state.drain_progress(&mut self.progress_receiver);
    }

    fn handle_slot_change(&mut self) {
        let SchedulerState::LeaderReady {
            slot,
            remaining_cost_units,
            slot_start,
            slot_duration,
        } = self.state
        else {
            return;
        };
        // Seed the slot once. Later snapshots can include our own reservations and must not
        // replace the initial pacing budget.
        if self.scheduling_slot == Some(slot) || !self.in_flight.is_empty() {
            return;
        }

        self.scheduling_slot = Some(slot);
        self.scheduled_cost = 0;
        self.cost_pacer = Some(CostPacer::new(
            remaining_cost_units,
            slot_start,
            slot_duration.saturating_sub(self.execution_margin),
        ));
    }

    fn pacing_budget(&self, now: Instant, consumed_cost: u64) -> u64 {
        if !matches!(self.state, SchedulerState::LeaderReady { slot, .. } if self.scheduling_slot == Some(slot))
        {
            return 0;
        }
        self.cost_pacer
            .as_ref()
            .map_or(0, |pacer| pacer.available_budget(now, consumed_cost))
    }
}

/// Connects to Agave and runs the scheduler loop on the calling thread until exit is set.
///
/// If exit is already set, returns without connecting. Otherwise, attempts the handshake once
/// and returns any error to the caller. Shared resources remain alive until the loop exits.
/// The loop receives leader progress, dispatches TPU packets, and retains checked transactions.
///
/// The exit flag cannot interrupt an in-progress handshake. The timeout has the syscall-level
/// semantics of [`client::connect`], rather than imposing a deadline on the entire handshake.
pub fn run(config: Config, exit: &AtomicBool) -> Result<(), ClientHandshakeError> {
    if exit.load(Ordering::Relaxed) {
        return Ok(());
    }

    let logon = ClientLogon {
        worker_count: config.worker_count,
        check_worker_count: config.check_worker_count,
        simulate_worker_count: 0,
        allocator_size: config.allocator_size,
        allocator_handles: config.allocator_handles,
        tpu_to_pack_capacity: config.tpu_to_pack_capacity,
        progress_tracker_capacity: config.progress_tracker_capacity,
        pack_to_worker_capacity: config.pack_to_worker_capacity,
        worker_to_pack_capacity: config.worker_to_pack_capacity,
        pack_to_check_worker_capacity: config.pack_to_check_worker_capacity,
        check_worker_to_pack_capacity: config.check_worker_to_pack_capacity,
        pack_to_simulate_worker_capacity: 0,
        simulate_worker_to_pack_capacity: 0,
    };
    let mut scheduler = Scheduler::new(
        client::connect(config.ipc_path, logon, config.handshake_timeout)?,
        config.transaction_state_capacity,
        config.execution_margin,
        config.max_cost_units_per_worker,
        config.max_cost_units_per_batch,
        config.target_entry_bytes_per_batch,
    )?;

    while !exit.load(Ordering::Relaxed) {
        scheduler.run_iteration();
    }

    Ok(())
}
