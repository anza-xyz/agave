// ============================================================
// Alpenglow Rollout Configuration
// ============================================================
// Copyright (c) 2026 Richard Patterson
// GitHub: @De-ASI-INTERFACE | DeASI-INTERFACE · QuantumTradingInfinity · richy.ai
// SPDX-License-Identifier: Apache-2.0
// ============================================================

use std::time::Duration;

/// Production-safe default configuration for the Alpenglow rollout.
/// All thresholds are conservative and must be hardened through
/// testnet + shadow cluster validation before mainnet activation.
#[derive(Clone, Debug)]
pub struct AlpenglowRolloutConfig {
    /// Enable the Alpenglow feature gate. Must be false on mainnet
    /// until Phase 3 approval by validator governance.
    pub feature_gate_enabled:             bool,
    /// Target finality ceiling. Alert if p95 exceeds this value.
    pub target_finality_ceiling:          Duration,
    /// Minimum finalization rate before alerting operations team.
    pub min_finalization_rate_pct:        f64,
    /// Relay queue depth threshold for back-pressure alerting.
    pub relay_queue_depth_alert_threshold: usize,
}

impl Default for AlpenglowRolloutConfig {
    fn default() -> Self {
        Self {
            feature_gate_enabled:              false,
            target_finality_ceiling:           Duration::from_millis(200),
            min_finalization_rate_pct:         95.0,
            relay_queue_depth_alert_threshold: 1_024,
        }
    }
}
