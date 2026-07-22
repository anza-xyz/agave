//! Per-round metrics for the clock-sync service.

use {
    crate::{clock::LocalNs, delay::DelayTracker, welch_lynch::RoundOutcome},
    solana_metrics::datapoint_info,
};

/// Counters accumulated while a round is open, reported and reset at close.
#[derive(Default)]
pub(crate) struct RoundStats {
    pub decode_errors: u64,
    pub stale_round: u64,
    pub far_future_round: u64,
    pub duplicate_peer: u64,
    /// How late after the scheduled pulse time we enqueued our broadcast.
    pub send_lateness_ns: i64,
    /// Datagrams that arrived while the egress channel was full.
    pub egress_full: u64,
}

impl RoundStats {
    pub(crate) fn report(
        &mut self,
        round: u64,
        outcome: &RoundOutcome,
        cumulative_offset_ns: i64,
        offset_vs_system_ns: i64,
        n_peers: usize,
        delays: &DelayTracker,
        local_now: LocalNs,
    ) {
        let (outcome_kind, correction_ns) = match outcome {
            RoundOutcome::Midpoint { correction_ns, .. } => (0i64, *correction_ns),
            RoundOutcome::Absorption { correction_ns, .. } => (1, *correction_ns),
            RoundOutcome::NoQuorum { .. } => (2, 0),
        };
        let (received, in_window, in_window_stake, total_stake, trimmed_spread_ns) = match *outcome
        {
            RoundOutcome::Midpoint {
                trimmed_spread_ns,
                in_window,
                in_window_stake,
                total_stake,
                ..
            } => (
                in_window,
                in_window,
                in_window_stake,
                total_stake,
                trimmed_spread_ns,
            ),
            RoundOutcome::Absorption {
                cluster_size,
                cluster_stake,
                total_stake,
                ..
            } => (cluster_size, 0, cluster_stake, total_stake, 0),
            RoundOutcome::NoQuorum {
                received,
                in_window,
                in_window_stake,
                total_stake,
            } => (received, in_window, in_window_stake, total_stake, 0),
        };
        let (delay_min_ns, delay_median_ns, delay_max_ns) =
            delays.stats_ns(local_now).unwrap_or((0, 0, 0));
        datapoint_info!(
            "clock_sync",
            ("round", round, i64),
            ("outcome", outcome_kind, i64),
            ("correction_ns", correction_ns, i64),
            ("trimmed_spread_ns", trimmed_spread_ns, i64),
            ("cumulative_offset_ns", cumulative_offset_ns, i64),
            ("offset_vs_system_ns", offset_vs_system_ns, i64),
            ("n_peers", n_peers, i64),
            ("received", received, i64),
            ("in_window", in_window, i64),
            ("in_window_stake", in_window_stake, i64),
            ("total_stake", total_stake, i64),
            ("send_lateness_ns", self.send_lateness_ns, i64),
            ("delay_min_ns", delay_min_ns, i64),
            ("delay_median_ns", delay_median_ns, i64),
            ("delay_max_ns", delay_max_ns, i64),
            ("decode_errors", self.decode_errors, i64),
            ("stale_round", self.stale_round, i64),
            ("far_future_round", self.far_future_round, i64),
            ("duplicate_peer", self.duplicate_peer, i64),
            ("egress_full", self.egress_full, i64),
        );
        *self = Self::default();
    }
}
