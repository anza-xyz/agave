#![cfg(feature = "agave-unstable-api")]
#![cfg(unix)]

use {
    agave_scheduler_bindings::{
        CheckWorkerToPackMessage, PackToCheckWorkerMessage, ProgressMessage, TpuToPackMessage,
    },
    agave_scheduler_handshake::{
        ClientCheckWorkerSession, ClientHandshakeError, ClientLogon, ClientSession,
        ClientWorkerSession, client,
    },
    rts_alloc::Allocator,
    std::{
        path::PathBuf,
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    },
};

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
    /// Minimum execution-worker-to-scheduler queue capacity in messages.
    pub worker_to_pack_capacity: usize,
    /// Minimum scheduler-to-check-worker queue capacity in messages.
    pub pack_to_check_worker_capacity: usize,
    /// Minimum check-worker-to-scheduler queue capacity in messages.
    pub check_worker_to_pack_capacity: usize,
}

#[expect(
    dead_code,
    reason = "resources retained for the scheduler loop implementation"
)]
struct Scheduler {
    allocator: Allocator,
    tpu_receiver: shaq::spsc::Consumer<TpuToPackMessage>,
    progress_receiver: shaq::spsc::Consumer<ProgressMessage>,
    check_sender: shaq::mpmc::Producer<PackToCheckWorkerMessage>,
    check_receiver: shaq::mpmc::Consumer<CheckWorkerToPackMessage>,
    workers: Vec<ClientWorkerSession>,
}

impl Scheduler {
    fn new(session: ClientSession) -> Result<Self, ClientHandshakeError> {
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
            allocator,
            tpu_receiver: tpu_to_pack,
            progress_receiver: progress_tracker,
            check_sender: pack_to_check_worker,
            check_receiver: check_worker_to_pack,
            workers,
        })
    }

    fn run_iteration(&mut self) {
        std::hint::spin_loop();
    }
}

/// Connects to Agave and runs the scheduler loop on the calling thread until exit is set.
///
/// If exit is already set, returns without connecting. Otherwise, attempts the handshake once
/// and returns any error to the caller. Shared resources remain alive until the loop exits.
/// The placeholder loop spins without processing queues.
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
    let mut scheduler = Scheduler::new(client::connect(
        config.ipc_path,
        logon,
        config.handshake_timeout,
    )?)?;

    while !exit.load(Ordering::Relaxed) {
        scheduler.run_iteration();
    }

    Ok(())
}
