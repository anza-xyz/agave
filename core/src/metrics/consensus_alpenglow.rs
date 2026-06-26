// ============================================================
// Alpenglow Consensus Metrics
// ============================================================
// Copyright (c) 2026 Richard Patterson
// GitHub: @De-ASI-INTERFACE | DeASI-INTERFACE · QuantumTradingInfinity · richy.ai
// SPDX-License-Identifier: Apache-2.0
//
// Emits per-slot datapoints for finality latency, path selection,
// relay queue depth, and certification outcomes.
// Feeds into Grafana dashboards via the standard Solana metrics bus.
// ============================================================

use crate::consensus::alpenglow::FinalityOutcome;
use crate::consensus::votor::FinalityPath;

/// Record a FinalityOutcome and relay queue depth to the metrics bus.
pub fn record(outcome: &FinalityOutcome, relay_queue_depth: usize) {
    datapoint_info!(
        "alpenglow_consensus",
        // Identification
        ("slot",                outcome.slot as i64,                         i64),
        // Outcome
        ("finalized",           outcome.finalized as i64,                    i64),
        ("finality_latency_us", outcome.finality_latency.as_micros() as i64, i64),
        // Path counters
        ("path_fast",     (outcome.path == FinalityPath::Fast)     as i64,   i64),
        ("path_fallback", (outcome.path == FinalityPath::Fallback) as i64,   i64),
        ("path_pending",  (outcome.path == FinalityPath::Pending)  as i64,   i64),
        // Relay health
        ("relay_queue_depth", relay_queue_depth as i64,                      i64),
    );
}
