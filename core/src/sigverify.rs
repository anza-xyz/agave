//! The `sigverify` module provides digital signature verification functions.
//! By default, signatures are verified in parallel using all available CPU
//! cores.  When perf-libs are available signature verification is offloaded
//! to the GPU.
//!

pub use solana_perf::sigverify::{
    count_packets_in_batches, ed25519_verify_cpu, ed25519_verify_disabled, init, TxOffset,
};
use {
    crate::{
        banking_trace::BankingPacketSender,
        sigverify_stage::{SigVerifier, SigVerifyServiceError},
    },
    agave_banking_stage_ingress_types::BankingPacketBatch,
    crossbeam_channel::{Sender, TrySendError},
    solana_perf::{cuda_runtime::PinnedVec, packet::PacketBatch, recycler::Recycler, sigverify},
};

pub struct TransactionSigVerifier {
    banking_stage_sender: BankingPacketSender,
    forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
    recycler: Recycler<TxOffset>,
    recycler_out: Recycler<PinnedVec<u8>>,
    reject_non_vote: bool,
}

impl TransactionSigVerifier {
    pub fn new_reject_non_vote(
        packet_sender: BankingPacketSender,
        forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
    ) -> Self {
        let mut new_self = Self::new(packet_sender, forward_stage_sender);
        new_self.reject_non_vote = true;
        new_self
    }

    pub fn new(
        banking_stage_sender: BankingPacketSender,
        forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
    ) -> Self {
        init();
        Self {
            banking_stage_sender,
            forward_stage_sender,
            recycler: Recycler::warmed(50, 4096),
            recycler_out: Recycler::warmed(50, 4096),
            reject_non_vote: false,
        }
    }
}

impl SigVerifier for TransactionSigVerifier {
    type SendType = BankingPacketBatch;

    fn send_packets(
        &mut self,
        packet_batches: Vec<PacketBatch>,
    ) -> Result<(), SigVerifyServiceError<Self::SendType>> {
        let banking_packet_batch = BankingPacketBatch::new(packet_batches);
        if let Some(forward_stage_sender) = &self.forward_stage_sender {
            self.banking_stage_sender
                .send(banking_packet_batch.clone())?;
            if let Err(TrySendError::Full(_)) =
                forward_stage_sender.try_send((banking_packet_batch, self.reject_non_vote))
            {
                warn!("forwarding stage channel is full, dropping packets.");
            }
        } else {
            self.banking_stage_sender.send(banking_packet_batch)?;
        }

        Ok(())
    }

    fn verify_batches(
        &self,
        mut batches: Vec<PacketBatch>,
        valid_packets: usize,
    ) -> Vec<PacketBatch> {
        sigverify::ed25519_verify(
            &mut batches,
            &self.recycler,
            &self.recycler_out,
            self.reject_non_vote,
            valid_packets,
        );
        batches
    }
}
