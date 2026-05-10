use {
    super::PacketBundle,
    crate::hooks::{BundleLocks, TipManager},
    agave_tpu_plugin::{LifecycleStage, TpuStage},
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc::Receiver,
        },
        thread::{self, JoinHandle},
    },
};

pub struct BundleStage {
    abort_signal: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    #[allow(dead_code)]
    locks: Arc<BundleLocks>,
    #[allow(dead_code)]
    tip_manager: Arc<TipManager>,
}

impl BundleStage {
    pub fn spawn(
        receiver: Receiver<PacketBundle>,
        locks: Arc<BundleLocks>,
        tip_manager: Arc<TipManager>,
        _exit: Arc<AtomicBool>,
    ) -> Self {
        let abort_signal = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&abort_signal);
        let handle = thread::Builder::new()
            .name("jitoBundleStage".to_string())
            .spawn(move || {
                while receiver.recv().is_ok() {
                    if signal.load(Ordering::Acquire) {
                        break;
                    }
                    // stub: production executes set yield_flag → lock accounts → execute → unlock
                }
            })
            .expect("jitoBundleStage spawn failed");
        Self { abort_signal, handle: Some(handle), locks, tip_manager }
    }
}

impl LifecycleStage for BundleStage {
    fn abort(&self) {
        self.abort_signal.store(true, Ordering::Release);
    }
    fn join(mut self: Box<Self>) -> thread::Result<()> {
        self.abort();
        self.handle.take().map(|h| h.join()).unwrap_or(Ok(()))
    }
}

impl TpuStage for BundleStage {}
