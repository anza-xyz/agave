//! The Welch-Lynch fine loop as a pure state machine.
//!
//! No I/O and no clocks: the caller feeds pulse arrivals stamped in its own
//! local timebase (nanoseconds) and drives round closes; the machine returns
//! the correction to apply. This file owns the two protocol decisions:
//!
//! - **Fault-tolerant midpoint** (stake-weighted): sort the in-window clock
//!   offset estimates, discard the lightest prefix and suffix whose stake
//!   just exceeds Σ/3, and shift by the average of the surviving extremes.
//!   Any prefix or suffix carrying more than Σ/3 of stake must contain an
//!   honest validator, so both survivors are bounded by honest estimates.
//! - **Absorption**: when the in-window quorum fails, look for a cluster of
//!   width 2S carrying more than 2Σ/3 of stake among *all* arrivals for the
//!   round (in-window or not). If one exists the cluster is fine and we are
//!   the outlier, so we snap to its midpoint without disturbing anyone.
//!
//! Arrivals are buffered raw, keyed by the sender's round tag, and evaluated
//! lazily at [`WelchLynch::close_round`]: pulses for round r+1 legitimately
//! arrive while round r is still open, and `next` for a round is only final
//! once the previous round's correction has been applied.
//!
//! A sender reports how late after its scheduled pulse time it actually
//! broadcast (`lateness_ns`), which we subtract from its offset estimate. The
//! value is clamped to `[0, W]`: a Byzantine sender lying about lateness can
//! only move its own estimate within (or out of) the window, which is exactly
//! the power it already has by timing the send itself.

use {
    solana_pubkey::Pubkey,
    std::collections::{BTreeMap, HashMap},
};

#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// T: pulse period, in ns.
    pub period_ns: i64,
    /// W: acceptance window half-width around the expected arrival, in ns.
    /// Must be strictly less than `period_ns`.
    pub window_ns: i64,
    /// S: absorption cluster half-width, in ns.
    pub absorption_half_width_ns: i64,
}

/// A buffered pulse arrival, in the local timebase.
#[derive(Debug, Clone, Copy)]
struct Arrival {
    /// When the pulse arrived, local ns.
    arrival_ns: i64,
    /// Calibrated one-way link delay to the sender, ns.
    delay_ns: i64,
    /// Sender-reported send lateness, ns (unclamped; clamped at use).
    lateness_ns: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PulseReject {
    /// Round tag below the currently open round.
    StaleRound,
    /// Round tag more than one round ahead of the currently open round.
    FarFutureRound,
    /// Already have a pulse from this peer for this round.
    DuplicatePeer,
}

/// What happened when a round closed. Stake figures are lamports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundOutcome {
    /// Fine loop: quorum of in-window estimates, fault-tolerant midpoint.
    Midpoint {
        correction_ns: i64,
        /// Spread between the surviving extremes after trimming.
        trimmed_spread_ns: i64,
        in_window: usize,
        in_window_stake: u64,
        total_stake: u64,
    },
    /// No in-window quorum, but a 2S-wide cluster with >2Σ/3 stake exists:
    /// we are the outlier and snap to it.
    Absorption {
        correction_ns: i64,
        cluster_size: usize,
        cluster_stake: u64,
        total_stake: u64,
    },
    /// Neither quorum nor cluster: free-run one period, no correction.
    /// (Phase 2 broadcasts `Panic` here.)
    NoQuorum {
        received: usize,
        in_window: usize,
        in_window_stake: u64,
        total_stake: u64,
    },
}

impl RoundOutcome {
    pub fn correction_ns(&self) -> i64 {
        match self {
            Self::Midpoint { correction_ns, .. } | Self::Absorption { correction_ns, .. } => {
                *correction_ns
            }
            Self::NoQuorum { .. } => 0,
        }
    }
}

pub struct WelchLynch {
    config: Config,
    me: Pubkey,
    /// The currently open pulse round.
    round: u64,
    /// Local time of this round's pulse (ours), ns.
    next_ns: i64,
    /// The synchronized virtual clock minus the local clock. The pulse of
    /// round k marks synchronized time k*T and fires at local time
    /// `next_k = k*T + Σ corrections`, so this is `-Σ corrections`: a
    /// positive correction means our clock runs ahead of the group (we
    /// delay our pulse), so synchronized time reads *behind* local time.
    cumulative_offset_ns: i64,
    /// Raw arrivals keyed by the sender's round tag. Only tags in
    /// `{round, round + 1}` are admitted, so this holds at most two buckets.
    buckets: BTreeMap<u64, HashMap<Pubkey, Arrival>>,
}

impl WelchLynch {
    /// `start_round` and `first_next_ns` come from the bootstrap (Phase 1:
    /// the next period boundary of the system's wall clock; Phase 2: the
    /// recovery consensus decision).
    pub fn new(config: Config, me: Pubkey, start_round: u64, first_next_ns: i64) -> Self {
        assert!(
            config.window_ns > 0 && config.window_ns < config.period_ns,
            "acceptance window must lie strictly inside the pulse period"
        );
        Self {
            config,
            me,
            round: start_round,
            next_ns: first_next_ns,
            cumulative_offset_ns: 0,
            buckets: BTreeMap::new(),
        }
    }

    pub fn round(&self) -> u64 {
        self.round
    }

    /// Local time at which this round's pulse should be broadcast.
    pub fn pulse_due_at_ns(&self) -> i64 {
        self.next_ns
    }

    /// Local time at which this round closes and the correction is computed.
    pub fn round_close_at_ns(&self) -> i64 {
        self.next_ns.saturating_add(self.config.window_ns)
    }

    pub fn cumulative_offset_ns(&self) -> i64 {
        self.cumulative_offset_ns
    }

    /// Record our own pulse: a perfect estimate of zero offset.
    pub fn record_own_pulse(&mut self) {
        self.buckets.entry(self.round).or_default().insert(
            self.me,
            Arrival {
                arrival_ns: self.next_ns,
                delay_ns: 0,
                lateness_ns: 0,
            },
        );
    }

    /// Buffer a peer's pulse. `arrival_ns` is the receive timestamp in our
    /// local timebase; `delay_ns` the calibrated one-way delay to `peer`.
    pub fn on_pulse(
        &mut self,
        peer: Pubkey,
        round: u64,
        arrival_ns: i64,
        delay_ns: i64,
        lateness_ns: i64,
    ) -> Result<(), PulseReject> {
        if round < self.round {
            return Err(PulseReject::StaleRound);
        }
        if round > self.round.saturating_add(1) {
            return Err(PulseReject::FarFutureRound);
        }
        let bucket = self.buckets.entry(round).or_default();
        if bucket.contains_key(&peer) {
            return Err(PulseReject::DuplicatePeer);
        }
        bucket.insert(
            peer,
            Arrival {
                arrival_ns,
                delay_ns,
                lateness_ns,
            },
        );
        Ok(())
    }

    /// How far off schedule the sender's pulse arrived, after removing link
    /// delay and reported send lateness. Equivalently, our clock minus the
    /// sender's: if our clock runs ahead, everyone's pulses arrive late in
    /// our timebase and the estimate is positive.
    fn offset_estimate(&self, arrival: &Arrival) -> i64 {
        let lateness = arrival.lateness_ns.clamp(0, self.config.window_ns);
        arrival
            .arrival_ns
            .saturating_sub(arrival.delay_ns)
            .saturating_sub(lateness)
            .saturating_sub(self.next_ns)
    }

    /// Close the currently open round: compute and apply the correction,
    /// advance to the next round, and drop this round's arrivals.
    ///
    /// `stakes` is the current-epoch staked set (must include ourselves);
    /// arrivals from pubkeys outside it carry no weight and are ignored.
    pub fn close_round(&mut self, stakes: &HashMap<Pubkey, u64>) -> RoundOutcome {
        let arrivals = self.buckets.remove(&self.round).unwrap_or_default();
        let total_stake: u64 = stakes
            .values()
            .fold(0u64, |acc, stake| acc.saturating_add(*stake));

        // (offset estimate, stake), sorted by offset; staked senders only.
        let mut estimates: Vec<(i64, u64)> = arrivals
            .iter()
            .filter_map(|(peer, arrival)| {
                let stake = stakes.get(peer).copied().filter(|stake| *stake > 0)?;
                Some((self.offset_estimate(arrival), stake))
            })
            .collect();
        estimates.sort_unstable();

        // The window test runs on the corrected estimate rather than the raw
        // arrival time: what must be near zero for a synchronized peer is its
        // estimated offset, after accounting for link delay and send lateness.
        let in_window: Vec<(i64, u64)> = estimates
            .iter()
            .copied()
            .filter(|(offset, _)| offset.unsigned_abs() <= self.config.window_ns as u64)
            .collect();
        let in_window_stake = stake_of(&in_window);

        let outcome = if is_quorum(in_window_stake, total_stake) {
            let (x_lo, x_hi) = trimmed_extremes(&in_window, total_stake)
                .expect("quorum stake guarantees both trimmed extremes exist");
            RoundOutcome::Midpoint {
                correction_ns: midpoint(x_lo, x_hi),
                trimmed_spread_ns: x_hi.saturating_sub(x_lo),
                in_window: in_window.len(),
                in_window_stake,
                total_stake,
            }
        } else if let Some(cluster) = find_absorption_cluster(
            &estimates,
            total_stake,
            self.config.absorption_half_width_ns,
        ) {
            RoundOutcome::Absorption {
                correction_ns: cluster.correction_ns,
                cluster_size: cluster.size,
                cluster_stake: cluster.stake,
                total_stake,
            }
        } else {
            RoundOutcome::NoQuorum {
                received: estimates.len(),
                in_window: in_window.len(),
                in_window_stake,
                total_stake,
            }
        };

        // Defensive bound: the honest-majority math already keeps the
        // midpoint within the honest estimate range, but never let a single
        // round move the clock further than the acceptance window.
        let correction = outcome.correction_ns().clamp(
            self.config.window_ns.saturating_neg(),
            self.config.window_ns,
        );

        self.next_ns = self
            .next_ns
            .saturating_add(self.config.period_ns)
            .saturating_add(correction);
        self.cumulative_offset_ns = self.cumulative_offset_ns.saturating_sub(correction);
        self.round = self.round.saturating_add(1);
        self.buckets.retain(|round, _| *round >= self.round);

        outcome
    }
}

fn stake_of(estimates: &[(i64, u64)]) -> u64 {
    estimates
        .iter()
        .fold(0u64, |acc, (_, stake)| acc.saturating_add(*stake))
}

/// Stake analog of `|est| >= n - f`: more than 2Σ/3.
fn is_quorum(stake: u64, total_stake: u64) -> bool {
    total_stake > 0 && (stake as u128).saturating_mul(3) > (total_stake as u128).saturating_mul(2)
}

/// True once `cumulative` has crossed the Σ/3 trim threshold.
fn crossed_trim_threshold(cumulative: u64, total_stake: u64) -> bool {
    (cumulative as u128).saturating_mul(3) > total_stake as u128
}

/// The surviving extremes of the stake-weighted trim: walking in from each
/// end of the sorted estimates, the first value at which the cumulative
/// stake exceeds Σ/3. `None` if an end never crosses the threshold.
fn trimmed_extremes(sorted: &[(i64, u64)], total_stake: u64) -> Option<(i64, i64)> {
    let mut cumulative = 0u64;
    let x_lo = sorted.iter().find_map(|(offset, stake)| {
        cumulative = cumulative.saturating_add(*stake);
        crossed_trim_threshold(cumulative, total_stake).then_some(*offset)
    })?;
    let mut cumulative = 0u64;
    let x_hi = sorted.iter().rev().find_map(|(offset, stake)| {
        cumulative = cumulative.saturating_add(*stake);
        crossed_trim_threshold(cumulative, total_stake).then_some(*offset)
    })?;
    Some((x_lo, x_hi))
}

fn midpoint(a: i64, b: i64) -> i64 {
    ((a as i128).saturating_add(b as i128) / 2) as i64
}

struct Cluster {
    correction_ns: i64,
    size: usize,
    stake: u64,
}

/// Slide a window of width 2S over the sorted estimates and return the
/// heaviest one carrying more than 2Σ/3 of stake, if any.
fn find_absorption_cluster(
    sorted: &[(i64, u64)],
    total_stake: u64,
    half_width_ns: i64,
) -> Option<Cluster> {
    let width = half_width_ns.saturating_mul(2);
    let mut best: Option<Cluster> = None;
    let mut hi = 0usize;
    let mut window_stake = 0u64;
    for lo in 0..sorted.len() {
        // Grow the window to cover everything within `width` of sorted[lo].
        while hi < sorted.len() && sorted[hi].0.saturating_sub(sorted[lo].0) <= width {
            window_stake = window_stake.saturating_add(sorted[hi].1);
            hi = hi.saturating_add(1);
        }
        let size = hi.saturating_sub(lo);
        if is_quorum(window_stake, total_stake)
            && best.as_ref().is_none_or(|b| window_stake > b.stake)
        {
            best = Some(Cluster {
                correction_ns: midpoint(sorted[lo].0, sorted[hi.saturating_sub(1)].0),
                size,
                stake: window_stake,
            });
        }
        window_stake = window_stake.saturating_sub(sorted[lo].1);
    }
    best
}

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

    const T: i64 = 1_000_000_000;
    const W: i64 = 500_000_000;
    const S: i64 = 50_000_000;

    fn config() -> Config {
        Config {
            period_ns: T,
            window_ns: W,
            absorption_half_width_ns: S,
        }
    }

    fn machine(me: Pubkey) -> WelchLynch {
        WelchLynch::new(config(), me, 100, T)
    }

    /// n validators with the given stakes; returns (machine, peer pubkeys,
    /// stakes map). Peer 0 is "me".
    fn cluster(stakes: &[u64]) -> (WelchLynch, Vec<Pubkey>, HashMap<Pubkey, u64>) {
        let peers: Vec<Pubkey> = stakes.iter().map(|_| Pubkey::new_unique()).collect();
        let map = peers.iter().copied().zip(stakes.iter().copied()).collect();
        (machine(peers[0]), peers, map)
    }

    /// Deliver a pulse from `peer` whose clock leads ours by `offset` ns
    /// (perfect delay calibration, zero lateness).
    fn pulse_with_offset(sm: &mut WelchLynch, peer: Pubkey, offset: i64) {
        let delay = 25_000_000;
        sm.on_pulse(
            peer,
            sm.round(),
            sm.pulse_due_at_ns() + offset + delay,
            delay,
            0,
        )
        .unwrap();
    }

    #[test]
    fn equal_stakes_reproduce_count_based_trim() {
        // n = 7, f = 2: trim the two smallest and two largest.
        let (mut sm, peers, stakes) = cluster(&[1; 7]);
        sm.record_own_pulse();
        let offsets = [
            -9_000_000, -6_000_000, -3_000_000, 3_000_000, 6_000_000, 9_000_000,
        ];
        for (peer, offset) in peers[1..].iter().zip(offsets) {
            pulse_with_offset(&mut sm, *peer, offset);
        }
        // Sorted: [-9, -6, -3, 0, 3, 6, 9] ms; trim 2 each side -> (-3 + 3)/2 = 0.
        let outcome = sm.close_round(&stakes);
        assert_eq!(outcome.correction_ns(), 0);
        assert!(matches!(
            outcome,
            RoundOutcome::Midpoint {
                trimmed_spread_ns: 6_000_000,
                in_window: 7,
                ..
            }
        ));
        assert_eq!(sm.round(), 101);
        assert_eq!(sm.pulse_due_at_ns(), 2 * T);
    }

    #[test]
    fn byzantine_whale_cannot_drag_the_midpoint() {
        // Adversary holds just under Σ/3 and reports a wild offset; the
        // correction must stay within the honest estimate range.
        let (mut sm, peers, stakes) = cluster(&[20, 20, 20, 29]);
        sm.record_own_pulse();
        pulse_with_offset(&mut sm, peers[1], 1_000_000);
        pulse_with_offset(&mut sm, peers[2], 2_000_000);
        // Whale claims to be 400ms ahead (in-window, maximally adversarial).
        pulse_with_offset(&mut sm, peers[3], 400_000_000);
        let outcome = sm.close_round(&stakes);
        let correction = outcome.correction_ns();
        assert!(
            (0..=2_000_000).contains(&correction),
            "correction {correction} outside honest range"
        );

        // Same attack pulling downward.
        let (mut sm, peers, stakes) = cluster(&[20, 20, 20, 29]);
        sm.record_own_pulse();
        pulse_with_offset(&mut sm, peers[1], 1_000_000);
        pulse_with_offset(&mut sm, peers[2], 2_000_000);
        pulse_with_offset(&mut sm, peers[3], -400_000_000);
        let correction = sm.close_round(&stakes).correction_ns();
        assert!((0..=2_000_000).contains(&correction));
    }

    #[test]
    fn absorption_snaps_outlier_to_cluster() {
        // Our clock is far behind the cluster: every honest pulse appears
        // ~600ms ahead, outside the 500ms window, but they form a tight 2S
        // cluster carrying 4/5 of stake.
        let (mut sm, peers, stakes) = cluster(&[1; 5]);
        sm.record_own_pulse();
        for (i, peer) in peers[1..].iter().enumerate() {
            pulse_with_offset(&mut sm, *peer, 600_000_000 + (i as i64) * 1_000_000);
        }
        let outcome = sm.close_round(&stakes);
        match outcome {
            RoundOutcome::Absorption {
                correction_ns,
                cluster_size,
                cluster_stake,
                total_stake,
            } => {
                // Cluster spans [600, 603]ms -> midpoint 601.5ms, clamped to W.
                assert_eq!(correction_ns, 601_500_000);
                assert_eq!(cluster_size, 4);
                assert_eq!(cluster_stake, 4);
                assert_eq!(total_stake, 5);
            }
            other => panic!("expected absorption, got {other:?}"),
        }
        // Applied correction is clamped to the window; we delay our pulse by
        // W, so the synchronized clock reads W behind our (fast) local clock.
        assert_eq!(sm.pulse_due_at_ns(), T + T + W);
        assert_eq!(sm.cumulative_offset_ns(), -W);
    }

    #[test]
    fn no_quorum_free_runs() {
        let (mut sm, peers, stakes) = cluster(&[1; 5]);
        sm.record_own_pulse();
        // One lonely peer: no quorum, and no 2S cluster with 2Σ/3 stake.
        pulse_with_offset(&mut sm, peers[1], 1_000_000);
        let outcome = sm.close_round(&stakes);
        assert!(matches!(
            outcome,
            RoundOutcome::NoQuorum {
                received: 2,
                in_window: 2,
                in_window_stake: 2,
                total_stake: 5,
            }
        ));
        assert_eq!(sm.pulse_due_at_ns(), 2 * T);
        assert_eq!(sm.cumulative_offset_ns(), 0);
    }

    #[test]
    fn round_tags_gate_buffering() {
        let (mut sm, peers, _stakes) = cluster(&[1; 4]);
        let r = sm.round();
        let t = sm.pulse_due_at_ns();
        assert_eq!(
            sm.on_pulse(peers[1], r - 1, t, 0, 0),
            Err(PulseReject::StaleRound)
        );
        assert_eq!(
            sm.on_pulse(peers[1], r + 2, t, 0, 0),
            Err(PulseReject::FarFutureRound)
        );
        assert_eq!(sm.on_pulse(peers[1], r, t, 0, 0), Ok(()));
        assert_eq!(
            sm.on_pulse(peers[1], r, t + 1, 0, 0),
            Err(PulseReject::DuplicatePeer)
        );
        // Next round's pulse is buffered now and still there after close.
        assert_eq!(sm.on_pulse(peers[2], r + 1, t + T, 0, 0), Ok(()));
        sm.record_own_pulse();
        sm.close_round(&HashMap::from([
            (peers[0], 1),
            (peers[1], 1),
            (peers[2], 1),
        ]));
        assert_eq!(
            sm.on_pulse(peers[2], r + 1, t + T, 0, 0),
            Err(PulseReject::DuplicatePeer),
            "arrival buffered for r+1 must survive the close of r"
        );
    }

    #[test]
    fn unstaked_peers_carry_no_weight() {
        let (mut sm, peers, mut stakes) = cluster(&[1, 1, 1, 0]);
        stakes.remove(&peers[2]); // absent from the staked set entirely
        sm.record_own_pulse();
        pulse_with_offset(&mut sm, peers[1], 2_000_000);
        pulse_with_offset(&mut sm, peers[2], 300_000_000); // not staked
        pulse_with_offset(&mut sm, peers[3], 300_000_000); // zero stake
        let outcome = sm.close_round(&stakes);
        // Σ = 2, quorum needs > 4/3 -> both staked pulses; trim Σ/3 from
        // each end of [0, 2ms] -> midpoint 1ms.
        assert_eq!(outcome.correction_ns(), 1_000_000);
    }

    #[test]
    fn lateness_is_subtracted_and_clamped() {
        let (mut sm, peers, stakes) = cluster(&[1, 1, 1]);
        sm.record_own_pulse();
        // Peer clock matches ours but it sent 5ms late and says so.
        let delay = 10_000_000;
        sm.on_pulse(
            peers[1],
            sm.round(),
            sm.pulse_due_at_ns() + delay + 5_000_000,
            delay,
            5_000_000,
        )
        .unwrap();
        // Peer lies with negative lateness: clamped to 0, not added.
        sm.on_pulse(
            peers[2],
            sm.round(),
            sm.pulse_due_at_ns() + delay,
            delay,
            -100_000_000,
        )
        .unwrap();
        let outcome = sm.close_round(&stakes);
        assert_eq!(outcome.correction_ns(), 0);
    }

    #[test]
    #[should_panic(expected = "acceptance window")]
    fn window_must_be_inside_period() {
        WelchLynch::new(
            Config {
                period_ns: T,
                window_ns: T,
                absorption_half_width_ns: S,
            },
            Pubkey::new_unique(),
            0,
            0,
        );
    }
}
