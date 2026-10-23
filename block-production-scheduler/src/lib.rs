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
        ClientCheckWorkerSession, ClientHandshakeError, ClientSession, ClientWorkerSession, client,
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
    std::{collections::HashSet, io::ErrorKind, thread, time::Instant},
};

const CONNECTION_RETRY_INTERVAL: Duration = Duration::from_millis(100);

mod check_response;
mod config;
mod execution_response;
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

pub use config::{Config, SchedulerConfig, SessionConfig};

struct Scheduler {
    state: SchedulerState,
    scheduling_slot: Option<Slot>,
    cost_pacer: Option<CostPacer>,
    /// Estimated CUs dispatched in the current scheduling slot, used for pacing.
    scheduled_cost: u64,
    /// Transactions held out of the priority queue until the next scheduling slot.
    deferred_execution: Vec<usize>,
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
            deferred_execution: Vec::with_capacity(transaction_state_capacity),
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
        self.handle_execution_worker_responses();
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
        for id in self.deferred_execution.drain(..) {
            self.transactions.requeue(id);
        }
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
/// Missing or refused connections are retried until Agave is available or exit is set.
/// Other errors are returned to the caller. Shared resources remain alive until the loop exits.
/// The loop receives leader progress, checks and schedules transactions, and handles completion.
///
/// The exit flag cannot interrupt an in-progress handshake. The timeout has the syscall-level
/// semantics of [`client::connect`], rather than imposing a deadline on the entire handshake.
pub fn run(config: Config, exit: &AtomicBool) -> Result<(), ClientHandshakeError> {
    while !exit.load(Ordering::Relaxed) {
        if let Some(session) = try_connect(&config)? {
            return run_session(session, config.scheduler, exit);
        }
        thread::sleep(CONNECTION_RETRY_INTERVAL);
    }
    Ok(())
}

/// Runs the scheduler on the calling thread using an established local or external session.
pub fn run_session(
    session: ClientSession,
    config: SchedulerConfig,
    exit: &AtomicBool,
) -> Result<(), ClientHandshakeError> {
    let mut scheduler = Scheduler::new(
        session,
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

/// Returns `None` when Agave is not yet listening.
fn try_connect(config: &Config) -> Result<Option<ClientSession>, ClientHandshakeError> {
    // Resolve on every attempt: Agave replaces the ledger symlink when it starts.
    let session = config
        .ipc_path
        .canonicalize()
        .map_err(ClientHandshakeError::from)
        .and_then(|path| {
            client::connect(
                path,
                config.session.client_logon(),
                config.session.handshake_timeout,
            )
        });
    match session {
        Ok(session) => Ok(Some(session)),
        Err(ClientHandshakeError::Io(error))
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
