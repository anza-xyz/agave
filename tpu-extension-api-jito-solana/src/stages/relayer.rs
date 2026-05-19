use {
    agave_tpu_extension_api::{
        LifecycleStage, PacketIngress, TpuStage, TpuStageContext, TpuStageFactory,
    },
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::Duration,
    },
};

/// Factory for Jito's relayer stage.
pub struct RelayerStageFactory;

impl TpuStageFactory for RelayerStageFactory {
    fn spawn(self: Box<Self>, context: &dyn TpuStageContext) -> Box<dyn TpuStage> {
        Box::new(RelayerStage::spawn(
            context.packet_ingress().clone(),
            context.exit(),
        ))
    }
}

/// Receives packets from a trusted relayer gRPC stream and forwards to Agave sigverify.
///
/// Production loop (stub parks instead):
/// ```text
/// loop {
///     let batch = match stream.next().await {
///         Some(Ok(b)) => b,
///         Some(Err(_)) | None => { reconnect(); continue; }
///     };
///     // API: PacketIngress::send_to_sigverify
///     if packet_ingress.send_to_sigverify(batch).is_err() { break; }
/// }
/// ```
pub struct RelayerStage {
    abort_signal: Arc<AtomicBool>,
    thread: thread::Thread,
    handle: Option<JoinHandle<()>>,
}

impl RelayerStage {
    fn spawn(packet_ingress: PacketIngress, exit: Arc<AtomicBool>) -> Self {
        let abort_signal = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&abort_signal);
        let handle = thread::Builder::new()
            .name("jitoRelayerStage".to_string())
            .spawn(move || {
                // Production: open gRPC stream to relayer, receive packets,
                // apply policy, forward via packet_ingress.send_to_sigverify().
                let _ingress = packet_ingress;
                while !signal.load(Ordering::Acquire) && !exit.load(Ordering::Acquire) {
                    thread::park_timeout(Duration::from_millis(50));
                }
            })
            .expect("jitoRelayerStage spawn failed");
        let thread = handle.thread().clone();
        Self {
            abort_signal,
            thread,
            handle: Some(handle),
        }
    }
}

impl LifecycleStage for RelayerStage {
    fn abort(&self) {
        self.abort_signal.store(true, Ordering::Release);
        self.thread.unpark();
    }

    fn join(mut self: Box<Self>) -> thread::Result<()> {
        self.abort();
        self.handle.take().map(|h| h.join()).unwrap_or(Ok(()))
    }
}

impl TpuStage for RelayerStage {}
