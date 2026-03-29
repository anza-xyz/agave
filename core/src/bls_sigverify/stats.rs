#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    agave_math_utils::welford_stats::WelfordStats,
    solana_clock::Slot,
    std::time::{Duration, Instant},
};

/// Max number of root slots to wait before triggering reporting of stats.  At 400ms slot times, this is 4s.
const SLOTS_INTERVAL: Slot = 10;
/// Max amount of seconds to wait before triggering reporting of stats.
const DURATION_INTERVAL: Duration = Duration::from_secs(5);

/// A struct to control when stats should be reported depending on how many slots or time has passed.
#[derive(Debug)]
pub(super) struct Reporting {
    /// The last time when reporting was done.
    time: Instant,
    /// The last slot when reporting was done.
    slot: Slot,
}

impl Reporting {
    fn new(root_slot: Slot) -> Self {
        Self {
            time: Instant::now(),
            slot: root_slot,
        }
    }

    /// Returns `true` if reporting should be done else `false`.
    fn should_report(&self, root_slot: Slot) -> bool {
        self.slot + SLOTS_INTERVAL >= root_slot || self.time.elapsed() > DURATION_INTERVAL
    }
}

/// Stats for the sigverifier.
#[derive(Debug)]
pub(super) struct SigVerifierStats {
    /// Stats for sigverifying votes.
    pub(super) vote_stats: SigVerifyVoteStats,
    /// Stats for sigverifying certs.
    pub(super) cert_stats: SigVerifyCertStats,
    /// Stats on how long [`verify_and_send_batch`] took.
    pub(super) verify_and_send_batch_us: WelfordStats,
    /// Stats on how long [`extract_and_filter_msgs`] took.
    pub(super) extract_filter_msgs_us: WelfordStats,
    /// Number of packets received.
    pub(super) num_pkts: WelfordStats,
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
    /// Number of certs received that the node has already generated.
    pub(super) num_generated_certs_received: u64,
    /// Last time the stats were reported.
    pub(super) last_report: Reporting,
}

impl SigVerifierStats {
    pub(super) fn new(root_slot: Slot) -> Self {
        Self {
            vote_stats: SigVerifyVoteStats::default(),
            cert_stats: SigVerifyCertStats::default(),
            extract_filter_msgs_us: WelfordStats::default(),
            num_pkts: WelfordStats::default(),
            discard_vote_invalid_rank: 0,
            num_discarded_pkts: 0,
            num_malformed_pkts: 0,
            discard_vote_no_epoch_stakes: 0,
            num_old_votes_received: 0,
            num_old_certs_received: 0,
            num_verified_certs_received: 0,
            num_generated_certs_received: 0,
            verify_and_send_batch_us: WelfordStats::default(),
            last_report: Reporting::new(root_slot),
        }
    }

    /// Reports stats if they have not been reported in some time.
    ///
    /// Also resets all stats.
    pub(super) fn maybe_report(&mut self, root_slot: Slot) {
        if self.last_report.should_report(root_slot) {
            self.do_report(root_slot);
            *self = SigVerifierStats::new(root_slot);
        }
    }

    /// Reports stats regardless of when they were last reported.
    pub(super) fn do_report(&mut self, root_slot: Slot) {
        let Self {
            vote_stats,
            cert_stats,
            extract_filter_msgs_us,
            num_pkts,
            num_discarded_pkts,
            num_malformed_pkts,
            num_old_votes_received,
            num_old_certs_received,
            num_verified_certs_received,
            num_generated_certs_received,
            discard_vote_invalid_rank,
            discard_vote_no_epoch_stakes,
            verify_and_send_batch_us,
            last_report: _,
        } = self;

        vote_stats.report();
        cert_stats.report();
        datapoint_info!(
            "bls_sig_verifier_stats",
            ("root_slot", root_slot, i64),
            (
                "extract_and_verify_us_count",
                extract_filter_msgs_us.count(),
                i64
            ),
            (
                "extract_and_verify_us_mean",
                extract_filter_msgs_us.mean().unwrap_or(0),
                i64
            ),
            ("discard_vote_invalid_rank", *discard_vote_invalid_rank, i64),
            ("num_discarded_pkts", *num_discarded_pkts, i64),
            ("num_old_votes_received", *num_old_votes_received, i64),
            (
                "num_verified_certs_received",
                *num_verified_certs_received,
                i64
            ),
            (
                "num_generated_certs_received",
                *num_generated_certs_received,
                i64
            ),
            (
                "discard_vote_no_epoch_stakes",
                *discard_vote_no_epoch_stakes,
                i64
            ),
            ("num_malformed_pkts", *num_malformed_pkts, i64),
            ("num_old_certs_received", *num_old_certs_received, i64),
            (
                "verify_and_send_batch_us_max",
                verify_and_send_batch_us.maximum().unwrap_or(0),
                i64
            ),
            (
                "verify_and_send_batch_us_mean",
                verify_and_send_batch_us.mean().unwrap_or(0),
                i64
            ),
            (
                "verify_and_send_batch_us_count",
                verify_and_send_batch_us.count(),
                i64
            ),
            ("num_pkts_max", num_pkts.maximum().unwrap_or(0), i64),
            ("num_pkts_mean", num_pkts.mean().unwrap_or(0), i64),
            ("num_pkts_count", num_pkts.count(), i64),
        );
    }
}

/// Stats from sigverifying certs.
#[derive(Default, Debug)]
pub(super) struct SigVerifyCertStats {
    /// Number of certs [`verify_and_send_certificates`] was requested to verify the signature of.
    pub(super) certs_to_sig_verify: u64,
    /// Number of certs [`verify_and_send_certificates`] successfully verified the signature of.
    pub(super) sig_verified_certs: u64,
    /// Number of certs that were verified unnecessarily because another cert of the same
    /// `CertificateType` was already verified.
    pub(super) unnecessary_certs_verified: u64,

    /// Number of times stake verification failed on a cert.
    pub(super) stake_verification_failed: u64,
    /// Number of times signature verification failed on a cert.
    pub(super) signature_verification_failed: u64,
    /// Number of times the cert was too far in the future and discarded.
    pub(super) too_far_in_future: u64,

    /// Number of votes sent successfully over the channel to consensus pool.
    pub(super) pool_sent: u64,
    /// Number of times the channel to consensus pool was full.
    pub(super) pool_channel_full: u64,

    /// Stats for [`verify_and_send_certificates`].
    pub(super) fn_verify_and_send_certs_stats: WelfordStats,
}

impl SigVerifyCertStats {
    pub(super) fn merge(&mut self, other: Self) {
        let Self {
            certs_to_sig_verify,
            sig_verified_certs,
            unnecessary_certs_verified,
            stake_verification_failed,
            signature_verification_failed,
            too_far_in_future,
            pool_sent,
            pool_channel_full,
            fn_verify_and_send_certs_stats,
        } = other;
        self.certs_to_sig_verify += certs_to_sig_verify;
        self.sig_verified_certs += sig_verified_certs;
        self.unnecessary_certs_verified += unnecessary_certs_verified;
        self.stake_verification_failed += stake_verification_failed;
        self.signature_verification_failed += signature_verification_failed;
        self.too_far_in_future += too_far_in_future;
        self.pool_sent += pool_sent;
        self.pool_channel_full += pool_channel_full;
        self.fn_verify_and_send_certs_stats
            .merge(fn_verify_and_send_certs_stats);
    }

    pub(super) fn report(&self) {
        let Self {
            certs_to_sig_verify,
            sig_verified_certs,
            unnecessary_certs_verified,
            stake_verification_failed,
            signature_verification_failed,
            too_far_in_future,
            pool_sent,
            pool_channel_full,
            fn_verify_and_send_certs_stats,
        } = self;
        datapoint_info!(
            "bls_cert_sigverify_stats",
            ("certs_to_sig_verify", *certs_to_sig_verify, i64),
            ("sig_verified_certs", *sig_verified_certs, i64),
            (
                "unnecessary_certs_verified",
                *unnecessary_certs_verified,
                i64
            ),
            ("stake_verification_failed", *stake_verification_failed, i64),
            (
                "signature_verification_failed",
                *signature_verification_failed,
                i64
            ),
            ("too_far_in_future", *too_far_in_future, i64),
            ("pool_sent", *pool_sent, i64),
            ("pool_channel_full", *pool_channel_full, i64),
            (
                "fn_verify_and_send_certs_count",
                fn_verify_and_send_certs_stats.count(),
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
#[derive(Debug, Default)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(super) struct SigVerifyVoteStats {
    /// Number of votes [`verify_and_send_votes`] was requested to verify the signature of.
    pub(super) votes_to_sig_verify: u64,
    /// Number of votes [`verify_and_send_votes`] successfully verified the signature of.
    pub(super) sig_verified_votes: u64,

    /// Number of times the cert was too far in the future and discarded.
    pub(super) too_far_in_future: u64,

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
    pub(super) fn_verify_and_send_votes_stats: WelfordStats,
    /// Stats for [`verify_votes_optimistic`].
    pub(super) fn_verify_votes_optimistic_stats: WelfordStats,
    /// Stats for [`verify_individual_votes`].
    pub(super) fn_verify_individual_votes_stats: WelfordStats,

    /// Stats for number of distinct votes in batches.
    pub(super) distinct_votes_stats: WelfordStats,
}

impl SigVerifyVoteStats {
    pub(super) fn merge(&mut self, other: Self) {
        let Self {
            votes_to_sig_verify,
            sig_verified_votes,
            too_far_in_future,
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
        } = other;
        self.votes_to_sig_verify += votes_to_sig_verify;
        self.sig_verified_votes += sig_verified_votes;
        self.too_far_in_future += too_far_in_future;
        self.metrics_sent += metrics_sent;
        self.metrics_channel_full += metrics_channel_full;
        self.rewards_sent += rewards_sent;
        self.rewards_channel_full += rewards_channel_full;
        self.repair_sent += repair_sent;
        self.repair_channel_full += repair_channel_full;
        self.pool_sent += pool_sent;
        self.pool_channel_full += pool_channel_full;
        self.fn_verify_and_send_votes_stats
            .merge(fn_verify_and_send_votes_stats);
        self.fn_verify_votes_optimistic_stats
            .merge(fn_verify_votes_optimistic_stats);
        self.fn_verify_individual_votes_stats
            .merge(fn_verify_individual_votes_stats);
        self.distinct_votes_stats.merge(distinct_votes_stats);
    }

    pub(super) fn report(&self) {
        let Self {
            votes_to_sig_verify,
            sig_verified_votes,
            too_far_in_future,
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
            ("too_far_in_future", *too_far_in_future, i64),
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
                fn_verify_and_send_votes_stats.count(),
                i64
            ),
            (
                "fn_verify_and_send_votes_mean",
                fn_verify_and_send_votes_stats.mean().unwrap_or(0),
                i64
            ),
            (
                "fn_verify_votes_optimistic_count",
                fn_verify_votes_optimistic_stats.count(),
                i64
            ),
            (
                "fn_verify_votes_optimistic_mean",
                fn_verify_votes_optimistic_stats.mean().unwrap_or(0),
                i64
            ),
            (
                "fn_verify_individual_votes_count",
                fn_verify_individual_votes_stats.count(),
                i64
            ),
            (
                "fn_verify_individual_votes_mean",
                fn_verify_individual_votes_stats.mean().unwrap_or(0),
                i64
            ),
            ("distinct_votes_count", distinct_votes_stats.count(), i64),
            (
                "distinct_votes_mean",
                distinct_votes_stats.mean().unwrap_or(0),
                i64
            ),
        );
    }
}
