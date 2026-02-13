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
    solana_measure::measure::Measure,
    solana_perf::{
        packet::PacketBatch,
        sigverify::{self, sigverify_thread_pool},
    },
    std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
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

    fn verify_and_send_packets(
        &mut self,
        batches: Vec<PacketBatch>,
        valid_packets: usize,
        total_valid_packets: Arc<AtomicUsize>,
        total_verify_time_us: Arc<AtomicUsize>,
    ) -> Result<(), SigVerifyServiceError<Self::SendType>> {
        let banking_stage_sender = self.banking_stage_sender.clone();
        let forward_stage_sender = self.forward_stage_sender.clone();
        let reject_non_vote = self.reject_non_vote;

        sigverify_thread_pool().spawn(move || {
            let mut verify_time = Measure::start("sigverify_batch_time");
            let mut batches = batches;
            sigverify::ed25519_verify(&mut batches, reject_non_vote, valid_packets);
            verify_time.stop();
            let num_valid_packets = sigverify::count_valid_packets(&batches);

            let banking_packet_batch = BankingPacketBatch::new(batches);
            if let Some(forward_stage_sender) = &forward_stage_sender {
                if let Err(err) = banking_stage_sender.send(banking_packet_batch.clone()) {
                    error!("sigverify send failed: {err:?}");
                    return;
                }
                if let Err(TrySendError::Full(_)) =
                    forward_stage_sender.try_send((banking_packet_batch, reject_non_vote))
                {
                    warn!("forwarding stage channel is full, dropping packets.");
                }
            } else if let Err(err) = banking_stage_sender.send(banking_packet_batch) {
                error!("sigverify send failed: {err:?}");
                return;
            }

            total_valid_packets.fetch_add(num_valid_packets, Ordering::Relaxed);
            total_verify_time_us.fetch_add(verify_time.as_us() as usize, Ordering::Relaxed);
        });

        Ok(())
    }
}
