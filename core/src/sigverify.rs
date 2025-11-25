//! The `sigverify` module provides digital signature verification functions.
//! By default, signatures are verified in parallel using all available CPU
//! cores.

pub use solana_perf::sigverify::{
    count_packets_in_batches, ed25519_verify, ed25519_verify_disabled, TxOffset,
};
use {
    crate::{
        banking_trace::BankingPacketSender,
        sigverify_stage::{SigVerifier, SigVerifyServiceError},
    },
    agave_banking_stage_ingress_types::BankingPacketBatch,
    crossbeam_channel::{Sender, TrySendError},
    solana_perf::{packet::PacketBatch, sigverify},
};

pub struct TransactionSigVerifier {
    banking_stage_sender: BankingPacketSender,
    forward_stage_sender: Option<Sender<(BankingPacketBatch, bool)>>,
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
        Self {
            banking_stage_sender,
            forward_stage_sender,
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
        sigverify::ed25519_verify(&mut batches, self.reject_non_vote, valid_packets);
        batches
    }
}
