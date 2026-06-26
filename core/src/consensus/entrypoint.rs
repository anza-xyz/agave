// ============================================================
// ConsensusEntrypoint — Alpenglow / Legacy Bridge
// ============================================================
// Copyright (c) 2026 Richard Patterson
// GitHub: @De-ASI-INTERFACE | DeASI-INTERFACE · QuantumTradingInfinity · richy.ai
// SPDX-License-Identifier: Apache-2.0
//
// Routes slot finalization through Alpenglow when
// feature_set::alpenglow_consensus_enabled is active.
// Legacy TowerBFT path is fully preserved for rollback safety.
// ============================================================

use std::time::Instant;

use crate::consensus::alpenglow::{AlpenglowConfig, AlpenglowConsensus, FinalityOutcome};
use crate::consensus::votor::{VotePacket, VotorConfig};
use crate::feature_set;
use crate::metrics::consensus_alpenglow;

pub struct ConsensusEntrypoint {
    alpenglow: AlpenglowConsensus,
}

impl ConsensusEntrypoint {
    /// Construct the entrypoint, activating Alpenglow only when the
    /// feature gate is present in the current feature set.
    pub fn new(feature_set: &feature_set::FeatureSet, total_stake: u64) -> Self {
        let enabled = feature_set.is_active(
            &feature_set::alpenglow_consensus_enabled::id()
        );
        let cfg = AlpenglowConfig {
            feature_gate_enabled: enabled,
            votor: VotorConfig {
                total_network_stake: total_stake,
                ..Default::default()
            },
            ..Default::default()
        };
        Self { alpenglow: AlpenglowConsensus::new(cfg) }
    }

    /// Finalize a slot. If Alpenglow returns Pending and the feature gate
    /// is disabled, the caller must fall through to the legacy TowerBFT path.
    pub fn finalize_slot(
        &mut self,
        slot:       u64,
        votes:      &[VotePacket],
        started_at: Instant,
    ) -> FinalityOutcome {
        let outcome = self.alpenglow.process_slot(slot, votes, started_at);
        consensus_alpenglow::record(&outcome, self.alpenglow.relay_queue_depth());
        outcome
    }

    pub fn is_alpenglow_active(&self) -> bool {
        self.alpenglow.cfg.feature_gate_enabled
    }

    pub fn finalization_rate(&self) -> f64 {
        self.alpenglow.finalization_rate()
    }
}
