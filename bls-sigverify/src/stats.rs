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

/// Max amount of seconds to wait before triggering reporting of stats.
const DURATION_INTERVAL: Duration = Duration::from_secs(1);

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
    last_report: Instant,
}

impl Default for SigVerifierStats {
    fn default() -> Self {
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
            last_report: Instant::now(),
        }
    }
}

impl SigVerifierStats {
    /// Reports stats if they have not been reported in some time.
    ///
    /// Also resets all stats.
    pub(super) fn maybe_report(&mut self, root_slot: Slot) {
        if self.last_report.elapsed() < DURATION_INTERVAL {
            return;
        }
        let mut stats = Self::default();
        std::mem::swap(&mut stats, self);
        stats.do_report(root_slot);
    }

    pub(super) fn do_report(self, root_slot: Slot) {
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
    /// Number of reward candidates beyond the per-type limit in one batch.
    pub(super) reward_candidate_cap_dropped: Saturating<u64>,
    /// Number of reward candidates dropped as copies of a retained signer set, or as exact
    /// repeats within a batch.
    pub(super) reward_candidates_filtered: Saturating<u64>,
    /// Number of inputs handed to the reward certificate verifier.
    pub(super) reward_inputs_sent: Saturating<u64>,
    /// Number of reward certificate verifier inputs dropped because it was behind.
    pub(super) reward_inputs_dropped: Saturating<u64>,
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
            reward_candidate_cap_dropped,
            reward_candidates_filtered,
            reward_inputs_sent,
            reward_inputs_dropped,
            banning_validator,
            certificate_verification_failed,
            pool_sender,
            fn_verify_and_send_certs_stats,
        } = other;
        self.certs_to_sig_verify += certs_to_sig_verify;
        self.sig_verified_certs += sig_verified_certs;
        self.unnecessary_certs_verified += unnecessary_certs_verified;
        self.redundant_certs_skipped += redundant_certs_skipped;
        self.reward_candidate_cap_dropped += reward_candidate_cap_dropped;
        self.reward_candidates_filtered += reward_candidates_filtered;
        self.reward_inputs_sent += reward_inputs_sent;
        self.reward_inputs_dropped += reward_inputs_dropped;
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
            reward_candidate_cap_dropped,
            reward_candidates_filtered,
            reward_inputs_sent,
            reward_inputs_dropped,
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
            (
                "reward_candidate_cap_dropped",
                reward_candidate_cap_dropped.0,
                i64
            ),
            (
                "reward_candidates_filtered",
                reward_candidates_filtered.0,
                i64
            ),
            ("reward_inputs_sent", reward_inputs_sent.0, i64),
            ("reward_inputs_dropped", reward_inputs_dropped.0, i64),
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
            reward_candidate_cap_dropped: Saturating(0),
            reward_candidates_filtered: Saturating(0),
            reward_inputs_sent: Saturating(0),
            reward_inputs_dropped: Saturating(0),
            banning_validator: Saturating(0),
            certificate_verification_failed: Saturating(0),
            pool_sender: new_cert_stats_pool_sender_stats(),
            fn_verify_and_send_certs_stats: WelfordStats::default(),
        }
    }
}

/// Stats for reward certificate verification, on its own thread.
pub(super) struct RewardCertVerifierStats {
    last_report: Instant,
    /// Number of consensus certificates recorded for reward coverage.
    pub(super) verified_inputs: Saturating<u64>,
    /// Number of reward candidates received.
    pub(super) candidates_received: Saturating<u64>,
    /// Number of reward candidates whose reward window closed before they were handled.
    pub(super) late_candidates: Saturating<u64>,
    /// Number of reward candidates whose signature was checked.
    pub(super) certs_to_sig_verify: Saturating<u64>,
    /// Number of reward candidates whose signature verified.
    pub(super) sig_verified_certs: Saturating<u64>,
    /// Number of reward candidates that failed verification. Their senders are banned.
    pub(super) certificate_verification_failed: Saturating<u64>,
    pub(super) candidate_cap_dropped: Saturating<u64>,
    pub(super) coverage_dropped: Saturating<u64>,
    pub(super) budget_dropped: Saturating<u64>,
    pub(super) global_budget_dropped: Saturating<u64>,
    pub(super) budget_cache_full: Saturating<u64>,
}

impl Default for RewardCertVerifierStats {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            verified_inputs: Saturating(0),
            candidates_received: Saturating(0),
            late_candidates: Saturating(0),
            certs_to_sig_verify: Saturating(0),
            sig_verified_certs: Saturating(0),
            certificate_verification_failed: Saturating(0),
            candidate_cap_dropped: Saturating(0),
            coverage_dropped: Saturating(0),
            budget_dropped: Saturating(0),
            global_budget_dropped: Saturating(0),
            budget_cache_full: Saturating(0),
        }
    }
}

impl RewardCertVerifierStats {
    pub(super) fn maybe_report(&mut self) {
        if self.last_report.elapsed() >= DURATION_INTERVAL {
            self.report();
        }
    }

    pub(super) fn report(&mut self) {
        let Self {
            last_report: _,
            verified_inputs,
            candidates_received,
            late_candidates,
            certs_to_sig_verify,
            sig_verified_certs,
            certificate_verification_failed,
            candidate_cap_dropped,
            coverage_dropped,
            budget_dropped,
            global_budget_dropped,
            budget_cache_full,
        } = std::mem::take(self);
        datapoint_info!(
            "bls_cert_sigverify_rewards_stats",
            ("verified_inputs", verified_inputs.0, i64),
            ("candidates_received", candidates_received.0, i64),
            ("late_candidates", late_candidates.0, i64),
            ("certs_to_sig_verify", certs_to_sig_verify.0, i64),
            ("sig_verified_certs", sig_verified_certs.0, i64),
            (
                "certificate_verification_failed",
                certificate_verification_failed.0,
                i64
            ),
            ("candidate_cap_dropped", candidate_cap_dropped.0, i64),
            ("coverage_dropped", coverage_dropped.0, i64),
            ("budget_dropped", budget_dropped.0, i64),
            ("global_budget_dropped", global_budget_dropped.0, i64),
            ("budget_cache_full", budget_cache_full.0, i64),
        );
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
    pub(super) num_individual_verified: Saturating<usize>,
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
    pub(super) votes_to_sig_verify: Saturating<usize>,
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

    pub(super) fn report(self) {
        let Self {
            votes_to_sig_verify,
            fn_verify_and_send_votes_stats,
            distinct_votes_stats,
            senders,
            vote_verification_stats,
        } = self;
        senders.report();
        vote_verification_stats.report();
        datapoint_info!(
            "bls_vote_sigverify_stats",
            ("votes_to_sig_verify", votes_to_sig_verify.0, i64),
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
