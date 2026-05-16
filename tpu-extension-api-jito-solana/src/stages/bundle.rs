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

/// Processing stage: executes verified bundles atomically while coordinating
/// with the packet scheduler via the gate and lock hooks.
///
/// This is the last stage in the bundle pipeline and the primary reason the
/// other extension hooks exist:
///
/// 1. Raises [`BundleSchedulerGate`] (`yield_flag`) so the packet scheduler
///    pauses while a bundle is in flight — preventing interleaving.
/// 2. Acquires [`BundleExternalLocks`] on each writable account so the
///    scheduler's account-conflict check blocks any concurrent Agave
///    transaction that would touch the same accounts.
/// 3. Executes the bundle (reference stub: counts packets only).
/// 4. Releases locks and lowers the gate so the scheduler resumes.
///
/// Registered via [`processing_stage`] so it is aborted *after* `BlockEngineStage`
/// and `BundleSigverifyStage` — giving in-flight bundles a chance to finish
/// before intake stops.
///
/// [`BundleSchedulerGate`]: crate::hooks::BundleSchedulerGate
/// [`BundleExternalLocks`]: crate::hooks::BundleExternalLocks
/// [`processing_stage`]: agave_tpu_extension_api::TpuExtensionsBuilder::processing_stage
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
