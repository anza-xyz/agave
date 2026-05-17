use {
    agave_tpu_extension_api::{
        LifecycleStage, PacketBatch, PacketIngress, TpuStage, TpuStageContext, TpuStageFactory,
    },
    crossbeam_channel::{Receiver, RecvTimeoutError},
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::Duration,
    },
};

/// Factory for Jito's fetch-stage manager.
pub struct FetchStageManagerFactory;

impl TpuStageFactory for FetchStageManagerFactory {
    fn spawn(self: Box<Self>, context: &dyn TpuStageContext) -> Box<dyn TpuStage> {
        Box::new(FetchStageManager::spawn(
            context.packet_ingress().clone(),
            context.take_packet_receiver(),
            context.exit(),
        ))
    }
}

/// Routes raw Agave packet intake through fork-owned policy before sigverify.
///
/// Agave hands over its raw QUIC/UDP socket output via
/// [`TpuStageContext::take_packet_receiver`]. The fork applies policy (e.g.
/// drop low-priority transactions while a bundle is executing) then forwards
/// accepted packets onward.
///
/// Production loop (stub forwards everything instead):
/// ```text
/// while let Ok(batch) = packet_receiver.recv_timeout(timeout) {
///     // Drop or throttle based on fork policy (e.g. yield_flag is set).
///     let accepted = policy.filter(batch);
///     if !accepted.is_empty() {
///         // API: PacketIngress::send_to_sigverify
///         packet_ingress.send_to_sigverify(accepted);
///     }
/// }
/// ```
pub struct FetchStageManager {
    abort_signal: Arc<AtomicBool>,
    thread: thread::Thread,
    handle: Option<JoinHandle<()>>,
}

impl FetchStageManager {
    fn spawn(
        packet_ingress: PacketIngress,
        packet_receiver: Option<Receiver<PacketBatch>>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let abort_signal = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&abort_signal);
        let handle = thread::Builder::new()
            .name("jitoFetchManager".to_string())
            .spawn(move || {
                while !signal.load(Ordering::Acquire) && !exit.load(Ordering::Acquire) {
                    let Some(packet_receiver) = &packet_receiver else {
                        thread::park_timeout(Duration::from_millis(50));
                        continue;
                    };
                    match packet_receiver.recv_timeout(Duration::from_millis(50)) {
                        Ok(packets) => {
                            let _ = packet_ingress.send_to_sigverify(packets);
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("jitoFetchManager spawn failed");
        let thread = handle.thread().clone();
        Self {
            abort_signal,
            thread,
            handle: Some(handle),
        }
    }
}

impl LifecycleStage for FetchStageManager {
    fn abort(&self) {
        self.abort_signal.store(true, Ordering::Release);
        self.thread.unpark();
    }

    fn join(mut self: Box<Self>) -> thread::Result<()> {
        self.abort();
        self.handle.take().map(|h| h.join()).unwrap_or(Ok(()))
    }
}

impl TpuStage for FetchStageManager {}
