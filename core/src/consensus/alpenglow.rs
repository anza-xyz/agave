// ============================================================
// AlpenglowConsensus — Orchestration Layer
// ============================================================
// Copyright (c) 2026 Richard Patterson
// GitHub: @De-ASI-INTERFACE | DeASI-INTERFACE · QuantumTradingInfinity · richy.ai
// SPDX-License-Identifier: Apache-2.0
//
// Top-level control plane for the Alpenglow consensus upgrade.
// Coordinates Votor finality evaluation and Rotor relay scheduling.
// When feature_gate_enabled is false, all calls are transparent
// no-ops and the legacy TowerBFT path remains in full control.
// ============================================================

use std::time::{Duration, Instant};

use crate::consensus::rotor::{RelayMessage, Rotor, RotorConfig};
use crate::consensus::votor::{FinalityPath, VotePacket, Votor, VotorConfig};

#[derive(Clone, Debug)]
pub struct AlpenglowConfig {
    pub votor:                VotorConfig,
    pub rotor:                RotorConfig,
    /// Must be explicitly set to true. Defaults to false for safety.
    pub feature_gate_enabled: bool,
    pub target_finality:      Duration,
}

impl Default for AlpenglowConfig {
    fn default() -> Self {
        Self {
            votor:                VotorConfig::default(),
            rotor:                RotorConfig::default(),
            feature_gate_enabled: false,
            target_finality:      Duration::from_millis(150),
        }
    }
}

/// Canonical output from AlpenglowConsensus slot processing.
#[derive(Clone, Debug)]
pub struct FinalityOutcome {
    pub slot:             u64,
    pub finalized:        bool,
    pub path:             FinalityPath,
    pub finality_latency: Duration,
}

/// Main Alpenglow controller wiring Votor + Rotor together.
pub struct AlpenglowConsensus {
    pub cfg:             AlpenglowConfig,
    rotor:               Rotor,
    slots_processed:     u64,
    slots_finalized:     u64,
}

impl AlpenglowConsensus {
    pub fn new(cfg: AlpenglowConfig) -> Self {
        Self {
            rotor:           Rotor::new(),
            cfg,
            slots_processed: 0,
            slots_finalized: 0,
        }
    }

    /// Ingest an incoming relay message. No-op when feature gate is off.
    pub fn ingest_relay(&mut self, relay: RelayMessage) {
        if self.cfg.feature_gate_enabled {
            self.rotor.enqueue(relay, &self.cfg.rotor);
        }
    }

    /// Drain the next relay batch for network forwarding.
    pub fn drain_relay_batch(&mut self) -> Vec<RelayMessage> {
        if self.cfg.feature_gate_enabled {
            self.rotor.next_batch(&self.cfg.rotor)
        } else {
            vec![]
        }
    }

    /// Process a slot through the Alpenglow finality path.
    /// Returns Pending without touching state if feature gate is off.
    pub fn process_slot(
        &mut self,
        slot:       u64,
        votes:      &[VotePacket],
        started_at: Instant,
    ) -> FinalityOutcome {
        self.slots_processed += 1;

        if !self.cfg.feature_gate_enabled {
            return FinalityOutcome {
                slot,
                finalized:        false,
                path:             FinalityPath::Pending,
                finality_latency: started_at.elapsed(),
            };
        }

        let decision   = Votor::evaluate(votes, &self.cfg.votor, started_at, slot);
        let finalized  = decision.certified
            && decision.elapsed <= self.cfg.target_finality
            && matches!(decision.path, FinalityPath::Fast | FinalityPath::Fallback);

        if finalized { self.slots_finalized += 1; }

        FinalityOutcome {
            slot,
            finalized,
            path:             decision.path,
            finality_latency: decision.elapsed,
        }
    }

    /// Observability: finalization rate since controller creation.
    pub fn finalization_rate(&self) -> f64 {
        if self.slots_processed == 0 { return 0.0; }
        self.slots_finalized as f64 / self.slots_processed as f64
    }

    pub fn relay_queue_depth(&self) -> usize { self.rotor.queue_depth() }
    pub fn relay_dropped_hops(&self) -> u64  { self.rotor.dropped_hops() }
}

// ============================================================
// Tests
// Author: Richard Patterson (@De-ASI-INTERFACE)
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> AlpenglowConfig {
        AlpenglowConfig {
            feature_gate_enabled: true,
            votor: VotorConfig { total_network_stake: 100_000, ..Default::default() },
            ..Default::default()
        }
    }

    fn votes(w: u64) -> Vec<VotePacket> {
        vec![VotePacket {
            slot: 1, tower_root: None,
            stake_weight:  w,
            signature_set: vec![],
            received_at:   Instant::now(),
        }]
    }

    #[test]
    fn feature_off_always_pending() {
        let mut ag = AlpenglowConsensus::new(AlpenglowConfig::default());
        let out = ag.process_slot(1, &votes(90_000), Instant::now());
        assert!(!out.finalized);
        assert_eq!(out.path, FinalityPath::Pending);
    }

    #[test]
    fn feature_on_fast_finality() {
        let mut ag = AlpenglowConsensus::new(enabled());
        let out = ag.process_slot(1, &votes(70_000), Instant::now());
        assert!(out.finalized);
        assert_eq!(out.path, FinalityPath::Fast);
    }

    #[test]
    fn finalization_rate_tracks_correctly() {
        let mut ag = AlpenglowConsensus::new(enabled());
        ag.process_slot(1, &votes(70_000), Instant::now());
        ag.process_slot(2, &votes(5_000),  Instant::now());
        let rate = ag.finalization_rate();
        assert!(rate > 0.49 && rate < 0.51);
    }

    #[test]
    fn relay_ingestion_and_drain_roundtrip() {
        let mut ag = AlpenglowConsensus::new(enabled());
        ag.ingest_relay(RelayMessage {
            slot: 1, leader: [0u8; 32],
            payload_hash: [0u8; 32],
            stake_weight: 1_000, hop_count: 0,
        });
        assert_eq!(ag.relay_queue_depth(), 1);
        let batch = ag.drain_relay_batch();
        assert_eq!(batch.len(), 1);
        assert_eq!(ag.relay_queue_depth(), 0);
    }
}
