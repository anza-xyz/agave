#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    histogram::Histogram,
    std::time::{Duration, Instant},
};

pub(super) const STATS_INTERVAL_DURATION: Duration = Duration::from_secs(1);

/// Stats for when the sigverifier is receiving packets from the streamer.
pub(super) struct StreamerRecvStats {
    /// Stats on amount of time streamer::recv() took.
    pub(super) streamer_recv_us: Histogram,
    /// Stats on how long [`verify_and_send_batches`] took.
    pub(super) verify_and_send_batches_us: Histogram,
    /// Stats on how many batches the streamer sent.
    pub(super) num_batches: Histogram,
    /// Stats on how many packets the streamer sent.
    pub(super) num_packets: Histogram,
    /// Tracks when stats were last reported.
    pub(super) last_report: Instant,
}

impl Default for StreamerRecvStats {
    fn default() -> Self {
        Self {
            streamer_recv_us: Histogram::default(),
            verify_and_send_batches_us: Histogram::default(),
            num_batches: Histogram::default(),
            num_packets: Histogram::default(),
            last_report: Instant::now(),
        }
    }
}

impl StreamerRecvStats {
    pub(super) fn maybe_report(&mut self) {
        let Self {
            streamer_recv_us,
            verify_and_send_batches_us,
            num_batches,
            num_packets,
            last_report,
        } = self;

        if last_report.elapsed() < STATS_INTERVAL_DURATION || num_batches.entries() == 0 {
            return;
        }

        datapoint_info!(
            "bls-sigverifier-streamer-recv-stats",
            (
                "streamer_recv_us_90pct",
                streamer_recv_us.percentile(90.0).unwrap_or(0),
                i64
            ),
            (
                "streamer_recv_us_min",
                streamer_recv_us.minimum().unwrap_or(0),
                i64
            ),
            (
                "streamer_recv_us_max",
                streamer_recv_us.maximum().unwrap_or(0),
                i64
            ),
            (
                "streamer_recv_us_mean",
                streamer_recv_us.mean().unwrap_or(0),
                i64
            ),
            ("streamer_recv_us_count", streamer_recv_us.entries(), i64),
            (
                "verify_and_send_batches_us_90pct",
                verify_and_send_batches_us.percentile(90.0).unwrap_or(0),
                i64
            ),
            (
                "verify_and_send_batches_us_min",
                verify_and_send_batches_us.minimum().unwrap_or(0),
                i64
            ),
            (
                "verify_and_send_batches_us_max",
                verify_and_send_batches_us.maximum().unwrap_or(0),
                i64
            ),
            (
                "verify_and_send_batches_us_mean",
                verify_and_send_batches_us.mean().unwrap_or(0),
                i64
            ),
            (
                "verify_and_send_batches_us_count",
                verify_and_send_batches_us.entries(),
                i64
            ),
            (
                "num_batches_90pct",
                num_batches.percentile(90.0).unwrap_or(0),
                i64
            ),
            ("num_batches_min", num_batches.minimum().unwrap_or(0), i64),
            ("num_batches_max", num_batches.maximum().unwrap_or(0), i64),
            ("num_batches_mean", num_batches.mean().unwrap_or(0), i64),
            ("num_batches_count", num_batches.entries(), i64),
            (
                "num_packets_90pct",
                num_packets.percentile(90.0).unwrap_or(0),
                i64
            ),
            ("num_packets_min", num_packets.minimum().unwrap_or(0), i64),
            ("num_packets_max", num_packets.maximum().unwrap_or(0), i64),
            ("num_packets_mean", num_packets.mean().unwrap_or(0), i64),
            ("num_packets_count", num_packets.entries(), i64),
        );

        *self = Self::default();
    }
}

/// Stats for the sigverifier itself.
pub(super) struct SigVerifierStats {
    /// Stats for sigverifying votes.
    pub(super) vote_stats: SigVerifyVoteStats,
    /// Stats for sigverifying certs.
    pub(super) cert_stats: SigVerifyCertStats,
    /// Stats on how long [`extract_and_filter_msgs`] took.
    pub(super) extract_filter_msgs_us: Histogram,
    /// Number of packets received from the streamer.
    pub(super) num_pkts_received: u64,
    /// Number of discarded packets received from the streamer.
    pub(super) num_discarded_pkts: u64,
    /// Number of times we failed to deserialize a packet.
    pub(super) num_malformed_pkts: u64,
    /// Number of votes discarded due to an invalid rank.
    pub(super) discard_vote_invalid_rank: u64,
    /// Number of votes discarded due to no epoch stakes.
    pub(super) discard_vote_no_epoch_stakes: u64,
    /// Number of outdated votes received.
    pub(super) num_old_votes_received: u64,
    /// Number of outdated certs received.
    pub(super) num_old_certs_received: u64,
    /// Number of already verified certs received.
    pub(super) num_verified_certs_received: u64,
    /// Last time the stats were reported.
    pub(super) last_report: Instant,
}

impl Default for SigVerifierStats {
    fn default() -> Self {
        Self {
            vote_stats: SigVerifyVoteStats::default(),
            cert_stats: SigVerifyCertStats::default(),
            extract_filter_msgs_us: Histogram::new(),
            num_pkts_received: 0,
            discard_vote_invalid_rank: 0,
            num_discarded_pkts: 0,
            num_malformed_pkts: 0,
            discard_vote_no_epoch_stakes: 0,
            num_old_votes_received: 0,
            num_old_certs_received: 0,
            num_verified_certs_received: 0,
            last_report: Instant::now(),
        }
    }
}

impl SigVerifierStats {
    pub(super) fn maybe_report(&mut self) {
        let Self {
            vote_stats,
            cert_stats,
            extract_filter_msgs_us,
            num_pkts_received,
            num_discarded_pkts,
            num_malformed_pkts,
            num_old_votes_received,
            num_old_certs_received,
            num_verified_certs_received,
            discard_vote_invalid_rank,
            discard_vote_no_epoch_stakes,
            last_report,
        } = self;
        if last_report.elapsed() < STATS_INTERVAL_DURATION {
            return;
        }

        vote_stats.report();
        cert_stats.report();
        datapoint_info!(
            "bls_sig_verifier_stats",
            (
                "extract_and_verify_us_count",
                extract_filter_msgs_us.entries(),
                i64
            ),
            (
                "extract_and_verify_us_mean",
                extract_filter_msgs_us.mean().unwrap_or(0),
                i64
            ),
            ("num_pkts_received", *num_pkts_received, i64),
            ("discard_vote_invalid_rank", *discard_vote_invalid_rank, i64),
            ("num_discarded_pkts", *num_discarded_pkts, i64),
            ("num_old_votes_received", *num_old_votes_received, i64),
            (
                "num_verified_certs_received",
                *num_verified_certs_received,
                i64
            ),
            (
                "discard_vote_no_epoch_stakes",
                *discard_vote_no_epoch_stakes,
                i64
            ),
            ("num_malformed_pkts", *num_malformed_pkts, i64),
            ("num_old_certs_received", *num_old_certs_received, i64),
        );
        *self = SigVerifierStats::default();
    }
}

/// Stats from sigverifying certs.
#[derive(Default)]
pub(super) struct SigVerifyCertStats {
    /// Number of certs [`verify_and_send_certificates`] was requested to verify the signature of.
    pub(super) certs_to_sig_verify: u64,
    /// Number of certs [`verify_and_send_certificates`] successfully verified the signature of.
    pub(super) sig_verified_certs: u64,

    /// Number of times stake verification failed on a cert.
    pub(super) stake_verification_failed: u64,
    /// Number of times signature verification failed on a cert.
    pub(super) signature_verification_failed: u64,

    /// Number of votes sent successfully over the channel to consensus pool.
    pub(super) pool_sent: u64,
    /// Number of times the channel to consensus pool was full.
    pub(super) pool_channel_full: u64,

    /// Stats for [`verify_and_send_certificates`].
    pub(super) fn_verify_and_send_certs_stats: Histogram,
}

impl SigVerifyCertStats {
    pub(super) fn merge(&mut self, other: Self) {
        let Self {
            certs_to_sig_verify,
            sig_verified_certs,
            stake_verification_failed,
            signature_verification_failed,
            pool_sent,
            pool_channel_full,
            fn_verify_and_send_certs_stats,
        } = other;
        self.certs_to_sig_verify += certs_to_sig_verify;
        self.sig_verified_certs += sig_verified_certs;
        self.stake_verification_failed += stake_verification_failed;
        self.signature_verification_failed += signature_verification_failed;
        self.pool_sent += pool_sent;
        self.pool_channel_full += pool_channel_full;
        self.fn_verify_and_send_certs_stats
            .merge(&fn_verify_and_send_certs_stats);
    }

    pub(super) fn report(&self) {
        let Self {
            certs_to_sig_verify,
            sig_verified_certs,
            stake_verification_failed,
            signature_verification_failed,
            pool_sent,
            pool_channel_full,
            fn_verify_and_send_certs_stats,
        } = self;
        datapoint_info!(
            "bls_cert_sigverify_stats",
            ("certs_to_sig_verify", *certs_to_sig_verify, i64),
            ("sig_verified_certs", *sig_verified_certs, i64),
            ("stake_verification_failed", *stake_verification_failed, i64),
            (
                "signature_verification_failed",
                *signature_verification_failed,
                i64
            ),
            ("pool_sent", *pool_sent, i64),
            ("pool_channel_full", *pool_channel_full, i64),
            (
                "fn_verify_and_send_certs_count",
                fn_verify_and_send_certs_stats.entries(),
                i64
            ),
            (
                "fn_verify_and_send_certs_mean",
                fn_verify_and_send_certs_stats.mean().unwrap_or(0),
                i64
            ),
        );
    }
}

/// Stats from sigverifying votes.
#[derive(Default)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(super) struct SigVerifyVoteStats {
    /// Number of votes [`verify_and_send_votes`] was requested to verify the signature of.
    pub(super) votes_to_sig_verify: u64,
    /// Number of votes [`verify_and_send_votes`] successfully verified the signature of.
    pub(super) sig_verified_votes: u64,

    /// Number of votes sent successfully over the channel to metrics.
    pub(super) metrics_sent: u64,
    /// Number of times the channel to metrics was full.
    pub(super) metrics_channel_full: u64,
    /// Number of votes sent successfully over the channel to rewards.
    pub(super) rewards_sent: u64,
    /// Number of times the channel to rewards was full.
    pub(super) rewards_channel_full: u64,
    /// Number of votes sent successfully over the channel to consensus pool.
    pub(super) pool_sent: u64,
    /// Number of times the channel to consensus pool was full.
    pub(super) pool_channel_full: u64,
    /// Number of votes sent successfully over the channel to repair.
    pub(super) repair_sent: u64,
    /// Number of times the channel to repair was full.
    pub(super) repair_channel_full: u64,

    /// Stats for [`verify_and_send_votes`].
    pub(super) fn_verify_and_send_votes_stats: Histogram,
    /// Stats for [`verify_votes_optimistic`].
    pub(super) fn_verify_votes_optimistic_stats: Histogram,
    /// Stats for [`verify_individual_votes`].
    pub(super) fn_verify_individual_votes_stats: Histogram,

    /// Stats for number of distinct votes in batches.
    pub(super) distinct_votes_stats: Histogram,
}

impl SigVerifyVoteStats {
    pub(super) fn merge(&mut self, other: Self) {
        let Self {
            votes_to_sig_verify: votes_to_verify,
            sig_verified_votes: verified_votes,
            metrics_sent,
            metrics_channel_full,
            rewards_sent,
            rewards_channel_full,
            repair_sent,
            repair_channel_full,
            pool_sent,
            pool_channel_full,
            fn_verify_and_send_votes_stats,
            fn_verify_votes_optimistic_stats,
            fn_verify_individual_votes_stats: fn_verify_individual_votes,
            distinct_votes_stats,
        } = other;
        self.votes_to_sig_verify += votes_to_verify;
        self.sig_verified_votes += verified_votes;
        self.metrics_sent += metrics_sent;
        self.metrics_channel_full += metrics_channel_full;
        self.rewards_sent += rewards_sent;
        self.rewards_channel_full += rewards_channel_full;
        self.repair_sent += repair_sent;
        self.repair_channel_full += repair_channel_full;
        self.pool_sent += pool_sent;
        self.pool_channel_full += pool_channel_full;
        self.fn_verify_and_send_votes_stats
            .merge(&fn_verify_and_send_votes_stats);
        self.fn_verify_votes_optimistic_stats
            .merge(&fn_verify_votes_optimistic_stats);
        self.fn_verify_individual_votes_stats
            .merge(&fn_verify_individual_votes);
        self.distinct_votes_stats.merge(&distinct_votes_stats);
    }

    pub(super) fn report(&self) {
        let Self {
            votes_to_sig_verify,
            sig_verified_votes,
            metrics_sent,
            metrics_channel_full,
            rewards_sent,
            rewards_channel_full,
            repair_sent,
            repair_channel_full,
            pool_sent,
            pool_channel_full,
            fn_verify_and_send_votes_stats,
            fn_verify_votes_optimistic_stats,
            fn_verify_individual_votes_stats,
            distinct_votes_stats,
        } = self;
        datapoint_info!(
            "bls_vote_sigverify_stats",
            ("votes_to_sig_verify", *votes_to_sig_verify, i64),
            ("sig_verified_votes", *sig_verified_votes, i64),
            ("metrics_sent", *metrics_sent, i64),
            ("metrics_channel_full", *metrics_channel_full, i64),
            ("rewards_sent", *rewards_sent, i64),
            ("rewards_channel_full", *rewards_channel_full, i64),
            ("repair_sent", *repair_sent, i64),
            ("repair_channel_full", *repair_channel_full, i64),
            ("pool_sent", *pool_sent, i64),
            ("pool_channel_full", *pool_channel_full, i64),
            (
                "fn_verify_and_send_votes_count",
                fn_verify_and_send_votes_stats.entries(),
                i64
            ),
            (
                "fn_verify_and_send_votes_mean",
                fn_verify_and_send_votes_stats.mean().unwrap_or(0),
                i64
            ),
            (
                "fn_verify_votes_optimistic_count",
                fn_verify_votes_optimistic_stats.entries(),
                i64
            ),
            (
                "fn_verify_votes_optimistic_mean",
                fn_verify_votes_optimistic_stats.mean().unwrap_or(0),
                i64
            ),
            (
                "fn_verify_individual_votes_count",
                fn_verify_individual_votes_stats.entries(),
                i64
            ),
            (
                "fn_verify_individual_votes_mean",
                fn_verify_individual_votes_stats.mean().unwrap_or(0),
                i64
            ),
            ("distinct_votes_count", distinct_votes_stats.entries(), i64),
            (
                "distinct_votes_mean",
                distinct_votes_stats.mean().unwrap_or(0),
                i64
            ),
        );
    }
}
