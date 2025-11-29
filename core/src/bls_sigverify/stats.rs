use {
    solana_measure::measure::Measure,
    std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, Instant},
    },
};

pub(crate) const STATS_INTERVAL_DURATION: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub(crate) struct BLSPreVerifyStats {
    pub(crate) recv_batches_us_hist: histogram::Histogram, // time to call recv_batch
    pub(crate) verify_batches_pp_us_hist: histogram::Histogram, // per-packet time to call verify_batch
    pub(crate) dedup_packets_pp_us_hist: histogram::Histogram, // per-packet time to call verify_batch
    pub(crate) batches_hist: histogram::Histogram, // number of packet batches per verify call
    pub(crate) packets_hist: histogram::Histogram, // number of packets per verify call
    pub(crate) num_deduper_saturations: usize,
    pub(crate) total_batches: usize,
    pub(crate) total_packets: usize,
    pub(crate) total_dedup: usize,
    pub(crate) total_dedup_time_us: usize,
    pub(crate) last_stats_logged: Instant,
}

impl BLSPreVerifyStats {
    pub(crate) fn new() -> Self {
        Self {
            recv_batches_us_hist: histogram::Histogram::default(),
            verify_batches_pp_us_hist: histogram::Histogram::default(),
            dedup_packets_pp_us_hist: histogram::Histogram::default(),
            batches_hist: histogram::Histogram::default(),
            packets_hist: histogram::Histogram::default(),
            num_deduper_saturations: 0,
            total_batches: 0,
            total_packets: 0,
            total_dedup: 0,
            total_dedup_time_us: 0,
            last_stats_logged: Instant::now(),
        }
    }

    pub(crate) fn maybe_report(&mut self) {
        if Instant::now().duration_since(self.last_stats_logged) < STATS_INTERVAL_DURATION {
            return;
        }
        datapoint_info!(
            "bls_pre_verify_stats",
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
                "verify_batches_pp_us_90pct",
                self.verify_batches_pp_us_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            (
                "verify_batches_pp_us_min",
                self.verify_batches_pp_us_hist.minimum().unwrap_or(0),
                i64
            ),
            (
                "verify_batches_pp_us_max",
                self.verify_batches_pp_us_hist.maximum().unwrap_or(0),
                i64
            ),
            (
                "verify_batches_pp_us_mean",
                self.verify_batches_pp_us_hist.mean().unwrap_or(0),
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
            ("num_deduper_saturations", self.num_deduper_saturations, i64),
            ("total_batches", self.total_batches, i64),
            ("total_packets", self.total_packets, i64),
            ("total_dedup", self.total_dedup, i64),
            ("total_dedup_time_us", self.total_dedup_time_us, i64),
        );
        *self = BLSPreVerifyStats::new();
    }

    pub fn increase_stats(
        &mut self,
        recv_duration: &Duration,
        verify_time: &Measure,
        dedup_time: &Measure,
        batches_len: usize,
        num_packets: usize,
        discard_or_dedup_fail: usize,
    ) {
        self.recv_batches_us_hist
            .increment(recv_duration.as_micros() as u64)
            .unwrap();
        self.verify_batches_pp_us_hist
            .increment(verify_time.as_us() / (num_packets as u64))
            .unwrap();
        self.dedup_packets_pp_us_hist
            .increment(dedup_time.as_us() / (num_packets as u64))
            .unwrap();
        self.batches_hist.increment(batches_len as u64).unwrap();
        self.packets_hist.increment(num_packets as u64).unwrap();
        self.total_batches += batches_len;
        self.total_packets += num_packets;
        self.total_dedup += discard_or_dedup_fail;
        self.total_dedup_time_us += dedup_time.as_us() as usize;
    }
}

// Finer grained stats compared to other verifiers because BLS decoding
// is done in batch verification and we send one BLS message at a time.
#[derive(Debug)]
pub(crate) struct BLSSigVerifierStats {
    pub(crate) total_valid_packets: AtomicU64,

    pub(crate) preprocess_count: AtomicU64,
    pub(crate) preprocess_elapsed_us: AtomicU64,
    pub(crate) votes_batch_count: AtomicU64,
    pub(crate) votes_batch_distinct_messages_count: AtomicU64,
    pub(crate) votes_batch_optimistic_elapsed_us: AtomicU64,
    pub(crate) votes_batch_parallel_verify_count: AtomicU64,
    pub(crate) votes_batch_parallel_verify_elapsed_us: AtomicU64,
    pub(crate) certs_batch_count: AtomicU64,
    pub(crate) certs_batch_elapsed_us: AtomicU64,
    pub(crate) verify_elapsed_us: AtomicU64,

    pub(crate) sent: AtomicU64,
    pub(crate) sent_failed: AtomicU64,
    pub(crate) verified_votes_sent: AtomicU64,
    pub(crate) verified_votes_sent_failed: AtomicU64,
    pub(crate) received: AtomicU64,
    pub(crate) received_bad_rank: AtomicU64,
    pub(crate) received_bad_signature_certs: AtomicU64,
    pub(crate) received_bad_signature_votes: AtomicU64,
    pub(crate) received_discarded: AtomicU64,
    pub(crate) received_malformed: AtomicU64,
    pub(crate) received_no_epoch_stakes: AtomicU64,
    pub(crate) received_old: AtomicU64,
    pub(crate) received_verified: AtomicU64,
    pub(crate) received_votes: AtomicU64,
    pub(crate) last_stats_logged: Instant,
}

impl BLSSigVerifierStats {
    pub(crate) fn new() -> Self {
        Self {
            total_valid_packets: AtomicU64::new(0),

            preprocess_count: AtomicU64::new(0),
            preprocess_elapsed_us: AtomicU64::new(0),
            votes_batch_count: AtomicU64::new(0),
            votes_batch_distinct_messages_count: AtomicU64::new(0),
            votes_batch_optimistic_elapsed_us: AtomicU64::new(0),
            votes_batch_parallel_verify_count: AtomicU64::new(0),
            votes_batch_parallel_verify_elapsed_us: AtomicU64::new(0),
            certs_batch_count: AtomicU64::new(0),
            certs_batch_elapsed_us: AtomicU64::new(0),
            verify_elapsed_us: AtomicU64::new(0),

            sent: AtomicU64::new(0),
            sent_failed: AtomicU64::new(0),
            verified_votes_sent: AtomicU64::new(0),
            verified_votes_sent_failed: AtomicU64::new(0),
            received: AtomicU64::new(0),
            received_bad_rank: AtomicU64::new(0),
            received_bad_signature_certs: AtomicU64::new(0),
            received_bad_signature_votes: AtomicU64::new(0),
            received_discarded: AtomicU64::new(0),
            received_malformed: AtomicU64::new(0),
            received_no_epoch_stakes: AtomicU64::new(0),
            received_old: AtomicU64::new(0),
            received_verified: AtomicU64::new(0),
            received_votes: AtomicU64::new(0),
            last_stats_logged: Instant::now(),
        }
    }

    /// If sufficient time has passed since last report, report stats.
    pub(crate) fn maybe_report_stats(&mut self) {
        let now = Instant::now();
        let time_since_last_log = now.duration_since(self.last_stats_logged);
        if time_since_last_log < STATS_INTERVAL_DURATION {
            return;
        }
        datapoint_info!(
            "bls_sig_verifier_stats",
            (
                "preprocess_count",
                self.preprocess_count.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "preprocess_elapsed_us",
                self.preprocess_elapsed_us.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "votes_batch_count",
                self.votes_batch_count.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "votes_batch_distinct_messages_count",
                self.votes_batch_distinct_messages_count
                    .load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "votes_batch_optimistic_elapsed_us",
                self.votes_batch_optimistic_elapsed_us
                    .load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "votes_batch_parallel_verify_count",
                self.votes_batch_parallel_verify_count
                    .load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "votes_batch_parallel_verify_elapsed_us",
                self.votes_batch_parallel_verify_elapsed_us
                    .load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "certs_batch_count",
                self.certs_batch_count.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "certs_batch_elapsed_us",
                self.certs_batch_elapsed_us.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "verify_elapsed_us",
                self.verify_elapsed_us.load(Ordering::Relaxed) as i64,
                i64
            ),
            ("sent", self.sent.load(Ordering::Relaxed) as i64, i64),
            (
                "sent_failed",
                self.sent_failed.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "verified_votes_sent",
                self.verified_votes_sent.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "verified_votes_sent_failed",
                self.verified_votes_sent_failed.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "received",
                self.received.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "received_bad_rank",
                self.received_bad_rank.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "received_bad_signature_certs",
                self.received_bad_signature_certs.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "received_bad_signature_votes",
                self.received_bad_signature_votes.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "received_discarded",
                self.received_discarded.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "received_old",
                self.received_old.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "received_verified",
                self.received_verified.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "received_votes",
                self.received_votes.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "received_no_epoch_stakes",
                self.received_no_epoch_stakes.load(Ordering::Relaxed) as i64,
                i64
            ),
            (
                "received_malformed",
                self.received_malformed.load(Ordering::Relaxed) as i64,
                i64
            ),
        );
        *self = BLSSigVerifierStats::new();
    }
}
