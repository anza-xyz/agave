//! The `sigverify` module provides digital signature verification functions.
//! By default, signatures are verified in parallel using all available CPU
//! cores.
//!

pub use solana_perf::sigverify::{
    count_packets_in_batches, ed25519_verify_cpu, ed25519_verify_disabled, TxOffset,
};
use {
    crate::{
        banking_trace::BankingPacketSender,
        sigverify_stage::{SigVerifier, SigVerifyServiceError},
    },
    agave_banking_stage_ingress_types::BankingPacketBatch,
    solana_perf::{packet::PacketBatch, sigverify},
};

pub struct TransactionSigVerifier {
    packet_sender: BankingPacketSender,
    reject_non_vote: bool,
}

impl TransactionSigVerifier {
    pub fn new_reject_non_vote(packet_sender: BankingPacketSender) -> Self {
        let mut new_self = Self::new(packet_sender);
        new_self.reject_non_vote = true;
        new_self
    }

    pub fn new(packet_sender: BankingPacketSender) -> Self {
        Self {
            packet_sender,
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
        self.packet_sender
            .send(BankingPacketBatch::new(packet_batches))?;
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
