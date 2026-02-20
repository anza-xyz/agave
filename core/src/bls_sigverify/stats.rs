#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    super::{bls_cert_sigverify::Stats as CertStats, bls_vote_sigverify::Stats as VoteStats},
    histogram::Histogram,
    std::time::{Duration, Instant},
};

pub(super) const STATS_INTERVAL_DURATION: Duration = Duration::from_secs(1);

pub(super) struct PacketStats {
    /// Measurements of [`BLSSigVerifyService::receive_packets`].
    recv_batches_hist: Histogram,
    /// Measurements of [`BLSSigVerifyService::verify_packets`].
    verify_batches_hist: Histogram,
    /// Measurements of batches sizes received from [`BLSSigVerifyService::receive_packets`].
    batches_hist: Histogram,
    /// Measurements of packets received from [`BLSSigVerifyService::receive_packets`].
    packets_hist: Histogram,
    /// Total amount of time spent calling [`BLSSigVerifyService::verify_packets`].
    total_verify_time_us: u64,
    /// Tracks when stats were last reported.
    last_report: Instant,
}

impl Default for PacketStats {
    fn default() -> Self {
        Self {
            recv_batches_hist: Histogram::default(),
            verify_batches_hist: Histogram::default(),
            batches_hist: Histogram::default(),
            packets_hist: Histogram::default(),
            total_verify_time_us: 0,
            last_report: Instant::now(),
        }
    }
}

impl PacketStats {
    pub(super) fn update(
        &mut self,
        num_packets_received: u64,
        num_batches_received: u64,
        receive_packets_us: u64,
        verify_packets_us: u64,
    ) {
        self.recv_batches_hist
            .increment(receive_packets_us)
            .unwrap();
        self.verify_batches_hist
            .increment(verify_packets_us / (num_packets_received))
            .unwrap();
        self.batches_hist.increment(num_batches_received).unwrap();
        self.packets_hist.increment(num_packets_received).unwrap();
        self.total_verify_time_us += verify_packets_us;
    }

    pub(super) fn maybe_report(&mut self) {
        let Self {
            recv_batches_hist,
            verify_batches_hist,
            batches_hist,
            packets_hist,
            total_verify_time_us,
            last_report,
        } = self;

        if last_report.elapsed() < STATS_INTERVAL_DURATION || batches_hist.entries() == 0 {
            return;
        }

        datapoint_info!(
            "bls-verifier-packet-stats",
            (
                "recv_batches_us_90pct",
                recv_batches_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            (
                "recv_batches_us_min",
                recv_batches_hist.minimum().unwrap_or(0),
                i64
            ),
            (
                "recv_batches_us_max",
                recv_batches_hist.maximum().unwrap_or(0),
                i64
            ),
            (
                "recv_batches_us_mean",
                recv_batches_hist.mean().unwrap_or(0),
                i64
            ),
            ("recv_batches_count", recv_batches_hist.entries(), i64),
            (
                "verify_batches_us_90pct",
                verify_batches_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            (
                "verify_batches_us_min",
                verify_batches_hist.minimum().unwrap_or(0),
                i64
            ),
            (
                "verify_batches_us_max",
                verify_batches_hist.maximum().unwrap_or(0),
                i64
            ),
            (
                "verify_batches_us_mean",
                verify_batches_hist.mean().unwrap_or(0),
                i64
            ),
            (
                "verify_batches_us_count",
                verify_batches_hist.entries(),
                i64
            ),
            (
                "batches_90pct",
                batches_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            ("batches_min", batches_hist.minimum().unwrap_or(0), i64),
            ("batches_max", batches_hist.maximum().unwrap_or(0), i64),
            ("batches_mean", batches_hist.mean().unwrap_or(0), i64),
            ("batches_count", batches_hist.entries(), i64),
            (
                "packets_90pct",
                packets_hist.percentile(90.0).unwrap_or(0),
                i64
            ),
            ("packets_min", packets_hist.minimum().unwrap_or(0), i64),
            ("packets_max", packets_hist.maximum().unwrap_or(0), i64),
            ("packets_mean", packets_hist.mean().unwrap_or(0), i64),
            ("packets_count", packets_hist.entries(), i64),
            ("total_verify_time_us", *total_verify_time_us, i64),
        );

        *self = Self::default();
    }
}

// We are adding our own stats because we do BLS decoding in batch verification,
// and we send one BLS message at a time. So it makes sense to have finer-grained stats
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(super) struct Stats {
    pub(super) vote_stats: VoteStats,
    pub(super) cert_stats: CertStats,

    pub(super) preprocess_count: u64,
    pub(super) preprocess_elapsed_us: u64,

    pub(super) received: u64,
    pub(super) received_bad_rank: u64,
    pub(super) received_discarded: u64,
    pub(super) received_malformed: u64,
    pub(super) received_no_epoch_stakes: u64,
    pub(super) received_old: u64,
    pub(super) received_verified: u64,

    pub(super) last_stats_logged: Instant,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            vote_stats: VoteStats::default(),
            cert_stats: CertStats::default(),

            preprocess_count: 0,
            preprocess_elapsed_us: 0,

            received: 0,
            received_bad_rank: 0,
            received_discarded: 0,
            received_malformed: 0,
            received_no_epoch_stakes: 0,
            received_old: 0,
            received_verified: 0,
            last_stats_logged: Instant::now(),
        }
    }
}

impl Stats {
    /// If sufficient time has passed since last report, report stats.
    pub(super) fn maybe_report(&mut self) {
        let now = Instant::now();
        let time_since_last_log = now.duration_since(self.last_stats_logged);
        if time_since_last_log < STATS_INTERVAL_DURATION {
            return;
        }
        self.vote_stats.report();
        self.cert_stats.report();
        datapoint_info!(
            "bls_sig_verifier_stats",
            ("preprocess_count", self.preprocess_count, i64),
            ("preprocess_elapsed_us", self.preprocess_elapsed_us, i64),
            ("received", self.received, i64),
            ("received_bad_rank", self.received_bad_rank, i64),
            ("received_discarded", self.received_discarded, i64),
            ("received_old", self.received_old, i64),
            ("received_verified", self.received_verified, i64),
            (
                "received_no_epoch_stakes",
                self.received_no_epoch_stakes,
                i64
            ),
            ("received_malformed", self.received_malformed, i64),
        );
        *self = Stats::default();
    }
}
