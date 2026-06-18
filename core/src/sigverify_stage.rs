//! The `sigverify_stage` implements the signature verification stage of the TPU. It
//! receives a list of lists of packets and outputs the same list, but tags each
//! top-level list with a list of booleans, telling the next stage whether the
//! signature in that packet is valid. It assumes each packet contains one
//! transaction. All processing is done on the CPU by default.

use {
    crate::{
        banking_trace::BankingPacketSender,
        sigverify::{
            GossipSigVerifier, GossipVerifiedVoteBatch, SigVerifyWorkerPool,
            SigVerifyWorkerSenders, SigVerifyWorkerState, SigVerifyWorkerStats,
        },
    },
    agave_banking_stage_ingress_types::{BankingPacketBatch, SchedulerPriorityFloor},
    core::time::Duration,
    crossbeam_channel::{Receiver, Sender, unbounded},
    solana_perf::{deduper::Deduper, packet::PacketBatch},
    solana_runtime::bank_forks::SharableBanks,
    solana_transaction::Transaction,
    std::{
        num::NonZeroUsize,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::Instant,
    },
    thiserror::Error,
};

#[derive(Error, Debug)]
pub enum SigVerifyServiceError {
    #[error("sigverify worker queue closed")]
    WorkerQueueClosed,
}

const DEDUPER_NUM_BITS: u64 = 63_999_979;
const MAX_DEDUPER_AGE: Duration = Duration::from_secs(2);
const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;

pub struct SigVerifyStage {
    exit: Arc<AtomicBool>,
    servicer_thread_hdl: Option<JoinHandle<()>>,
    // pool held so workers stay alive. If dropped, workers in pool shut down.
    _worker_pool: SigVerifyWorkerPool,
}

pub struct GossipSigVerifyHandle {
    verifier: GossipSigVerifier,
    verified_vote_receiver: Receiver<GossipVerifiedVoteBatch>,
}

#[derive(Default)]
struct SigVerifierStats {
    num_deduper_saturations: usize,
    total_batches: Arc<AtomicUsize>,
    total_packets: Arc<AtomicUsize>,
    total_dedup: Arc<AtomicUsize>,
    total_valid_packets: Arc<AtomicUsize>,
    total_dedup_time_us: Arc<AtomicUsize>,
    total_verify_time_us: Arc<AtomicUsize>,
    /// Max occupancy of the banking_stage output channel since last report.
    max_pre_send_len: Arc<AtomicUsize>,
    /// Count of sends in which the EvictingSender had to drop a batch.
    eviction_drops: Arc<AtomicUsize>,
    total_dropped_below_priority_floor: Arc<AtomicUsize>,
    total_priority_floor_time_us: Arc<AtomicUsize>,
}

struct ServicerState {
    deduper: Arc<Deduper<2, [u8]>>,
    metrics_name: &'static str,
    stats: SigVerifierStats,
}

impl SigVerifierStats {
    const REPORT_INTERVAL: Duration = Duration::from_secs(2);

    fn maybe_report_and_reset(&mut self, name: &'static str) {
        // No need to report a datapoint if no batches/packets received
        if self.total_batches.load(Ordering::Relaxed) == 0 {
            return;
        }

        datapoint_info!(
            name,
            (
                "num_deduper_saturations",
                core::mem::take(&mut self.num_deduper_saturations),
                i64
            ),
            (
                "total_batches",
                self.total_batches.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "total_packets",
                self.total_packets.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "total_dedup",
                self.total_dedup.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "total_dedup_time_us",
                self.total_dedup_time_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "total_dropped_below_priority_floor",
                self.total_dropped_below_priority_floor
                    .swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "total_priority_floor_time_us",
                self.total_priority_floor_time_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "total_valid_packets",
                self.total_valid_packets.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "total_verify_time_us",
                self.total_verify_time_us.swap(0, Ordering::Relaxed),
                i64
            ),
            (
                "max_pre_send_len",
                self.max_pre_send_len.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "eviction_drops",
                self.eviction_drops.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
        );
    }
}

impl SigVerifyStage {
    pub fn new(
        packet_receiver: Receiver<PacketBatch>,
        vote_packet_receiver: Receiver<PacketBatch>,
        non_vote_sender: BankingPacketSender,
        tpu_vote_sender: BankingPacketSender,
        forward_stage_sender: Sender<(BankingPacketBatch, bool)>,
        num_workers: NonZeroUsize,
        forward_non_votes: bool,
        sharable_banks: SharableBanks,
        scheduler_priority_floor: Option<Arc<SchedulerPriorityFloor>>,
    ) -> (Self, GossipSigVerifyHandle) {
        let (gossip_verified_vote_sender, verified_vote_receiver) = unbounded();
        let non_vote_stats = SigVerifierStats::default();
        let tpu_vote_stats = SigVerifierStats::default();
        let exit = Arc::new(AtomicBool::new(false));
        let mut rng = rand::rng();
        let non_vote_deduper = Arc::new(Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS));
        let tpu_vote_deduper = Arc::new(Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS));
        let worker_pool = SigVerifyWorkerPool::new(
            num_workers,
            packet_receiver,
            vote_packet_receiver,
            SigVerifyWorkerSenders {
                gossip_verified_vote_sender,
                forward_stage_sender,
            },
            forward_non_votes,
            sharable_banks,
            SigVerifyWorkerState::new(
                non_vote_sender,
                non_vote_deduper.clone(),
                SigVerifyWorkerStats {
                    total_batches: non_vote_stats.total_batches.clone(),
                    total_packets: non_vote_stats.total_packets.clone(),
                    total_dedup: non_vote_stats.total_dedup.clone(),
                    total_dedup_time_us: non_vote_stats.total_dedup_time_us.clone(),
                    total_valid_packets: non_vote_stats.total_valid_packets.clone(),
                    total_verify_time_us: non_vote_stats.total_verify_time_us.clone(),
                    max_pre_send_len: non_vote_stats.max_pre_send_len.clone(),
                    eviction_drops: non_vote_stats.eviction_drops.clone(),
                    total_dropped_below_priority_floor: non_vote_stats
                        .total_dropped_below_priority_floor
                        .clone(),
                    total_priority_floor_time_us: non_vote_stats
                        .total_priority_floor_time_us
                        .clone(),
                },
                scheduler_priority_floor,
            ),
            SigVerifyWorkerState::new(
                tpu_vote_sender,
                tpu_vote_deduper.clone(),
                SigVerifyWorkerStats {
                    total_batches: tpu_vote_stats.total_batches.clone(),
                    total_packets: tpu_vote_stats.total_packets.clone(),
                    total_dedup: tpu_vote_stats.total_dedup.clone(),
                    total_dedup_time_us: tpu_vote_stats.total_dedup_time_us.clone(),
                    total_valid_packets: tpu_vote_stats.total_valid_packets.clone(),
                    total_verify_time_us: tpu_vote_stats.total_verify_time_us.clone(),
                    max_pre_send_len: tpu_vote_stats.max_pre_send_len.clone(),
                    eviction_drops: tpu_vote_stats.eviction_drops.clone(),
                    total_dropped_below_priority_floor: tpu_vote_stats
                        .total_dropped_below_priority_floor
                        .clone(),
                    total_priority_floor_time_us: tpu_vote_stats
                        .total_priority_floor_time_us
                        .clone(),
                },
                None, // votes are not dropped for priority-floor
            ),
        );
        let servicer_thread_hdl = Self::servicer(
            exit.clone(),
            ServicerState {
                deduper: non_vote_deduper,
                metrics_name: "tpu-verifier",
                stats: non_vote_stats,
            },
            ServicerState {
                deduper: tpu_vote_deduper,
                metrics_name: "tpu-vote-verifier",
                stats: tpu_vote_stats,
            },
        );
        let gossip_sigverify_handle = GossipSigVerifyHandle {
            verifier: worker_pool.gossip_verifier(),
            verified_vote_receiver,
        };

        (
            Self {
                exit,
                servicer_thread_hdl: Some(servicer_thread_hdl),
                _worker_pool: worker_pool,
            },
            gossip_sigverify_handle,
        )
    }

    pub fn join(mut self) -> thread::Result<()> {
        self.join_servicer_thread()
    }

    fn join_servicer_thread(&mut self) -> thread::Result<()> {
        self.exit.store(true, Ordering::Relaxed);
        self.servicer_thread_hdl
            .take()
            .map(JoinHandle::join)
            .unwrap_or(Ok(()))
    }

    /// Drives deduper reset and metrics reporting for sigverify packet streams.
    fn servicer(
        exit: Arc<AtomicBool>,
        mut non_vote_state: ServicerState,
        mut tpu_vote_state: ServicerState,
    ) -> JoinHandle<()> {
        let mut last_print = Instant::now();
        Builder::new()
            .name("solSigVerSvc".to_string())
            .spawn(move || {
                let mut rng = rand::rng();
                while !exit.load(Ordering::Relaxed) {
                    for state in [&mut non_vote_state, &mut tpu_vote_state] {
                        if state.deduper.maybe_reset(
                            &mut rng,
                            DEDUPER_FALSE_POSITIVE_RATE,
                            MAX_DEDUPER_AGE,
                        ) {
                            state.stats.num_deduper_saturations += 1;
                        }
                    }
                    if last_print.elapsed() > SigVerifierStats::REPORT_INTERVAL {
                        non_vote_state
                            .stats
                            .maybe_report_and_reset(non_vote_state.metrics_name);
                        tpu_vote_state
                            .stats
                            .maybe_report_and_reset(tpu_vote_state.metrics_name);
                        last_print = Instant::now();
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            })
            .unwrap()
    }
}

impl Drop for SigVerifyStage {
    fn drop(&mut self) {
        if let Err(err) = self.join_servicer_thread() {
            error!("sigverify servicer encountered unexpected error: {err:?}");
        }
    }
}

impl GossipSigVerifyHandle {
    #[cfg(test)]
    pub(crate) fn new_for_tests(
        worker_sender: Sender<crate::sigverify::GossipVerifyTask>,
        verified_vote_receiver: Receiver<GossipVerifiedVoteBatch>,
    ) -> Self {
        Self {
            verifier: GossipSigVerifier::new_for_tests(worker_sender),
            verified_vote_receiver,
        }
    }

    /// Submit gossip votes for signature verification and collect the corresponding responses.
    ///
    /// This takes `&mut self` because responses for all submitted gossip tasks share one receiver.
    /// Allowing concurrent callers would make it possible for one caller to consume another caller's
    /// verification results.
    pub(crate) fn verify_and_receive_votes(
        &mut self,
        votes: Vec<Transaction>,
        packet_batches: Vec<PacketBatch>,
    ) -> std::result::Result<(Vec<Transaction>, Vec<PacketBatch>), crossbeam_channel::RecvError>
    {
        let num_batches = match self
            .verifier
            .send_votes_to_worker_pool(votes, packet_batches)
        {
            Ok(num_batches) => num_batches,
            Err(err) => {
                error!("gossip sigverify enqueue failed: {err:?}");
                return Ok((Vec::new(), Vec::new()));
            }
        };
        let mut verified_vote_txs = Vec::with_capacity(num_batches);
        let mut verified_packet_batches = Vec::with_capacity(num_batches);

        for _ in 0..num_batches {
            let verified_vote_batch = self.verified_vote_receiver.recv()?;
            verified_vote_txs.push(verified_vote_batch.transaction);
            verified_packet_batches.push(verified_vote_batch.packet_batch);
        }

        Ok((verified_vote_txs, verified_packet_batches))
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::banking_trace::BankingTracer,
        crossbeam_channel::bounded,
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_message::{VersionedMessage, v1},
        solana_perf::{
            packet::{BytesPacket, BytesPacketBatch, to_packet_batches},
            test_tx::test_tx,
        },
        solana_pubkey::Pubkey,
        solana_runtime::{bank::Bank, genesis_utils::create_genesis_config},
        solana_signer::Signer,
        solana_system_interface::instruction as system_instruction,
        solana_transaction::versioned::VersionedTransaction,
        test_case::test_case,
    };

    fn test_tx_v1() -> VersionedTransaction {
        let payer = Keypair::new();
        let recipient = Pubkey::new_unique();
        let instruction = system_instruction::transfer(&payer.pubkey(), &recipient, 1);
        let message =
            v1::Message::try_compile(&payer.pubkey(), &[instruction], Hash::new_unique()).unwrap();

        VersionedTransaction::try_new(VersionedMessage::V1(message), &[&payer]).unwrap()
    }

    fn gen_batches(
        use_same_tx: bool,
        packets_per_batch: usize,
        total_packets: usize,
    ) -> Vec<PacketBatch> {
        if use_same_tx {
            let tx = test_tx();
            to_packet_batches(&vec![tx; total_packets], packets_per_batch)
        } else {
            let txs: Vec<_> = (0..total_packets).map(|_| test_tx()).collect();
            to_packet_batches(&txs, packets_per_batch)
        }
    }

    #[test]
    fn test_sigverify_stage_with_same_tx() {
        test_sigverify_stage(true)
    }

    #[test]
    fn test_sigverify_stage_without_same_tx() {
        test_sigverify_stage(false)
    }

    fn test_sigverify_stage(use_same_tx: bool) {
        agave_logger::setup();
        trace!("start");
        let (_bank, bank_forks) =
            Bank::new_with_bank_forks_for_tests(&create_genesis_config(1).genesis_config);
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let (packet_s, packet_r) = bounded(1024);
        let (vote_packet_s, vote_packet_r) = bounded(1024);
        let (verified_s, verified_r) = BankingTracer::channel_for_test();
        let (tpu_vote_s, _tpu_vote_r) = BankingTracer::channel_for_test();
        let (forward_stage_s, _forward_stage_r) = bounded(1024);
        let (stage, gossip_sigverify_handle) = SigVerifyStage::new(
            packet_r,
            vote_packet_r,
            verified_s,
            tpu_vote_s,
            forward_stage_s,
            NonZeroUsize::new(4).unwrap(),
            false,
            sharable_banks,
            None,
        );

        let now = Instant::now();
        let packets_per_batch = 128;
        let total_packets = 1920;

        let batches = gen_batches(use_same_tx, packets_per_batch, total_packets);
        trace!(
            "starting... generation took: {} ms batches: {}",
            now.elapsed().as_millis(),
            batches.len()
        );

        let mut sent_len = 0;
        for batch in batches.into_iter() {
            sent_len += batch.len();
            assert_eq!(batch.len(), packets_per_batch);
            packet_s.send(batch).unwrap();
        }
        let mut valid_received = 0;
        trace!("sent: {sent_len}");
        drop(packet_s);
        loop {
            if let Ok(verifieds) = verified_r.recv_timeout(Duration::from_secs(30)) {
                valid_received += verifieds.iter().filter(|p| !p.meta().discard()).count();
            } else {
                break;
            }

            if valid_received >= if use_same_tx { 1 } else { total_packets } {
                break;
            }
        }
        trace!("received: {valid_received}");

        if use_same_tx {
            assert_eq!(valid_received, 1);
        } else {
            assert_eq!(valid_received, total_packets);
        }
        drop(vote_packet_s);
        drop(gossip_sigverify_handle);
        stage.join().unwrap();
    }

    #[test_case(false, false; "tx_v1_disabled")]
    #[test_case(true, true; "tx_v1_enabled")]
    fn test_sigverify_stage_tx_v1_feature_gate(enable_tx_v1: bool, expected_valid: bool) {
        let genesis_config = create_genesis_config(1).genesis_config;
        let mut bank = Bank::new_for_tests(&genesis_config);
        if enable_tx_v1 {
            bank.activate_feature(&agave_feature_set::enable_tx_v1::id());
        } else {
            bank.deactivate_feature(&agave_feature_set::enable_tx_v1::id());
        }
        let (_bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
        let sharable_banks = bank_forks.read().unwrap().sharable_banks();
        let (packet_s, packet_r) = bounded(1024);
        let (vote_packet_s, vote_packet_r) = bounded(1024);
        let (verified_s, verified_r) = BankingTracer::channel_for_test();
        let (tpu_vote_s, _tpu_vote_r) = BankingTracer::channel_for_test();
        let (forward_stage_s, _forward_stage_r) = bounded(1024);
        let (stage, gossip_sigverify_handle) = SigVerifyStage::new(
            packet_r,
            vote_packet_r,
            verified_s,
            tpu_vote_s,
            forward_stage_s,
            NonZeroUsize::new(1).unwrap(),
            false,
            sharable_banks,
            None,
        );

        let mut bytes_batch = BytesPacketBatch::with_capacity(1);
        bytes_batch.push(BytesPacket::from_bytes(
            None,
            wincode::serialize(&test_tx_v1()).unwrap(),
        ));
        packet_s.send(PacketBatch::from(bytes_batch)).unwrap();

        let verified_batch = verified_r.recv_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(verified_batch.len(), 1);
        assert_eq!(
            !verified_batch.get(0).unwrap().meta().discard(),
            expected_valid
        );

        drop(packet_s);
        drop(vote_packet_s);
        drop(gossip_sigverify_handle);
        stage.join().unwrap();
    }
}
