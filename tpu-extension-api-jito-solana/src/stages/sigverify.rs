use {
    super::PacketBundle,
    agave_tpu_extension_api::{LifecycleStage, TpuStage},
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc::{Receiver, SyncSender},
        },
        thread::{self, JoinHandle},
    },
};

/// Intermediate stage: verifies bundle transaction signatures before forwarding
/// to the executor.
///
/// Sits between `BlockEngineStage` (intake) and `BundleStage` (execution).
/// In production it verifies each transaction's Ed25519 signatures and sanitizes
/// the transactions; bundles that fail verification are dropped. The reference
/// stub forwards bundles through without any verification to keep the stand-in
/// minimal.
///
/// A production implementation would change the channel type here: the input
/// would carry raw serialized transactions and the output would carry
/// `SanitizedTransaction` objects. The reference collapses both to
/// [`PacketBundle`](super::PacketBundle) for simplicity.
pub struct BundleSigverifyStage {
    abort_signal: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl BundleSigverifyStage {
    pub fn spawn(
        receiver: Receiver<PacketBundle>,
        verified_sender: SyncSender<PacketBundle>,
    ) -> Self {
        let abort_signal = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&abort_signal);
        let handle = thread::Builder::new()
            .name("jitoBundleSigverify".to_string())
            .spawn(move || {
                while let Ok(bundle) = receiver.recv() {
                    if signal.load(Ordering::Acquire) {
                        break;
                    }
                    if verified_sender.send(bundle).is_err() {
                        break;
                    }
                }
            })
            .expect("jitoBundleSigverify spawn failed");
        Self {
            abort_signal,
            handle: Some(handle),
        }
    }
}

impl LifecycleStage for BundleSigverifyStage {
    fn abort(&self) {
        self.abort_signal.store(true, Ordering::Release);
    }
    fn join(mut self: Box<Self>) -> thread::Result<()> {
        self.abort();
        self.handle.take().map(|h| h.join()).unwrap_or(Ok(()))
    }
}

impl TpuStage for BundleSigverifyStage {}
