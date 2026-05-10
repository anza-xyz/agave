use {
    super::PacketBundle,
    crate::hooks::BlockEngineConfig,
    agave_tpu_plugin::{LifecycleStage, TpuStage},
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc::SyncSender,
        },
        thread::{self, JoinHandle},
    },
};

pub struct BlockEngineStage {
    abort_signal: Arc<AtomicBool>,
    thread: thread::Thread,
    handle: Option<JoinHandle<()>>,
    #[allow(dead_code)]
    config: BlockEngineConfig,
}

impl BlockEngineStage {
    pub fn spawn(
        config: BlockEngineConfig,
        bundle_sender: SyncSender<PacketBundle>,
        _exit: Arc<AtomicBool>,
    ) -> Self {
        let abort_signal = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&abort_signal);
        let handle = thread::Builder::new()
            .name("jitoBlockEngineStage".to_string())
            .spawn(move || {
                while !signal.load(Ordering::Acquire) {
                    thread::park_timeout(std::time::Duration::from_millis(50));
                }
                drop(bundle_sender);
            })
            .expect("jitoBlockEngineStage spawn failed");
        let thread = handle.thread().clone();
        Self { abort_signal, thread, handle: Some(handle), config }
    }
}

impl LifecycleStage for BlockEngineStage {
    fn abort(&self) {
        self.abort_signal.store(true, Ordering::Release);
        self.thread.unpark();
    }
    fn join(mut self: Box<Self>) -> thread::Result<()> {
        self.abort();
        self.handle.take().map(|h| h.join()).unwrap_or(Ok(()))
    }
}

impl TpuStage for BlockEngineStage {}
