use {
    super::PacketBundle,
    agave_tpu_extension_api::{
        LifecycleStage, PacketIngress, TpuStage, TpuStageContext, TpuStageFactory,
    },
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc::SyncSender,
        },
        thread::{self, JoinHandle},
    },
};

/// Connection parameters for the external block-engine gRPC endpoint.
#[allow(dead_code)]
pub struct BlockEngineConfig {
    pub block_engine_url: String,
    pub trust_packets: bool,
}

/// Builds `BlockEngineStage` after Agave exposes packet ingress.
pub struct BlockEngineStageFactory {
    config: BlockEngineConfig,
    bundle_sender: SyncSender<PacketBundle>,
}

impl BlockEngineStageFactory {
    pub fn new(config: BlockEngineConfig, bundle_sender: SyncSender<PacketBundle>) -> Self {
        Self {
            config,
            bundle_sender,
        }
    }
}

impl TpuStageFactory for BlockEngineStageFactory {
    fn spawn(self: Box<Self>, context: &dyn TpuStageContext) -> Box<dyn TpuStage> {
        Box::new(BlockEngineStage::spawn(
            self.config,
            self.bundle_sender,
            context.packet_ingress().clone(),
            context.exit(),
        ))
    }
}

/// Receives bundles and packets from the block-engine gRPC stream.
///
/// First stage in `BlockEngineStage → BundleSigverifyStage → BundleStage`.
///
/// Production loop (stub parks instead):
/// ```text
/// loop {
///     let proto = stream.message().await?.ok_or(Disconnected)?;
///     let (bundles, plain_packets) = demux(proto);
///
///     for bundle in bundles {
///         bundle_sender.send(bundle)?;          // → BundleSigverifyStage
///     }
///     for batch in plain_packets {
///         if config.trust_packets {
///             // Packets from the block engine are already auth'd — skip sigverify.
///             // API: PacketIngress::trusted_banking → TrustedBankingIngress::send
///             packet_ingress.trusted_banking().unwrap().send(batch);
///         } else {
///             // API: PacketIngress::send_to_sigverify
///             packet_ingress.send_to_sigverify(batch);
///         }
///     }
/// }
/// ```
pub struct BlockEngineStage {
    abort_signal: Arc<AtomicBool>,
    thread: thread::Thread,
    handle: Option<JoinHandle<()>>,
}

impl BlockEngineStage {
    pub fn spawn(
        _config: BlockEngineConfig,
        bundle_sender: SyncSender<PacketBundle>,
        _packet_ingress: PacketIngress,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let abort_signal = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&abort_signal);
        let handle = thread::Builder::new()
            .name("jitoBlockEngineStage".to_string())
            .spawn(move || {
                while !signal.load(Ordering::Acquire) && !exit.load(Ordering::Acquire) {
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
