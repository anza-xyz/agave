use {
    super::PacketBundle,
    crate::hooks::BundleExternalLocks,
    agave_tpu_extension_api::{
        BundleExecution, BundleExecutionRequest, LifecycleStage, TpuStage, TpuStageContext,
        TpuStageFactory,
    },
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc::Receiver,
        },
        thread::{self, JoinHandle},
    },
};

/// Builds `BundleStage` after Agave exposes its bundle execution runtime.
pub struct BundleStageFactory {
    receiver: Receiver<PacketBundle>,
    yield_flag: Arc<AtomicBool>,
    locks: Arc<BundleExternalLocks>,
}

impl BundleStageFactory {
    pub fn new(
        receiver: Receiver<PacketBundle>,
        yield_flag: Arc<AtomicBool>,
        locks: Arc<BundleExternalLocks>,
    ) -> Self {
        Self {
            receiver,
            yield_flag,
            locks,
        }
    }
}

impl TpuStageFactory for BundleStageFactory {
    fn spawn(self: Box<Self>, context: &dyn TpuStageContext) -> Box<dyn TpuStage> {
        Box::new(BundleStage::spawn(
            self.receiver,
            self.yield_flag,
            self.locks,
            context.bundle_execution(),
        ))
    }
}

/// Executes verified bundles atomically.
///
/// For each bundle: raises the scheduler gate, acquires write locks on the
/// bundle's accounts, asks Agave's bundle runtime to execute and record it,
/// then releases locks and lowers the gate. The gate and locks together prevent
/// the packet scheduler from interleaving regular transactions during execution.
pub struct BundleStage {
    abort_signal: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    yield_flag: Arc<AtomicBool>,
}

impl BundleStage {
    pub fn spawn(
        receiver: Receiver<PacketBundle>,
        yield_flag: Arc<AtomicBool>,
        locks: Arc<BundleExternalLocks>,
        bundle_execution: Arc<dyn BundleExecution>,
    ) -> Self {
        let abort_signal = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&abort_signal);
        let scheduler_gate = Arc::clone(&yield_flag);
        let handle = thread::Builder::new()
            .name("jitoBundleStage".to_string())
            .spawn(move || {
                while let Ok(bundle) = receiver.recv() {
                    if signal.load(Ordering::Acquire) {
                        break;
                    }

                    let _gate = SchedulerGateGuard::raise(Arc::clone(&scheduler_gate));
                    let _lock_guard = BundleLockGuard::lock(
                        Arc::clone(&locks),
                        bundle.read_locks.clone(),
                        bundle.write_locks.clone(),
                    );

                    let _status =
                        bundle_execution.execute_and_record_bundle(BundleExecutionRequest::new(
                            Arc::clone(&bundle.packets),
                            bundle.read_locks.clone(),
                            bundle.write_locks.clone(),
                        ));
                }
            })
            .expect("jitoBundleStage spawn failed");
        Self {
            abort_signal,
            handle: Some(handle),
            yield_flag,
        }
    }
}

impl LifecycleStage for BundleStage {
    fn abort(&self) {
        self.abort_signal.store(true, Ordering::Release);
        self.yield_flag.store(false, Ordering::Release);
    }
    fn join(mut self: Box<Self>) -> thread::Result<()> {
        self.abort();
        self.handle.take().map(|h| h.join()).unwrap_or(Ok(()))
    }
}

impl TpuStage for BundleStage {}

struct SchedulerGateGuard {
    yield_flag: Arc<AtomicBool>,
}

impl SchedulerGateGuard {
    fn raise(yield_flag: Arc<AtomicBool>) -> Self {
        yield_flag.store(true, Ordering::Release);
        Self { yield_flag }
    }
}

impl Drop for SchedulerGateGuard {
    fn drop(&mut self) {
        self.yield_flag.store(false, Ordering::Release);
    }
}

struct BundleLockGuard {
    locks: Arc<BundleExternalLocks>,
    read_locks: Vec<solana_pubkey::Pubkey>,
    write_locks: Vec<solana_pubkey::Pubkey>,
}

impl BundleLockGuard {
    fn lock(
        locks: Arc<BundleExternalLocks>,
        read_locks: Vec<solana_pubkey::Pubkey>,
        write_locks: Vec<solana_pubkey::Pubkey>,
    ) -> Self {
        for account in &read_locks {
            locks.lock_read(*account);
        }
        for account in &write_locks {
            locks.lock_write(*account);
        }
        Self {
            locks,
            read_locks,
            write_locks,
        }
    }
}

impl Drop for BundleLockGuard {
    fn drop(&mut self) {
        for account in &self.write_locks {
            self.locks.unlock_write(account);
        }
        for account in &self.read_locks {
            self.locks.unlock_read(account);
        }
    }
}
