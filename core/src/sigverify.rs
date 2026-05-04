//! The `sigverify` module provides digital signature verification functions.
//! By default, signatures are verified in parallel using all available CPU
//! cores.

use {
    crate::{banking_trace::BankingPacketSender, sigverify_stage::SigVerifyServiceError},
    agave_banking_stage_ingress_types::BankingPacketBatch,
    crossbeam_channel::{Receiver, Sender, TrySendError, bounded},
    solana_measure::measure_us,
    solana_perf::{
        packet::PacketBatch,
        sigverify::{self},
    },
    solana_runtime::bank_forks::SharableBanks,
    solana_transaction::Transaction,
    std::{
        num::NonZeroUsize,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread::JoinHandle,
        time::Duration,
    },
};

pub(crate) struct TransactionSigVerifier {
    worker_sender: Sender<TransactionVerifyTask>,
}

struct TransactionVerifyTask {
    batch: PacketBatch,
}

pub(crate) struct GossipVerifyTask {
    batch: PacketBatch,
    transaction: Transaction,
}

pub(crate) struct GossipVerifiedVoteBatch {
    pub(crate) transaction: Transaction,
    pub(crate) packet_batch: PacketBatch,
}

#[derive(Clone)]
pub(crate) struct SigVerifyWorkerStats {
    pub(crate) total_valid_packets: Arc<AtomicUsize>,
    pub(crate) total_verify_time_us: Arc<AtomicUsize>,
}

impl TransactionSigVerifier {
    fn new(worker_sender: Sender<TransactionVerifyTask>) -> Self {
        Self { worker_sender }
    }

    pub(crate) fn send_packets_to_worker_pool(
        &mut self,
        batches: Vec<PacketBatch>,
    ) -> Result<usize, SigVerifyServiceError> {
        let mut dropped_packets = 0;
        for batch in batches {
            match self.worker_sender.try_send(TransactionVerifyTask { batch }) {
                Ok(()) => {}
                Err(TrySendError::Full(task)) => {
                    dropped_packets += task.batch.len();
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(SigVerifyServiceError::WorkerQueueClosed);
                }
            }
        }

        Ok(dropped_packets)
    }
}

pub(crate) struct GossipSigVerifier {
    worker_sender: Sender<GossipVerifyTask>,
}

impl GossipSigVerifier {
    #[cfg(test)]
    pub(crate) fn new_for_tests(worker_sender: Sender<GossipVerifyTask>) -> Self {
        Self { worker_sender }
    }

    pub(crate) fn send_votes_to_worker_pool(
        &self,
        votes: Vec<Transaction>,
        packet_batches: Vec<PacketBatch>,
    ) -> Result<usize, SigVerifyServiceError> {
        assert_eq!(votes.len(), packet_batches.len());

        let num_votes = votes.len();
        let mut num_sent = 0;
        for (transaction, batch) in votes.into_iter().zip(packet_batches) {
            match self
                .worker_sender
                .try_send(GossipVerifyTask { batch, transaction })
            {
                Ok(()) => {
                    num_sent += 1;
                }
                Err(TrySendError::Full(_)) => {
                    warn!(
                        "gossip sigverify worker queue is full, dropping {} votes.",
                        num_votes.saturating_sub(num_sent)
                    );
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err(SigVerifyServiceError::WorkerQueueClosed);
                }
            }
        }

        Ok(num_sent)
    }
}

// Work queues are kept separate so that a spam on TPU
// will not lead to us dropping votes.
const SIGVERIFY_NON_VOTE_WORK_CHANNEL_SIZE: usize = 50_000;
const SIGVERIFY_TPU_VOTE_WORK_CHANNEL_SIZE: usize = 5_000; // channel is batches not individual packets
const SIGVERIFY_GOSSIP_VOTE_WORK_CHANNEL_SIZE: usize = 50_000;

#[derive(Clone)]
struct WorkerPoolChannels {
    non_vote_receiver: Receiver<TransactionVerifyTask>,
    tpu_vote_receiver: Receiver<TransactionVerifyTask>,
    gossip_receiver: Receiver<GossipVerifyTask>,
    non_vote_banking_sender: BankingPacketSender,
    tpu_vote_banking_sender: BankingPacketSender,
    gossip_verified_vote_sender: Sender<GossipVerifiedVoteBatch>,
    forward_stage_sender: Sender<(BankingPacketBatch, bool)>,
    sharable_banks: SharableBanks,
    non_vote_stats: SigVerifyWorkerStats,
    tpu_vote_stats: SigVerifyWorkerStats,
}

pub(crate) struct SigVerifyWorkerPool {
    exit: Arc<AtomicBool>,
    non_vote_sender: Sender<TransactionVerifyTask>,
    tpu_vote_sender: Sender<TransactionVerifyTask>,
    gossip_sender: Sender<GossipVerifyTask>,
    worker_hdls: Vec<JoinHandle<()>>,
}

impl Drop for SigVerifyWorkerPool {
    fn drop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
        self.worker_hdls.drain(..).for_each(|hdl| {
            if let Err(err) = hdl.join() {
                error!("sigverify worker encountered unexpected error: {err:?}");
            }
        });
    }
}

impl SigVerifyWorkerPool {
    pub(crate) fn new(
        num_workers: NonZeroUsize,
        non_vote_banking_sender: BankingPacketSender,
        tpu_vote_banking_sender: BankingPacketSender,
        gossip_verified_vote_sender: Sender<GossipVerifiedVoteBatch>,
        forward_stage_sender: Sender<(BankingPacketBatch, bool)>,
        forward_non_votes: bool,
        sharable_banks: SharableBanks,
        non_vote_stats: SigVerifyWorkerStats,
        tpu_vote_stats: SigVerifyWorkerStats,
    ) -> Self {
        let (non_vote_sender, non_vote_receiver) = bounded(SIGVERIFY_NON_VOTE_WORK_CHANNEL_SIZE);
        let (tpu_vote_sender, tpu_vote_receiver) = bounded(SIGVERIFY_TPU_VOTE_WORK_CHANNEL_SIZE);
        let (gossip_sender, gossip_receiver) = bounded(SIGVERIFY_GOSSIP_VOTE_WORK_CHANNEL_SIZE);
        let channels = WorkerPoolChannels {
            non_vote_receiver,
            tpu_vote_receiver,
            gossip_receiver,
            non_vote_banking_sender,
            tpu_vote_banking_sender,
            gossip_verified_vote_sender,
            forward_stage_sender,
            sharable_banks,
            non_vote_stats,
            tpu_vote_stats,
        };
        let exit = Arc::new(AtomicBool::new(false));
        let worker_hdls = (0..num_workers.get())
            .map(|idx| {
                let exit = exit.clone();
                let channels = channels.clone();

                std::thread::Builder::new()
                    .name(format!("solSigVerify{idx:02}"))
                    .spawn(move || Self::worker(exit, channels, forward_non_votes))
                    .expect("failed to spawn sigverify worker thread")
            })
            .collect();
        Self {
            exit,
            non_vote_sender,
            tpu_vote_sender,
            gossip_sender,
            worker_hdls,
        }
    }

    pub(crate) fn non_vote_verifier(&self) -> TransactionSigVerifier {
        TransactionSigVerifier::new(self.non_vote_sender.clone())
    }

    pub(crate) fn tpu_vote_verifier(&self) -> TransactionSigVerifier {
        TransactionSigVerifier::new(self.tpu_vote_sender.clone())
    }

    pub(crate) fn gossip_verifier(&self) -> GossipSigVerifier {
        GossipSigVerifier {
            worker_sender: self.gossip_sender.clone(),
        }
    }

    fn worker(exit: Arc<AtomicBool>, channels: WorkerPoolChannels, forward_non_votes: bool) {
        while !exit.load(Ordering::Relaxed) {
            if !Self::worker_iteration(&channels, forward_non_votes) {
                break;
            }
        }
    }

    /// Returns false if some channel connection is disconnected.
    fn worker_iteration(channels: &WorkerPoolChannels, forward_non_votes: bool) -> bool {
        crossbeam_channel::select! {
            recv(&channels.non_vote_receiver) -> maybe_work => {
                match maybe_work {
                    Ok(work) => Self::run_transaction_task(
                        work,
                        false,
                        &channels.non_vote_banking_sender,
                        &channels.forward_stage_sender,
                        forward_non_votes,
                        false,
                        &channels.sharable_banks,
                        &channels.non_vote_stats,
                    ),
                    Err(_) => false,
                }
            }
            recv(&channels.tpu_vote_receiver) -> maybe_work => {
                match maybe_work {
                    Ok(work) => Self::run_transaction_task(
                        work,
                        true,
                        &channels.tpu_vote_banking_sender,
                        &channels.forward_stage_sender,
                        true,
                        true,
                        &channels.sharable_banks,
                        &channels.tpu_vote_stats,
                    ),
                    Err(_) => false,
                }
            }
            recv(&channels.gossip_receiver) -> maybe_work => {
                match maybe_work {
                    Ok(work) => Self::run_gossip_task(
                        work,
                        &channels.gossip_verified_vote_sender,
                    ),
                    Err(_) => false,
                }
            }
            default(Duration::from_millis(10)) => { true }
        }
    }

    fn run_transaction_task(
        mut work: TransactionVerifyTask,
        reject_non_vote: bool,
        banking_stage_sender: &BankingPacketSender,
        forward_stage_sender: &Sender<(BankingPacketBatch, bool)>,
        should_forward: bool,
        is_tpu_vote: bool,
        sharable_banks: &SharableBanks,
        stats: &SigVerifyWorkerStats,
    ) -> bool {
        let enable_tx_v1 = sharable_banks.working().feature_set.snapshot().enable_tx_v1;
        let (_, verify_time_us) = measure_us!(sigverify::ed25519_verify_serial(
            &mut work.batch,
            reject_non_vote,
            enable_tx_v1,
        ));
        let num_valid_packets = sigverify::count_valid_packets(std::iter::once(&work.batch));
        stats
            .total_valid_packets
            .fetch_add(num_valid_packets, Ordering::Relaxed);
        stats
            .total_verify_time_us
            .fetch_add(verify_time_us as usize, Ordering::Relaxed);

        let banking_packet_batch = BankingPacketBatch::new(vec![work.batch]);
        if let Err(err) = banking_stage_sender.send(banking_packet_batch.clone()) {
            error!("sigverify send failed: {err:?}");
            return false;
        }
        if should_forward {
            Self::try_forward(forward_stage_sender, banking_packet_batch, is_tpu_vote);
        }

        true
    }

    fn run_gossip_task(
        mut work: GossipVerifyTask,
        verified_vote_sender: &Sender<GossipVerifiedVoteBatch>,
    ) -> bool {
        // Gossip votes are legacy Transaction values, not tx-v1 packets.
        sigverify::ed25519_verify_serial(&mut work.batch, true, false);

        if let Err(err) = verified_vote_sender.send(GossipVerifiedVoteBatch {
            transaction: work.transaction,
            packet_batch: work.batch,
        }) {
            debug!("gossip sigverify response send failed: {err:?}");
        }

        true
    }

    fn try_forward(
        forward_stage_sender: &Sender<(BankingPacketBatch, bool)>,
        banking_packet_batch: BankingPacketBatch,
        is_tpu_vote: bool,
    ) {
        if let Err(TrySendError::Full(_)) =
            forward_stage_sender.try_send((banking_packet_batch, is_tpu_vote))
        {
            warn!("forwarding stage channel is full, dropping packets.");
        }
    }
}
