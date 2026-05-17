use {
    agave_banking_stage_ingress_types::BankingPacketBatch,
    crossbeam_channel::{SendError, Sender},
    solana_perf::packet::PacketBatch,
    std::sync::Arc,
};

/// Cloneable packet ingress handle supplied to TPU stage factories at startup.
///
/// Factories that send packets call [`send_to_sigverify`](Self::send_to_sigverify)
/// (unverified path) or use [`trusted_banking`](Self::trusted_banking) (pre-verified
/// bypass). The raw packet receiver for fork-owned intake routing is one-shot and
/// lives on [`TpuStageContext::take_packet_receiver`](crate::TpuStageContext::take_packet_receiver).
#[derive(Clone)]
pub struct PacketIngress {
    sigverify_sender: Sender<PacketBatch>,
    trusted_banking: Option<TrustedBankingIngress>,
}

impl PacketIngress {
    pub fn new(sigverify_sender: Sender<PacketBatch>) -> Self {
        Self {
            sigverify_sender,
            trusted_banking: None,
        }
    }

    pub fn with_trusted_banking(mut self, trusted_banking: TrustedBankingIngress) -> Self {
        self.trusted_banking = Some(trusted_banking);
        self
    }

    /// Send unverified packets into Agave's normal sigverify path.
    pub fn send_to_sigverify(&self, packets: PacketBatch) -> Result<(), SendError<PacketBatch>> {
        self.sigverify_sender.send(packets)
    }

    /// Optional trusted ingress for fork-owned paths that already verified packets.
    pub fn trusted_banking(&self) -> Option<&TrustedBankingIngress> {
        self.trusted_banking.as_ref()
    }
}

/// Sink used by [`TrustedBankingIngress`].
///
/// Implemented by Agave's channel wrapper so extensions do not depend on
/// `solana-core` internals such as banking trace.
pub trait TrustedBankingPacketSink: Send + Sync + 'static {
    fn send(&self, packets: BankingPacketBatch) -> Result<(), BankingPacketBatch>;
}

/// Cloneable ingress for packets that should bypass sigverify.
///
/// This remains off the Agave packet hot path: extensions call it from their own
/// stages when they can prove packets are already verified or trusted.
#[derive(Clone)]
pub struct TrustedBankingIngress {
    sink: Arc<dyn TrustedBankingPacketSink>,
}

impl TrustedBankingIngress {
    pub fn new(sink: Arc<dyn TrustedBankingPacketSink>) -> Self {
        Self { sink }
    }

    pub fn send(&self, packets: BankingPacketBatch) -> Result<(), BankingPacketBatch> {
        self.sink.send(packets)
    }
}
