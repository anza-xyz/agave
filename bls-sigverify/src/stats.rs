#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    agave_math_utils::welford_stats::WelfordStats,
    solana_clock::Slot,
    solana_metrics::datapoint_info,
    std::{
        num::Saturating,
        time::{Duration, Instant},
    },
};

/// Max number of root slots to wait before triggering reporting of stats.
const SLOTS_INTERVAL: Slot = 10;
/// Max amount of seconds to wait before triggering reporting of stats.
const DURATION_INTERVAL: Duration = Duration::from_secs(5);

fn per_second(count: u64, elapsed: Duration) -> u64 {
    let elapsed_nanos = elapsed.as_nanos();
    if elapsed_nanos == 0 {
        return 0;
    }

    let rate = u128::from(count)
        .saturating_mul(1_000_000_000)
        .div_euclid(elapsed_nanos);
    u64::try_from(rate).unwrap_or(u64::MAX)
}

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

    /// Returns `Some(duration since last report)` if reporting should be done else `None`.
    fn should_report(&self, root_slot: Slot) -> Option<Duration> {
        let elapsed = self.time.elapsed();
        (root_slot >= self.slot.saturating_add(SLOTS_INTERVAL) || elapsed > DURATION_INTERVAL)
            .then_some(elapsed)
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
    pub(super) num_pkts: Saturating<u64>,
    /// Number of times we failed to deserialize a packet.
    pub(super) num_malformed_pkts: Saturating<u64>,
    /// Number of votes discarded due to an invalid rank.
    pub(super) discard_vote_invalid_rank: Saturating<u64>,
    /// Number of votes discarded due to no epoch stakes.
    pub(super) discard_vote_no_epoch_stakes: Saturating<u64>,
    /// Number of outdated votes received.
    pub(super) num_old_votes_received: Saturating<u64>,
    /// Number of outdated certs received.
    pub(super) num_old_certs_received: Saturating<u64>,
    /// Number of already verified certs received.
    pub(super) num_verified_certs_received: Saturating<u64>,
    /// Number of certs received that the node has already generated.
    pub(super) num_generated_certs_received: Saturating<u64>,
    /// Number of times a vote was too far in the future and discarded.
    pub(super) vote_too_far_in_future: Saturating<u64>,
    pub(super) cert_too_far_in_future: Saturating<u64>,
    pub(super) num_keep_vote_failed: Saturating<u64>,
    pub(super) vote_pool_duplicate: Saturating<u64>,
    pub(super) invalid_vote_banning_validator: Saturating<u64>,
    /// Last time the stats were reported.
    last_report: Reporting,
}

impl SigVerifierStats {
    pub(super) fn new(root_slot: Slot) -> Self {
        Self {
            vote_stats: SigVerifyVoteStats::default(),
            cert_stats: SigVerifyCertStats::default(),
            extract_filter_msgs_us: WelfordStats::default(),
            num_pkts: Saturating(0),
            discard_vote_invalid_rank: Saturating(0),
            num_malformed_pkts: Saturating(0),
            discard_vote_no_epoch_stakes: Saturating(0),
            num_old_votes_received: Saturating(0),
            num_old_certs_received: Saturating(0),
            num_verified_certs_received: Saturating(0),
            num_generated_certs_received: Saturating(0),
            vote_too_far_in_future: Saturating(0),
            cert_too_far_in_future: Saturating(0),
            verify_and_send_batch_us: WelfordStats::default(),
            invalid_vote_banning_validator: Saturating(0),
            num_keep_vote_failed: Saturating(0),
            vote_pool_duplicate: Saturating(0),
            last_report: Reporting::new(root_slot),
        }
    }

    pub(super) fn elapsed_since_last_report(&self) -> Duration {
        self.last_report.time.elapsed()
    }

    /// Reports stats if they have not been reported in some time.
    ///
    /// Also resets all stats.
    pub(super) fn maybe_report(&mut self, root_slot: Slot) {
        if let Some(elapsed) = self.last_report.should_report(root_slot) {
            let mut stats = SigVerifierStats::new(root_slot);
            std::mem::swap(self, &mut stats);
            stats.do_report(root_slot, elapsed);
        }
    }

    /// Reports stats regardless of when they were last reported.
    ///
    /// `root_slot` should be the current root slot and is reported.
    /// `elapsed` should be the time since last report and is reported.
    pub(super) fn do_report(self, root_slot: Slot, elapsed: Duration) {
        let Self {
            vote_stats,
            cert_stats,
            extract_filter_msgs_us,
            num_pkts,
            num_malformed_pkts,
            num_old_votes_received,
            num_old_certs_received,
            num_verified_certs_received,
            num_generated_certs_received,
            discard_vote_invalid_rank,
            discard_vote_no_epoch_stakes,
            verify_and_send_batch_us,
            vote_too_far_in_future,
            cert_too_far_in_future,
            invalid_vote_banning_validator,
            num_keep_vote_failed,
            vote_pool_duplicate,
            last_report: _,
        } = self;

        vote_stats.report(elapsed);
        cert_stats.report();
        datapoint_info!(
            "bls_sig_verifier_stats",
            ("root_slot", root_slot, i64),
            ("elapsed_ms", elapsed.as_millis(), i64),
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
            (
                "discard_vote_invalid_rank",
                discard_vote_invalid_rank.0,
                i64
            ),
            ("num_old_votes_received", num_old_votes_received.0, i64),
            (
                "num_verified_certs_received",
                num_verified_certs_received.0,
                i64
            ),
            (
                "num_generated_certs_received",
                num_generated_certs_received.0,
                i64
            ),
            (
                "discard_vote_no_epoch_stakes",
                discard_vote_no_epoch_stakes.0,
                i64
            ),
            ("num_malformed_pkts", num_malformed_pkts.0, i64),
            ("num_old_certs_received", num_old_certs_received.0, i64),
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
            ("vote_too_far_in_future", vote_too_far_in_future.0, i64),
            ("cert_too_far_in_future", cert_too_far_in_future.0, i64),
            (
                "invalid_vote_banning_validator",
                invalid_vote_banning_validator.0,
                i64
            ),
            ("num_keep_vote_failed", num_keep_vote_failed.0, i64),
            ("vote_pool_duplicate", vote_pool_duplicate.0, i64),
            ("num_pkts", num_pkts.0, i64),
        );
    }
}

/// Stats from sigverifying certs.
#[derive(Debug)]
pub(super) struct SigVerifyCertStats {
    /// Number of certs [`verify_and_send_certificates`] attempted to verify the signature of.
    pub(super) certs_to_sig_verify: Saturating<u64>,
    /// Number of certs [`verify_and_send_certificates`] successfully verified the signature of.
    pub(super) sig_verified_certs: Saturating<u64>,
    /// Number of certs that were verified unnecessarily because another cert of the same
    /// `CertificateType` was already verified.
    pub(super) unnecessary_certs_verified: Saturating<u64>,
    /// Number of certs skipped because another cert of the same `CertificateType` was verified in
    /// the same batch.
    pub(super) redundant_certs_skipped: Saturating<u64>,
    /// Number of times we are banning a validator.
    pub(super) banning_validator: Saturating<u64>,

    /// Number of times cert verification failed.
    pub(super) certificate_verification_failed: Saturating<u64>,

    pub(super) pool_sender: SenderStats,

    /// Stats for [`verify_and_send_certificates`].
    pub(super) fn_verify_and_send_certs_stats: WelfordStats,
}

impl SigVerifyCertStats {
    pub(super) fn merge(&mut self, other: Self) {
        let Self {
            certs_to_sig_verify,
            sig_verified_certs,
            unnecessary_certs_verified,
            redundant_certs_skipped,
            banning_validator,
            certificate_verification_failed,
            pool_sender,
            fn_verify_and_send_certs_stats,
        } = other;
        self.certs_to_sig_verify += certs_to_sig_verify;
        self.sig_verified_certs += sig_verified_certs;
        self.unnecessary_certs_verified += unnecessary_certs_verified;
        self.redundant_certs_skipped += redundant_certs_skipped;
        self.banning_validator += banning_validator;
        self.certificate_verification_failed += certificate_verification_failed;
        self.pool_sender.merge(pool_sender);
        self.fn_verify_and_send_certs_stats
            .merge(fn_verify_and_send_certs_stats);
    }

    pub(super) fn report(self) {
        let Self {
            certs_to_sig_verify,
            sig_verified_certs,
            unnecessary_certs_verified,
            redundant_certs_skipped,
            banning_validator,
            certificate_verification_failed,
            pool_sender,
            fn_verify_and_send_certs_stats,
        } = self;

        pool_sender.report();
        datapoint_info!(
            "bls_cert_sigverify_stats",
            ("certs_to_sig_verify", certs_to_sig_verify.0, i64),
            ("sig_verified_certs", sig_verified_certs.0, i64),
            (
                "unnecessary_certs_verified",
                unnecessary_certs_verified.0,
                i64
            ),
            ("redundant_certs_skipped", redundant_certs_skipped.0, i64),
            ("banning_validator", banning_validator.0, i64),
            (
                "certificate_verification_failed",
                certificate_verification_failed.0,
                i64
            ),
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

impl Default for SigVerifyCertStats {
    fn default() -> Self {
        Self {
            certs_to_sig_verify: Saturating(0),
            sig_verified_certs: Saturating(0),
            unnecessary_certs_verified: Saturating(0),
            redundant_certs_skipped: Saturating(0),
            banning_validator: Saturating(0),
            certificate_verification_failed: Saturating(0),
            pool_sender: new_cert_stats_pool_sender_stats(),
            fn_verify_and_send_certs_stats: WelfordStats::default(),
        }
    }
}

#[derive(Debug, Default)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(super) struct VoteVerificationStats {
    /// Number of times optimistic verification succeeded
    pub(super) optimistic_verification_succeeded: Saturating<u64>,
    /// Number of times optimistic verification failed
    pub(super) optimistic_verification_failed: Saturating<u64>,
    /// Stats on how many votes were in the batch when it succeeded.
    pub(super) optimistic_batch: WelfordStats,
    /// Number of votes that were individually verified.
    pub(super) num_individual_verified: Saturating<u64>,
    /// Number of times we are banning a validator.
    pub(super) banning_validator: Saturating<u64>,
    /// Stats for [`verify_votes_optimistic`].
    pub(super) fn_verify_votes_optimistic_stats: WelfordStats,
    /// Stats for [`verify_individual_votes`].
    pub(super) fn_verify_individual_votes_stats: WelfordStats,
}

impl VoteVerificationStats {
    pub(super) fn merge(&mut self, other: Self) {
        let Self {
            optimistic_verification_succeeded,
            optimistic_verification_failed,
            optimistic_batch,
            num_individual_verified,
            banning_validator,
            fn_verify_votes_optimistic_stats,
            fn_verify_individual_votes_stats,
        } = other;
        self.optimistic_verification_succeeded += optimistic_verification_succeeded;
        self.optimistic_verification_failed += optimistic_verification_failed;
        self.optimistic_batch.merge(optimistic_batch);
        self.num_individual_verified += num_individual_verified;
        self.banning_validator += banning_validator;
        self.fn_verify_votes_optimistic_stats
            .merge(fn_verify_votes_optimistic_stats);
        self.fn_verify_individual_votes_stats
            .merge(fn_verify_individual_votes_stats);
    }

    pub(super) fn report(self) {
        let Self {
            optimistic_verification_succeeded,
            optimistic_verification_failed,
            optimistic_batch,
            num_individual_verified,
            banning_validator,
            fn_verify_votes_optimistic_stats,
            fn_verify_individual_votes_stats,
        } = self;
        datapoint_info!(
            "bls_vote_sigverify_verification_stats",
            (
                "optimistic_verification_succeeded",
                optimistic_verification_succeeded.0,
                i64
            ),
            (
                "optimistic_verification_failed",
                optimistic_verification_failed.0,
                i64
            ),
            ("optimistic_batch_count", optimistic_batch.count(), i64),
            (
                "optimistic_batch_mean",
                optimistic_batch.mean().unwrap_or(0),
                i64
            ),
            (
                "optimistic_batch_max",
                optimistic_batch.maximum().unwrap_or(0),
                i64
            ),
            ("num_individual_verified", num_individual_verified.0, i64),
            ("banning_validator", banning_validator.0, i64),
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
        );
    }
}

#[derive(Debug, Default)]
#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
/// Stats from sigverifying votes.
pub(super) struct SigVerifyVoteStats {
    /// Number of votes [`verify_and_send_votes`] was requested to verify the signature of.
    pub(super) votes_to_sig_verify: Saturating<u64>,
    pub(super) senders: VoteSenderStats,
    /// Stats for [`verify_and_send_votes`].
    pub(super) fn_verify_and_send_votes_stats: WelfordStats,
    /// Stats for number of distinct votes in batches.
    pub(super) distinct_votes_stats: WelfordStats,
    pub(super) vote_verification_stats: VoteVerificationStats,
}

impl SigVerifyVoteStats {
    pub(super) fn merge(&mut self, other: Self) {
        let Self {
            votes_to_sig_verify,
            fn_verify_and_send_votes_stats,
            distinct_votes_stats,
            senders,
            vote_verification_stats,
        } = other;
        self.votes_to_sig_verify += votes_to_sig_verify;
        self.fn_verify_and_send_votes_stats
            .merge(fn_verify_and_send_votes_stats);
        self.distinct_votes_stats.merge(distinct_votes_stats);
        self.senders.merge(senders);
        self.vote_verification_stats.merge(vote_verification_stats);
    }

    pub(super) fn report(self, elapsed: Duration) {
        let Self {
            votes_to_sig_verify,
            fn_verify_and_send_votes_stats,
            distinct_votes_stats,
            senders,
            vote_verification_stats,
        } = self;
        senders.report();
        vote_verification_stats.report();
        let votes_per_sec = per_second(votes_to_sig_verify.0, elapsed);
        datapoint_info!(
            "bls_vote_sigverify_stats",
            ("votes_to_sig_verify", votes_to_sig_verify.0, i64),
            ("votes_to_sig_verify_per_sec", votes_per_sec, i64),
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
            ("distinct_votes_count", distinct_votes_stats.count(), i64),
            (
                "distinct_votes_mean",
                distinct_votes_stats.mean().unwrap_or(0),
                i64
            ),
        );
    }
}

#[derive(Debug)]
pub(super) struct VoteSenderStats {
    pub(super) metrics_sender: SenderStats,
    pub(super) rewards_sender: SenderStats,
    pub(super) pool_sender: SenderStats,
    pub(super) repair_sender: SenderStats,
}

impl VoteSenderStats {
    pub(super) fn merge(&mut self, other: Self) {
        let Self {
            metrics_sender,
            rewards_sender,
            pool_sender,
            repair_sender,
        } = other;
        self.metrics_sender.merge(metrics_sender);
        self.rewards_sender.merge(rewards_sender);
        self.pool_sender.merge(pool_sender);
        self.repair_sender.merge(repair_sender);
    }

    pub(super) fn report(self) {
        let Self {
            metrics_sender,
            rewards_sender,
            pool_sender,
            repair_sender,
        } = self;
        metrics_sender.report();
        rewards_sender.report();
        pool_sender.report();
        repair_sender.report();
    }
}

impl Default for VoteSenderStats {
    fn default() -> Self {
        Self {
            metrics_sender: SenderStats::new("bls_vote_sigverify_metrics_sender_stats"),
            rewards_sender: SenderStats::new("bls_vote_sigverify_rewards_sender_stats"),
            repair_sender: SenderStats::new("bls_vote_sigverify_repair_sender_stats"),
            pool_sender: SenderStats::new("bls_vote_sigverify_pool_sender_stats"),
        }
    }
}

fn new_cert_stats_pool_sender_stats() -> SenderStats {
    SenderStats::new("bls_cert_sigverify_pool_sender_stats")
}

#[derive(Debug)]
pub(crate) struct SenderStats {
    name: &'static str,
    pub(super) sent: Saturating<u64>,
    pub(super) channel_full: Saturating<u64>,
}

impl SenderStats {
    pub(crate) fn new(name: &'static str) -> Self {
        Self {
            name,
            sent: Saturating(0),
            channel_full: Saturating(0),
        }
    }

    pub(super) fn merge(&mut self, other: Self) {
        let Self {
            sent,
            channel_full,
            name: _,
        } = other;
        self.sent += sent;
        self.channel_full += channel_full;
    }

    pub(super) fn report(self) {
        let Self {
            sent,
            channel_full,
            name,
        } = self;
        datapoint_info!(
            name,
            ("sent", sent.0, i64),
            ("channel_full", channel_full.0, i64),
        );
    }
}

pub(crate) struct VotesVerifierStats {
    last_report: Reporting,
    pub(crate) votes_verified: Saturating<u64>,
}

impl VotesVerifierStats {
    pub(crate) fn new(root_slot: Slot) -> Self {
        Self {
            last_report: Reporting::new(root_slot),
            votes_verified: Saturating(0),
        }
    }

    pub(crate) fn maybe_report(&mut self, root_slot: Slot) {
        if self.last_report.should_report(root_slot).is_some() {
            let mut stats = Self::new(root_slot);
            std::mem::swap(self, &mut stats);
            stats.do_report();
        }
    }

    fn do_report(self) {
        let Self {
            last_report: _,
            votes_verified,
        } = self;

        datapoint_info!(
            "bls_votes_verifier_stats",
            ("votes_verified", votes_verified.0, i64),
        );
    }
}

pub(crate) struct MsgReceiverStats {
    last_report: Reporting,
    pub(crate) votes_received: Saturating<u64>,
    pub(crate) votes_too_far_in_future: Saturating<u64>,
    pub(crate) old_votes_received: Saturating<u64>,
    pub(crate) vote_invalid_rank: Saturating<u64>,
    pub(crate) duplicate_vote: Saturating<u64>,
    pub(crate) invalid_vote: Saturating<u64>,
    pub(crate) vote_no_epoch_stakes: Saturating<u64>,
    pub(crate) keep_vote_failed: Saturating<u64>,
    pub(crate) certs_received: Saturating<u64>,
    pub(crate) certs_too_far_in_future_received: Saturating<u64>,
    pub(crate) old_certs_received: Saturating<u64>,
    pub(crate) generated_certs_received: Saturating<u64>,
    pub(crate) deserialization_failed: Saturating<u64>,
    pub(crate) total_pkts: Saturating<u64>,
}

impl MsgReceiverStats {
    pub(crate) fn new(root_slot: Slot) -> Self {
        Self {
            last_report: Reporting::new(root_slot),
            votes_received: Saturating(0),
            votes_too_far_in_future: Saturating(0),
            old_votes_received: Saturating(0),
            vote_invalid_rank: Saturating(0),
            duplicate_vote: Saturating(0),
            invalid_vote: Saturating(0),
            vote_no_epoch_stakes: Saturating(0),
            keep_vote_failed: Saturating(0),
            certs_received: Saturating(0),
            certs_too_far_in_future_received: Saturating(0),
            old_certs_received: Saturating(0),
            generated_certs_received: Saturating(0),
            deserialization_failed: Saturating(0),
            total_pkts: Saturating(0),
        }
    }

    pub(crate) fn maybe_report(&mut self, root_slot: Slot) {
        if self.last_report.should_report(root_slot).is_some() {
            let mut stats = Self::new(root_slot);
            std::mem::swap(self, &mut stats);
            stats.do_report();
        }
    }

    pub(crate) fn do_report(self) {
        let Self {
            last_report: _,
            votes_received,
            votes_too_far_in_future,
            old_votes_received,
            vote_invalid_rank,
            duplicate_vote,
            invalid_vote,
            vote_no_epoch_stakes,
            keep_vote_failed,
            certs_received,
            certs_too_far_in_future_received,
            old_certs_received,
            generated_certs_received,
            deserialization_failed,
            total_pkts,
        } = self;

        datapoint_info!(
            "bls_msg_receiver_stats",
            ("votes_received", votes_received.0, i64),
            ("votes_too_far_in_future", votes_too_far_in_future.0, i64),
            ("old_votes_received", old_votes_received.0, i64),
            ("vote_invalid_rank", vote_invalid_rank.0, i64),
            ("duplicate_vote", duplicate_vote.0, i64),
            ("invalid_vote", invalid_vote.0, i64),
            ("vote_no_epoch_stakes", vote_no_epoch_stakes.0, i64),
            ("keep_vote_failed", keep_vote_failed.0, i64),
            ("certs_received", certs_received.0, i64),
            (
                "certs_to_far_in_future",
                certs_too_far_in_future_received.0,
                i64
            ),
            ("old_certs_received", old_certs_received.0, i64),
            ("generated_certs_received", generated_certs_received.0, i64),
            ("deserialization_failed", deserialization_failed.0, i64),
            ("total_pkts", total_pkts.0, i64),
        );
    }
}
