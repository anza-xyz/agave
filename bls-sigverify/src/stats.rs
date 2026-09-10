use {
    agave_math_utils::welford_stats::WelfordStats,
    solana_metrics::datapoint_info,
    std::{
        num::Saturating,
        time::{Duration, Instant},
    },
};

/// Max amount of seconds to wait before triggering reporting of stats.
const REPORTING_DURATION: Duration = Duration::from_secs(1);

pub(crate) struct VotesVerifierStats {
    last_report: Instant,
    pub(crate) iterations: Saturating<usize>,
    pub(crate) votes_verified: Saturating<usize>,
    pub(crate) fallback_verified: Saturating<usize>,
    pub(crate) validators_banned: Saturating<usize>,
    pub(crate) distinct_votes: WelfordStats,
    pub(crate) optimistic_batch_sizes: WelfordStats,
    pub(crate) verify_votes_us: WelfordStats,
}

impl Default for VotesVerifierStats {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            iterations: Saturating(0),
            votes_verified: Saturating(0),
            fallback_verified: Saturating(0),
            validators_banned: Saturating(0),
            distinct_votes: WelfordStats::default(),
            optimistic_batch_sizes: WelfordStats::default(),
            verify_votes_us: WelfordStats::default(),
        }
    }
}

impl VotesVerifierStats {
    pub(crate) fn maybe_report(&mut self) {
        if self.last_report.elapsed() < REPORTING_DURATION {
            return;
        }
        let Self {
            last_report: _,
            iterations,
            votes_verified,
            fallback_verified,
            validators_banned,
            distinct_votes,
            optimistic_batch_sizes,
            verify_votes_us,
        } = self;

        datapoint_info!(
            "bls_votes_verifier_stats",
            ("iterations", iterations.0, i64),
            ("votes_verified", votes_verified.0, i64),
            ("fallback_verified", fallback_verified.0, i64),
            ("validators_banned", validators_banned.0, i64),
            ("distinct_votes_count", distinct_votes.count(), i64),
            (
                "distinct_votes_mean",
                distinct_votes.mean().unwrap_or(0),
                i64
            ),
            (
                "distinct_votes_max",
                distinct_votes.maximum().unwrap_or(0),
                i64
            ),
            (
                "optimistic_batch_sizes_count",
                optimistic_batch_sizes.count(),
                i64
            ),
            (
                "optimistic_batch_sizes_mean",
                optimistic_batch_sizes.mean().unwrap_or(0),
                i64
            ),
            (
                "optimistic_batch_sizes_max",
                optimistic_batch_sizes.maximum().unwrap_or(0),
                i64
            ),
            ("verify_votes_us_count", verify_votes_us.count(), i64),
            (
                "verify_votes_us_mean",
                verify_votes_us.mean().unwrap_or(0),
                i64
            ),
            (
                "verify_votes_us_max",
                verify_votes_us.maximum().unwrap_or(0),
                i64
            ),
        );
        *self = Self::default()
    }
}

pub(crate) struct VoteProcessorStats {
    last_report: Instant,
    pub(crate) iterations: Saturating<u64>,
    pub(crate) pool_channel_succ: Saturating<u64>,
    pub(crate) pool_channel_full: Saturating<u64>,
    pub(crate) pool_channel_reopened: Saturating<u64>,
    pub(crate) repair_channel_succ: Saturating<u64>,
    pub(crate) repair_channel_drops: Saturating<u64>,
    pub(crate) rewards_channel_succ: Saturating<u64>,
    pub(crate) rewards_channel_drops: Saturating<u64>,
    pub(crate) metrics_channel_succ: Saturating<u64>,
    pub(crate) metrics_channel_drops: Saturating<u64>,
}

impl Default for VoteProcessorStats {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            iterations: Saturating(0),
            pool_channel_succ: Saturating(0),
            pool_channel_full: Saturating(0),
            pool_channel_reopened: Saturating(0),
            repair_channel_succ: Saturating(0),
            repair_channel_drops: Saturating(0),
            rewards_channel_succ: Saturating(0),
            rewards_channel_drops: Saturating(0),
            metrics_channel_succ: Saturating(0),
            metrics_channel_drops: Saturating(0),
        }
    }
}

impl VoteProcessorStats {
    pub(crate) fn maybe_report(&mut self) {
        if self.last_report.elapsed() < REPORTING_DURATION {
            return;
        }
        let Self {
            last_report: _,
            iterations,
            pool_channel_succ,
            pool_channel_full,
            pool_channel_reopened,
            repair_channel_succ,
            repair_channel_drops,
            rewards_channel_succ,
            rewards_channel_drops,
            metrics_channel_succ,
            metrics_channel_drops,
        } = self;

        datapoint_info!(
            "bls_certs_verifier_stats",
            ("iterations", iterations.0, i64),
            ("pool_channel_succ", pool_channel_succ.0, i64),
            ("pool_channel_full", pool_channel_full.0, i64),
            ("pool_channel_reopened", pool_channel_reopened.0, i64),
            ("repair_channel_succ", repair_channel_succ.0, i64),
            ("repair_channel_drops", repair_channel_drops.0, i64),
            ("rewards_channel_succ", rewards_channel_succ.0, i64),
            ("rewards_channel_drops", rewards_channel_drops.0, i64),
            ("metrics_channel_succ", metrics_channel_succ.0, i64),
            ("metrics_channel_drops", metrics_channel_drops.0, i64),
        );
        *self = Self::default()
    }
}

pub(crate) struct CertsVerifierStats {
    last_report: Instant,
    pub(crate) iterations: Saturating<u64>,
    pub(crate) pool_channel_succ: Saturating<u64>,
    pub(crate) pool_channel_full: Saturating<u64>,
    pub(crate) pool_channel_reopened: Saturating<u64>,
    pub(crate) verify_certs_us: WelfordStats,
    pub(crate) certs_verified: Saturating<usize>,
    pub(crate) validators_banned: Saturating<usize>,
}

impl Default for CertsVerifierStats {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            iterations: Saturating(0),
            pool_channel_succ: Saturating(0),
            pool_channel_full: Saturating(0),
            pool_channel_reopened: Saturating(0),
            verify_certs_us: WelfordStats::default(),
            certs_verified: Saturating(0),
            validators_banned: Saturating(0),
        }
    }
}

impl CertsVerifierStats {
    pub(crate) fn maybe_report(&mut self) {
        if self.last_report.elapsed() < REPORTING_DURATION {
            return;
        }
        let Self {
            last_report: _,
            iterations,
            pool_channel_succ,
            pool_channel_full,
            pool_channel_reopened,
            certs_verified,
            validators_banned,
            verify_certs_us,
        } = self;

        datapoint_info!(
            "bls_certs_verifier_stats",
            ("iterations", iterations.0, i64),
            ("pool_channel_succ", pool_channel_succ.0, i64),
            ("pool_channel_full", pool_channel_full.0, i64),
            ("pool_channel_reopened", pool_channel_reopened.0, i64),
            ("certs_verified", certs_verified.0, i64),
            ("validators_banned", validators_banned.0, i64),
            ("verify_certs_us_count", verify_certs_us.count(), i64),
            (
                "verify_certs_us_mean",
                verify_certs_us.mean().unwrap_or(0),
                i64
            ),
            (
                "verify_certs_us_max",
                verify_certs_us.maximum().unwrap_or(0),
                i64
            ),
        );
        *self = Self::default()
    }
}

pub(crate) struct MsgReceiverStats {
    last_report: Instant,
    pub(crate) votes_received: Saturating<u64>,
    pub(crate) votes_batches: WelfordStats,
    pub(crate) votes_too_far_in_future: Saturating<u64>,
    pub(crate) old_votes: Saturating<u64>,
    pub(crate) invalid_rank: Saturating<u64>,
    pub(crate) duplicate_vote: Saturating<u64>,
    pub(crate) invalid_vote: Saturating<u64>,
    pub(crate) no_epoch_stakes: Saturating<u64>,
    pub(crate) keep_vote_failed: Saturating<u64>,

    pub(crate) certs_received: Saturating<u64>,
    pub(crate) certs_batches: WelfordStats,
    pub(crate) certs_too_far_in_future: Saturating<u64>,
    pub(crate) old_certs: Saturating<u64>,
    pub(crate) generated_certs_received: Saturating<u64>,

    pub(crate) deserialization_failed: Saturating<u64>,
    pub(crate) total_pkts: Saturating<usize>,
}

impl Default for MsgReceiverStats {
    fn default() -> Self {
        Self {
            last_report: Instant::now(),
            votes_received: Saturating(0),
            votes_batches: WelfordStats::default(),
            votes_too_far_in_future: Saturating(0),
            old_votes: Saturating(0),
            invalid_rank: Saturating(0),
            duplicate_vote: Saturating(0),
            invalid_vote: Saturating(0),
            no_epoch_stakes: Saturating(0),
            keep_vote_failed: Saturating(0),
            certs_received: Saturating(0),
            certs_batches: WelfordStats::default(),
            certs_too_far_in_future: Saturating(0),
            old_certs: Saturating(0),
            generated_certs_received: Saturating(0),
            deserialization_failed: Saturating(0),
            total_pkts: Saturating(0),
        }
    }
}

impl MsgReceiverStats {
    pub(crate) fn maybe_report(&mut self) {
        if self.last_report.elapsed() < REPORTING_DURATION {
            return;
        }
        let Self {
            last_report: _,
            votes_received,
            votes_batches,
            votes_too_far_in_future,
            old_votes,
            invalid_rank,
            duplicate_vote,
            invalid_vote,
            no_epoch_stakes,
            keep_vote_failed,
            certs_received,
            certs_batches,
            certs_too_far_in_future,
            old_certs,
            generated_certs_received,
            deserialization_failed,
            total_pkts,
        } = self;

        datapoint_info!(
            "bls_msg_receiver_stats",
            ("votes_received", votes_received.0, i64),
            ("votes_batches_count", votes_batches.count(), i64),
            ("votes_batches_mean", votes_batches.mean().unwrap_or(0), i64),
            (
                "votes_batches_max",
                votes_batches.maximum().unwrap_or(0),
                i64
            ),
            ("votes_too_far_in_future", votes_too_far_in_future.0, i64),
            ("old_votes", old_votes.0, i64),
            ("invalid_rank", invalid_rank.0, i64),
            ("duplicate_vote", duplicate_vote.0, i64),
            ("invalid_vote", invalid_vote.0, i64),
            ("no_epoch_stakes", no_epoch_stakes.0, i64),
            ("keep_vote_failed", keep_vote_failed.0, i64),
            ("certs_received", certs_received.0, i64),
            ("certs_batches_count", certs_batches.count(), i64),
            ("certs_batches_mean", certs_batches.mean().unwrap_or(0), i64),
            (
                "certs_batches_max",
                certs_batches.maximum().unwrap_or(0),
                i64
            ),
            ("certs_to_far_in_future", certs_too_far_in_future.0, i64),
            ("old_certs", old_certs.0, i64),
            ("generated_certs_received", generated_certs_received.0, i64),
            ("deserialization_failed", deserialization_failed.0, i64),
            ("total_pkts", total_pkts.0, i64),
        );
        *self = Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Move the reporting deadline directly, so these tests never need to sleep.
    macro_rules! test_reporting {
        ($name:ident, $stats:ty, counters [$($counter:ident),+], samples [$($sample:ident),+]) => {
            #[test]
            fn $name() {
                let mut stats = <$stats>::default();
                let future = Instant::now().checked_add(Duration::from_secs(60)).unwrap();
                stats.last_report = future;
                $(stats.$counter += 7;)+
                $(stats.$sample.add_sample(10); stats.$sample.add_sample(30);)+

                stats.maybe_report();
                assert_eq!(stats.last_report, future);
                $(assert_eq!(stats.$counter.0, 7, stringify!($counter));)+
                $(
                    assert_eq!(stats.$sample.count(), 2, stringify!($sample));
                    assert_eq!(stats.$sample.mean::<u64>(), Some(20));
                    assert_eq!(stats.$sample.maximum::<u64>(), Some(30));
                )+

                stats.last_report = Instant::now().checked_sub(REPORTING_DURATION).unwrap();
                let before_report = Instant::now();
                stats.maybe_report();
                assert!(stats.last_report >= before_report);
                $(assert_eq!(stats.$counter.0, 0, stringify!($counter));)+
                $(
                    assert_eq!(stats.$sample.count(), 0, stringify!($sample));
                    assert_eq!(stats.$sample.mean::<u64>(), None);
                    assert_eq!(stats.$sample.maximum::<u64>(), None);
                )+

                // The next interval must contain only fresh measurements.
                stats.last_report = future;
                $(stats.$counter += 1;)+
                $(stats.$sample.add_sample(5);)+
                stats.maybe_report();
                $(assert_eq!(stats.$counter.0, 1, stringify!($counter));)+
                $(
                    assert_eq!(stats.$sample.count(), 1, stringify!($sample));
                    assert_eq!(stats.$sample.mean::<u64>(), Some(5));
                    assert_eq!(stats.$sample.maximum::<u64>(), Some(5));
                )+
            }
        };
    }

    test_reporting!(
        vote_metrics_reset_only_after_reporting, VotesVerifierStats,
        counters [iterations, votes_verified, fallback_verified, validators_banned],
        samples [distinct_votes, optimistic_batch_sizes, verify_votes_us]
    );
    test_reporting!(
        certificate_metrics_reset_only_after_reporting, CertsVerifierStats,
        counters [iterations, certs_verified, validators_banned],
        samples [verify_certs_us]
    );
    test_reporting!(
        receiver_metrics_reset_only_after_reporting, MsgReceiverStats,
        counters [
            votes_received, votes_too_far_in_future, old_votes, invalid_rank,
            duplicate_vote, invalid_vote, no_epoch_stakes, keep_vote_failed,
            certs_received, certs_too_far_in_future, old_certs,
            generated_certs_received, deserialization_failed, total_pkts
        ],
        samples [votes_batches, certs_batches]
    );
}
