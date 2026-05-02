//! The `sigverify_stage` implements the signature verification stage of the TPU. It
//! receives a list of lists of packets and outputs the same list, but tags each
//! top-level list with a list of booleans, telling the next stage whether the
//! signature in that packet is valid. It assumes each packet contains one
//! transaction. All processing is done on the CPU by default.

use {
    crate::{
        banking_trace::BankingPacketSender,
        sigverify::{
            GossipSigVerifier, GossipVerifiedVoteBatch, SigVerifyWorkerPool, SigVerifyWorkerStats,
            TransactionSigVerifier,
        },
    },
    agave_banking_stage_ingress_types::BankingPacketBatch,
    core::time::Duration,
    crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded},
    solana_measure::measure::Measure,
    solana_perf::{
        deduper::{self, Deduper},
        packet::PacketBatch,
    },
    solana_streamer::streamer::{self, StreamerError},
    solana_transaction::Transaction,
    std::{
        num::NonZeroUsize,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::Instant,
    },
    thiserror::Error,
};

#[derive(Error, Debug)]
pub enum SigVerifyServiceError {
    #[error("streamer error")]
    Streamer(#[from] StreamerError),
    #[error("sigverify worker queue closed")]
    WorkerQueueClosed,
}

type Result<T> = std::result::Result<T, SigVerifyServiceError>;

pub struct SigVerifyStage {
    non_vote_thread_hdl: JoinHandle<()>,
    tpu_vote_thread_hdl: JoinHandle<()>,
    // pool held so workers stay alive. If dropped, workers in pool shut down.
    _worker_pool: SigVerifyWorkerPool,
}

pub struct GossipSigVerifyHandle {
    verifier: GossipSigVerifier,
    verified_vote_receiver: Receiver<GossipVerifiedVoteBatch>,
}

#[derive(Default)]
struct SigVerifierStats {
    recv_batches_us_hist: histogram::Histogram, // time to call recv_batch
    dedup_packets_pp_us_hist: histogram::Histogram, // per-packet time to call verify_batch
    batches_hist: histogram::Histogram,         // number of packet batches per verify call
    packets_hist: histogram::Histogram,         // number of packets per verify call
    num_deduper_saturations: usize,
    total_batches: usize,
    total_packets: usize,
    total_dedup: usize,
    total_valid_packets: Arc<AtomicUsize>,
    total_dedup_time_us: usize,
    total_verify_time_us: Arc<AtomicUsize>,
    total_dropped_on_capacity: usize,
}

impl SigVerifierStats {
    const REPORT_INTERVAL: Duration = Duration::from_secs(2);

    fn maybe_report_and_reset(&mut self, name: &'static str) {
        // No need to report a datapoint if no batches/packets received
        if self.total_batches == 0 {
            return;
        }

        datapoint_info!(
            name,
            (
                "recv_batches_us_90pct",
                self.recv_batches_us_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            (
                "recv_batches_us_min",
                self.recv_batches_us_hist.minimum().unwrap_or(0),
                i64
            ),
            (
                "recv_batches_us_max",
                self.recv_batches_us_hist.maximum().unwrap_or(0),
                i64
            ),
            (
                "recv_batches_us_mean",
                self.recv_batches_us_hist.mean().unwrap_or(0),
                i64
            ),
            (
                "dedup_packets_pp_us_90pct",
                self.dedup_packets_pp_us_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            (
                "dedup_packets_pp_us_min",
                self.dedup_packets_pp_us_hist.minimum().unwrap_or(0),
                i64
            ),
            (
                "dedup_packets_pp_us_max",
                self.dedup_packets_pp_us_hist.maximum().unwrap_or(0),
                i64
            ),
            (
                "dedup_packets_pp_us_mean",
                self.dedup_packets_pp_us_hist.mean().unwrap_or(0),
                i64
            ),
            (
                "batches_90pct",
                self.batches_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            ("batches_min", self.batches_hist.minimum().unwrap_or(0), i64),
            ("batches_max", self.batches_hist.maximum().unwrap_or(0), i64),
            ("batches_mean", self.batches_hist.mean().unwrap_or(0), i64),
            (
                "packets_90pct",
                self.packets_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            ("packets_min", self.packets_hist.minimum().unwrap_or(0), i64),
            ("packets_max", self.packets_hist.maximum().unwrap_or(0), i64),
            ("packets_mean", self.packets_hist.mean().unwrap_or(0), i64),
            (
                "num_deduper_saturations",
                core::mem::take(&mut self.num_deduper_saturations),
                i64
            ),
            (
                "total_batches",
                core::mem::take(&mut self.total_batches),
                i64
            ),
            (
                "total_packets",
                core::mem::take(&mut self.total_packets),
                i64
            ),
            ("total_dedup", core::mem::take(&mut self.total_dedup), i64),
            (
                "total_dedup_time_us",
                core::mem::take(&mut self.total_dedup_time_us),
                i64
            ),
            (
                "total_dropped_on_capacity",
                core::mem::take(&mut self.total_dropped_on_capacity),
                i64
            ),
            (
                "total_valid_packets",
                self.total_valid_packets.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
            (
                "total_verify_time_us",
                self.total_verify_time_us.swap(0, Ordering::Relaxed) as i64,
                i64
            ),
        );

        self.recv_batches_us_hist = histogram::Histogram::new();
        self.dedup_packets_pp_us_hist = histogram::Histogram::new();
        self.batches_hist = histogram::Histogram::new();
        self.packets_hist = histogram::Histogram::new();
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
    ) -> (Self, GossipSigVerifyHandle) {
        let (gossip_verified_vote_sender, verified_vote_receiver) = unbounded();
        let non_vote_stats = SigVerifierStats::default();
        let tpu_vote_stats = SigVerifierStats::default();
        let worker_pool = SigVerifyWorkerPool::new(
            num_workers,
            non_vote_sender,
            tpu_vote_sender,
            gossip_verified_vote_sender,
            forward_stage_sender,
            forward_non_votes,
            SigVerifyWorkerStats {
                total_valid_packets: non_vote_stats.total_valid_packets.clone(),
                total_verify_time_us: non_vote_stats.total_verify_time_us.clone(),
            },
            SigVerifyWorkerStats {
                total_valid_packets: tpu_vote_stats.total_valid_packets.clone(),
                total_verify_time_us: tpu_vote_stats.total_verify_time_us.clone(),
            },
        );
        let non_vote_thread_hdl = Self::verifier_service(
            packet_receiver,
            worker_pool.non_vote_verifier(),
            "solSigVerTpu",
            "tpu-verifier",
            non_vote_stats,
        );
        let tpu_vote_thread_hdl = Self::verifier_service(
            vote_packet_receiver,
            worker_pool.tpu_vote_verifier(),
            "solSigVerTpuVot",
            "tpu-vote-verifier",
            tpu_vote_stats,
        );
        let gossip_sigverify_handle = GossipSigVerifyHandle {
            verifier: worker_pool.gossip_verifier(),
            verified_vote_receiver,
        };

        (
            Self {
                non_vote_thread_hdl,
                tpu_vote_thread_hdl,
                _worker_pool: worker_pool,
            },
            gossip_sigverify_handle,
        )
    }

    pub fn join(self) -> thread::Result<()> {
        let non_vote_result = self.non_vote_thread_hdl.join();
        let tpu_vote_result = self.tpu_vote_thread_hdl.join();
        non_vote_result?;
        tpu_vote_result?;
        Ok(())
    }

    fn verifier<const K: usize>(
        deduper: &Deduper<K, [u8]>,
        recvr: &Receiver<PacketBatch>,
        verifier: &mut TransactionSigVerifier,
        stats: &mut SigVerifierStats,
    ) -> Result<()> {
        const SOFT_RECEIVE_CAP: usize = 5_000;
        let (mut batches, num_packets, recv_duration) =
            streamer::recv_packet_batches(recvr, SOFT_RECEIVE_CAP)?;

        let batches_len = batches.len();
        stats
            .recv_batches_us_hist
            .increment(recv_duration.as_micros() as u64)
            .unwrap();
        stats.batches_hist.increment(batches_len as u64).unwrap();
        stats.packets_hist.increment(num_packets as u64).unwrap();
        stats.total_batches += batches_len;
        stats.total_packets += num_packets;

        let mut dedup_time = Measure::start("sigverify_dedup_time");
        let discard_or_dedup_fail =
            deduper::dedup_packets_and_count_discards(deduper, &mut batches) as usize;
        dedup_time.stop();
        stats.total_dropped_on_capacity += verifier.send_packets_to_worker_pool(batches)?;
        stats
            .dedup_packets_pp_us_hist
            .increment(dedup_time.as_us() / (num_packets as u64))
            .unwrap();
        stats.total_dedup += discard_or_dedup_fail;
        stats.total_dedup_time_us += dedup_time.as_us() as usize;

        Ok(())
    }

    fn verifier_service(
        packet_receiver: Receiver<PacketBatch>,
        mut verifier: TransactionSigVerifier,
        thread_name: &'static str,
        metrics_name: &'static str,
        mut stats: SigVerifierStats,
    ) -> JoinHandle<()> {
        let mut last_print = Instant::now();
        const MAX_DEDUPER_AGE: Duration = Duration::from_secs(2);
        const DEDUPER_FALSE_POSITIVE_RATE: f64 = 0.001;
        const DEDUPER_NUM_BITS: u64 = 63_999_979;
        Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                let mut rng = rand::rng();
                let mut deduper = Deduper::<2, [u8]>::new(&mut rng, DEDUPER_NUM_BITS);
                loop {
                    if deduper.maybe_reset(&mut rng, DEDUPER_FALSE_POSITIVE_RATE, MAX_DEDUPER_AGE) {
                        stats.num_deduper_saturations += 1;
                    }
                    if let Err(e) =
                        Self::verifier(&deduper, &packet_receiver, &mut verifier, &mut stats)
                    {
                        match e {
                            SigVerifyServiceError::Streamer(StreamerError::RecvTimeout(
                                RecvTimeoutError::Disconnected,
                            )) => break,
                            SigVerifyServiceError::Streamer(StreamerError::RecvTimeout(
                                RecvTimeoutError::Timeout,
                            )) => (),
                            _ => error!("{e:?}"),
                        }
                    }
                    if last_print.elapsed() > SigVerifierStats::REPORT_INTERVAL {
                        stats.maybe_report_and_reset(metrics_name);
                        last_print = Instant::now();
                    }
                }
            })
            .unwrap()
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
        crossbeam_channel::unbounded,
        solana_perf::{packet::to_packet_batches, test_tx::test_tx},
    };

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
        let (packet_s, packet_r) = unbounded();
        let (vote_packet_s, vote_packet_r) = unbounded();
        let (verified_s, verified_r) = BankingTracer::channel_for_test();
        let (tpu_vote_s, _tpu_vote_r) = BankingTracer::channel_for_test();
        let (forward_stage_s, _forward_stage_r) = unbounded();
        let (stage, gossip_sigverify_handle) = SigVerifyStage::new(
            packet_r,
            vote_packet_r,
            verified_s,
            tpu_vote_s,
            forward_stage_s,
            NonZeroUsize::new(4).unwrap(),
            false,
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
                valid_received += verifieds
                    .iter()
                    .map(|batch| batch.iter().filter(|p| !p.meta().discard()).count())
                    .sum::<usize>();
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
}
