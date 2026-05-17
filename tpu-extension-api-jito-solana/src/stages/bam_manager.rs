use {
    agave_tpu_extension_api::{LifecycleStage, TpuStage},
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::Duration,
    },
};

/// Coordinator thread for Jito's Bundle-Aware Mempool (BAM).
///
/// `BamManager` is registered as a [`TpuStage`] (lifecycle management only).
/// The actual banking workers that drain transactions are spawned separately by
/// [`BamWorkerPoolFactory`](crate::hooks::BamWorkerPoolFactory) via
/// [`BankingWorkerPoolFactory::spawn_threads`](agave_tpu_extension_api::BankingWorkerPoolFactory::spawn_threads)
/// with [`BankingSchedulerMode::ReplaceInternal`](agave_tpu_extension_api::BankingSchedulerMode::ReplaceInternal),
/// which tells Agave not to start its own non-vote scheduler.
///
/// Production: this thread manages a shared priority queue, coordinates slot
/// transitions from the banking-stage context, and signals BAM workers.
/// Reference stub: parks until aborted.
pub struct BamManager {
    abort_signal: Arc<AtomicBool>,
    thread: thread::Thread,
    handle: Option<JoinHandle<()>>,
}

impl BamManager {
    pub fn spawn() -> Self {
        let abort_signal = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&abort_signal);
        let handle = thread::Builder::new()
            .name("jitoBamManager".to_string())
            .spawn(move || {
                while !signal.load(Ordering::Acquire) {
                    thread::park_timeout(Duration::from_millis(50));
                }
            })
            .expect("jitoBamManager spawn failed");
        let thread = handle.thread().clone();
        Self {
            abort_signal,
            thread,
            handle: Some(handle),
        }
    }
}

impl LifecycleStage for BamManager {
    fn abort(&self) {
        self.abort_signal.store(true, Ordering::Release);
        self.thread.unpark();
    }

    fn join(mut self: Box<Self>) -> thread::Result<()> {
        self.abort();
        self.handle.take().map(|h| h.join()).unwrap_or(Ok(()))
    }
}

impl TpuStage for BamManager {}
