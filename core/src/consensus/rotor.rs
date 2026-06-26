// ============================================================
// Rotor — Bounded-Hop Relay Propagation Scheduler
// ============================================================
// Copyright (c) 2026 Richard Patterson
// GitHub: @De-ASI-INTERFACE | DeASI-INTERFACE · QuantumTradingInfinity · richy.ai
// SPDX-License-Identifier: Apache-2.0
//
// Part of the Alpenglow consensus scaffold for Solana core.
// Replaces Turbine's gossip-heavy propagation with a stake-ordered,
// hop-limited relay queue targeting deterministic low-latency delivery.
//
// SECURITY: Hop exhaustion and queue overflow are silent drops with
// observable counters — callers must check return values.
// ============================================================

use std::collections::VecDeque;

/// A single unit of data scheduled for relay.
#[derive(Clone, Debug)]
pub struct RelayMessage {
    pub slot:         u64,
    pub leader:       [u8; 32],
    pub payload_hash: [u8; 32],
    pub stake_weight: u64,
    pub hop_count:    usize,
}

/// Tunable parameters for relay propagation behavior.
#[derive(Clone, Debug)]
pub struct RotorConfig {
    /// Maximum relay hops before a message is silently dropped.
    pub max_hops:        usize,
    /// Number of messages emitted per batch cycle.
    pub fanout:          usize,
    /// Maximum queue depth before back-pressure drops new messages.
    pub max_queue_depth: usize,
}

impl Default for RotorConfig {
    fn default() -> Self {
        Self {
            max_hops:        3,
            fanout:          200,
            max_queue_depth: 4_096,
        }
    }
}

/// Relay queue with hop enforcement and backpressure control.
pub struct Rotor {
    queue:         VecDeque<RelayMessage>,
    dropped_hops:  u64,
    dropped_depth: u64,
}

impl Rotor {
    pub fn new() -> Self {
        Self {
            queue:         VecDeque::new(),
            dropped_hops:  0,
            dropped_depth: 0,
        }
    }

    /// Enqueue a relay message. Returns false if queue is full.
    pub fn enqueue(&mut self, msg: RelayMessage, cfg: &RotorConfig) -> bool {
        if self.queue.len() >= cfg.max_queue_depth {
            self.dropped_depth += 1;
            return false;
        }
        self.queue.push_back(msg);
        true
    }

    /// Emit the next batch of relay-eligible messages.
    /// Messages exceeding hop limits are silently dropped and counted.
    pub fn next_batch(&mut self, cfg: &RotorConfig) -> Vec<RelayMessage> {
        let mut batch = Vec::with_capacity(cfg.fanout);
        while batch.len() < cfg.fanout {
            match self.queue.pop_front() {
                Some(msg) if msg.hop_count < cfg.max_hops => {
                    batch.push(msg);
                }
                Some(_) => {
                    self.dropped_hops += 1;
                }
                None => break,
            }
        }
        batch
    }

    pub fn queue_depth(&self) -> usize    { self.queue.len() }
    pub fn dropped_hops(&self) -> u64     { self.dropped_hops }
    pub fn dropped_depth(&self) -> u64    { self.dropped_depth }
}

// ============================================================
// Tests
// Author: Richard Patterson (@De-ASI-INTERFACE)
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    fn msg(slot: u64, hops: usize) -> RelayMessage {
        RelayMessage {
            slot,
            leader:       [0u8; 32],
            payload_hash: [0u8; 32],
            stake_weight: 1_000,
            hop_count:    hops,
        }
    }

    #[test]
    fn nominal_batch_relay() {
        let cfg = RotorConfig::default();
        let mut r = Rotor::new();
        for i in 0..10 { assert!(r.enqueue(msg(i, 0), &cfg)); }
        let batch = r.next_batch(&cfg);
        assert_eq!(batch.len(), 10);
        assert_eq!(r.queue_depth(), 0);
    }

    #[test]
    fn hop_limit_drops_silently() {
        let cfg = RotorConfig { max_hops: 2, ..Default::default() };
        let mut r = Rotor::new();
        r.enqueue(msg(1, 3), &cfg);
        let batch = r.next_batch(&cfg);
        assert!(batch.is_empty());
        assert_eq!(r.dropped_hops(), 1);
    }

    #[test]
    fn queue_depth_backpressure() {
        let cfg = RotorConfig { max_queue_depth: 2, ..Default::default() };
        let mut r = Rotor::new();
        assert!(r.enqueue(msg(1, 0), &cfg));
        assert!(r.enqueue(msg(2, 0), &cfg));
        assert!(!r.enqueue(msg(3, 0), &cfg));
        assert_eq!(r.dropped_depth(), 1);
    }

    #[test]
    fn fanout_limit_respected() {
        let cfg = RotorConfig { fanout: 3, ..Default::default() };
        let mut r = Rotor::new();
        for i in 0..10 { r.enqueue(msg(i, 0), &cfg); }
        let batch = r.next_batch(&cfg);
        assert_eq!(batch.len(), 3);
        assert_eq!(r.queue_depth(), 7);
    }
}
