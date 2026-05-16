use {
    super::PacketBundle,
    agave_tpu_extension_api::{LifecycleStage, TpuStage},
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc::SyncSender,
        },
        thread::{self, JoinHandle},
    },
};

/// Connection parameters for the external block-engine service.
///
/// In production Jito-Solana this drives a gRPC streaming connection to
/// a Jito block-engine endpoint that pushes bundles to the validator.
/// `trust_packets` controls whether the block engine's packet signatures
/// are trusted without local re-verification.
#[allow(dead_code)]
pub struct BlockEngineConfig {
    pub block_engine_url: String,
    pub trust_packets: bool,
}

/// Intake stage: receives bundles from an external block engine and forwards
/// them into the sigverify pipeline.
///
/// This is the first stage in the bundle pipeline:
/// `BlockEngineStage` → `BundleSigverifyStage` → `BundleStage`.
///
/// In production it maintains a persistent gRPC stream to the block-engine
/// endpoint and deserializes incoming bundle protos into the internal bundle
/// type. The reference stub parks its thread until abort is signalled.
///
/// Aborted first on shutdown (last registered via [`intake_stage`]) so new
/// bundle intake stops before the executor drains.
///
/// [`intake_stage`]: agave_tpu_extension_api::TpuExtensionsBuilder::intake_stage
pub struct BlockEngineStage {
    abort_signal: Arc<AtomicBool>,
    thread: thread::Thread,
    handle: Option<JoinHandle<()>>,
    #[allow(dead_code)]
    config: BlockEngineConfig,
}

impl BlockEngineStage {
    pub fn spawn(config: BlockEngineConfig, bundle_sender: SyncSender<PacketBundle>) -> Self {
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
        Self {
            abort_signal,
            thread,
            handle: Some(handle),
            config,
        }
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
