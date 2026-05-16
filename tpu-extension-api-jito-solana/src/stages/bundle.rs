use {
    super::PacketBundle,
    crate::hooks::BundleExternalLocks,
    agave_tpu_extension_api::{LifecycleStage, TpuStage},
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc::Receiver,
        },
        thread::{self, JoinHandle},
    },
};

/// Executes verified bundles atomically.
///
/// For each bundle: raises the scheduler gate, acquires write locks on the
/// bundle's accounts, executes (reference stub: no-op), then releases locks
/// and lowers the gate. The gate and locks together prevent the packet
/// scheduler from interleaving regular transactions during execution.
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

                    scheduler_gate.store(true, Ordering::Release);
                    for account in &bundle.write_locks {
                        locks.lock(*account);
                    }

                    // Reference mock: production executes the bundle packets here.
                    let _packet_count = bundle.packets.len();

                    for account in &bundle.write_locks {
                        locks.unlock(account);
                    }
                    scheduler_gate.store(false, Ordering::Release);
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
