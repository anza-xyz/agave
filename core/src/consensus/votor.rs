// ============================================================
// Votor — Stake-Weighted Finality Decision Engine
// ============================================================
// Copyright (c) 2026 Richard Patterson
// GitHub: @De-ASI-INTERFACE | DeASI-INTERFACE · QuantumTradingInfinity · richy.ai
// SPDX-License-Identifier: Apache-2.0
//
// Part of the Alpenglow consensus scaffold for Solana core.
// Replaces TowerBFT vote-locking with a two-path stake-weighted
// certification model targeting sub-150ms finality (SIMD-0326).
//
// SECURITY: All stake arithmetic uses u128 intermediate precision
// to prevent overflow on networks with high total stake values.
// ============================================================

use std::time::{Duration, Instant};

/// A single validator vote packet carrying stake weight,
/// slot reference, and an aggregated BLS signature set.
#[derive(Clone, Debug)]
pub struct VotePacket {
    pub slot:          u64,
    pub tower_root:    Option<u64>,
    pub stake_weight:  u64,
    pub signature_set: Vec<[u8; 64]>,
    pub received_at:   Instant,
}

/// Tunable parameters for path selection and timing bounds.
#[derive(Clone, Debug)]
pub struct VotorConfig {
    /// Fast-path minimum stake in basis points (default: 6667 = 66.67%)
    pub fast_path_stake_threshold_bps: u64,
    /// Fallback-path minimum stake in basis points (default: 5000 = 50%)
    pub fallback_stake_threshold_bps: u64,
    /// Hard ceiling on elapsed time for any certification.
    pub max_finality_target: Duration,
    /// Total active stake on the network for bps calculation.
    pub total_network_stake: u64,
}

impl Default for VotorConfig {
    fn default() -> Self {
        Self {
            fast_path_stake_threshold_bps:     6_667,
            fallback_stake_threshold_bps:      5_000,
            max_finality_target:               Duration::from_millis(150),
            total_network_stake:               1_000_000,
        }
    }
}

/// The finality certification path selected by Votor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinalityPath {
    /// Super-majority stake reached within the target window.
    Fast,
    /// Simple majority stake reached within the target window.
    Fallback,
    /// Insufficient stake or timing window expired.
    Pending,
}

/// The full output of a Votor slot evaluation.
#[derive(Clone, Debug)]
pub struct VotorDecision {
    pub slot:               u64,
    pub path:               FinalityPath,
    pub certified:          bool,
    pub observed_stake_bps: u64,
    pub elapsed:            Duration,
}

/// Stateless finality evaluator. Constructed per-slot evaluation call.
pub struct Votor;

impl Votor {
    /// Evaluate a set of vote packets for a given slot.
    ///
    /// # Arguments
    /// * `votes`      — Received validator vote packets for the slot.
    /// * `cfg`        — VotorConfig with stake thresholds and timing limits.
    /// * `start`      — Instant at which the slot leader began.
    /// * `slot`       — The slot number under evaluation.
    ///
    /// # Returns
    /// A `VotorDecision` indicating Fast, Fallback, or Pending finality.
    pub fn evaluate(
        votes:  &[VotePacket],
        cfg:    &VotorConfig,
        start:  Instant,
        slot:   u64,
    ) -> VotorDecision {
        let elapsed       = start.elapsed();
        let total_voted: u64 = votes.iter().map(|v| v.stake_weight).sum();

        // u128 prevents overflow for large-stake networks
        let observed_stake_bps = if cfg.total_network_stake == 0 {
            0u64
        } else {
            ((total_voted as u128 * 10_000)
                / cfg.total_network_stake as u128)
                .min(10_000) as u64
        };

        let within_window = elapsed <= cfg.max_finality_target;

        let path = if observed_stake_bps >= cfg.fast_path_stake_threshold_bps
            && within_window
        {
            FinalityPath::Fast
        } else if observed_stake_bps >= cfg.fallback_stake_threshold_bps
            && within_window
        {
            FinalityPath::Fallback
        } else {
            FinalityPath::Pending
        };

        let certified = path != FinalityPath::Pending;

        VotorDecision { slot, path, certified, observed_stake_bps, elapsed }
    }
}

// ============================================================
// Tests
// Author: Richard Patterson (@De-ASI-INTERFACE)
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(total: u64) -> VotorConfig {
        VotorConfig { total_network_stake: total, ..Default::default() }
    }

    fn votes(weights: &[u64], slot: u64) -> Vec<VotePacket> {
        weights.iter().map(|&w| VotePacket {
            slot,
            tower_root:    None,
            stake_weight:  w,
            signature_set: vec![],
            received_at:   Instant::now(),
        }).collect()
    }

    #[test]
    fn fast_path_certified() {
        let d = Votor::evaluate(&votes(&[70_000], 1), &cfg(100_000), Instant::now(), 1);
        assert_eq!(d.path, FinalityPath::Fast);
        assert!(d.certified);
    }

    #[test]
    fn fallback_path_certified() {
        let d = Votor::evaluate(&votes(&[55_000], 1), &cfg(100_000), Instant::now(), 1);
        assert_eq!(d.path, FinalityPath::Fallback);
        assert!(d.certified);
    }

    #[test]
    fn pending_insufficient_stake() {
        let d = Votor::evaluate(&votes(&[10_000], 1), &cfg(100_000), Instant::now(), 1);
        assert_eq!(d.path, FinalityPath::Pending);
        assert!(!d.certified);
    }

    #[test]
    fn pending_window_expired() {
        let past = Instant::now() - Duration::from_millis(500);
        let d = Votor::evaluate(&votes(&[70_000], 1), &cfg(100_000), past, 1);
        assert_eq!(d.path, FinalityPath::Pending);
    }

    #[test]
    fn zero_stake_network_returns_pending() {
        let d = Votor::evaluate(&votes(&[1_000], 1), &cfg(0), Instant::now(), 1);
        assert_eq!(d.path, FinalityPath::Pending);
    }

    #[test]
    fn multiple_validators_aggregate_correctly() {
        let d = Votor::evaluate(
            &votes(&[20_000, 20_000, 20_000, 20_000], 42),
            &cfg(100_000),
            Instant::now(),
            42,
        );
        assert_eq!(d.path, FinalityPath::Fast);
    }
}
