# Alpenglow Rollout Strategy

**Author:** Richard Patterson (@De-ASI-INTERFACE)  
**Orgs:** DeASI-INTERFACE · QuantumTradingInfinity · richy.ai  
**SIMD:** [SIMD-0326](https://github.com/solana-foundation/solana-improvement-documents/blob/main/proposals/0326-alpenglow.md)  
**Date:** 2026-06-26  

---

## Phase 1 — Testnet (Active: May 2026)

- Feature gate OFF on mainnet, fully ON on testnet
- Full Votor + Rotor activation
- Monitor: `finality_latency_us` p50/p95, `relay_queue_depth`, `path_fast` rate
- Acceptance gate: p95 finality < 200ms sustained over 72 hours

## Phase 2 — Shadow Cluster Validation

- Activate feature gate on a shadow mainnet cluster
- Run side-by-side comparison against legacy TowerBFT path
- Acceptance gate: p95 finality < 200ms, zero consensus divergence events,
  `relay_dropped_hops` rate < 0.1%

## Phase 3 — Staged Mainnet Cohorts

- Enable feature gate for < 5% of stake, expand in 10% increments per epoch
- Acceptance gate: p95 finality < 150ms at full stake, no liveness degradation
- Final activation via governance vote

## Alert Thresholds (Production)

| Metric | Warning | Critical |
|---|---|---|
| `finality_latency_us` p95 | > 150,000 (150ms) | > 200,000 (200ms) |
| `path_pending` rate | > 10% | > 25% |
| `relay_queue_depth` | > 1,024 | > 3,072 |
| `relay_dropped_hops` | > 0.1% | > 1.0% |

## Rollback Procedure

1. Coordinate with Anza Research via the validator Discord.
2. A governance vote disables `feature_set::alpenglow_consensus_enabled`.
3. Legacy TowerBFT path resumes immediately — no state migration required.
4. No validator restart required.
5. Post-incident: capture `alpenglow_consensus` metrics snapshot for analysis.
