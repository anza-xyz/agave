# Release Notes

## [Unreleased] — Alpenglow Consensus Scaffold

**Author:** Richard Patterson (@De-ASI-INTERFACE)  
**Orgs:** DeASI-INTERFACE · QuantumTradingInfinity · richy.ai  
**Branch:** `feature/alpenglow-consensus-scaffold-rp`  
**Target:** `anza-xyz/agave:master`  
**SIMD:** [SIMD-0326](https://github.com/solana-foundation/solana-improvement-documents/blob/main/proposals/0326-alpenglow.md)  
**Date:** 2026-06-26  

### Added

- `core/src/consensus/votor.rs` — Stake-weighted finality engine with
  Fast (66.67%) and Fallback (50%) certification paths, 150ms target window,
  u128 overflow-safe arithmetic, 6 unit tests.
- `core/src/consensus/rotor.rs` — Bounded-hop relay scheduler with
  backpressure control, observable drop counters, 4 unit tests.
- `core/src/consensus/alpenglow.rs` — Orchestration layer wiring Votor
  and Rotor with per-instance finalization rate tracking, 4 integration tests.
- `core/src/consensus/entrypoint.rs` — Feature-gated validator bridge
  preserving legacy TowerBFT fallback path.
- `core/src/metrics/consensus_alpenglow.rs` — Grafana-ready per-slot
  telemetry via `datapoint_info!` macro.
- `core/src/config/alpenglow.rs` — Production rollout configuration
  with conservative defaults.
- `docs/consensus/alpenglow-rollout.md` — Phased rollout strategy,
  alert thresholds, and rollback procedure.

### Changed

- `core/src/consensus/mod.rs` — Module exports for new consensus path.
- `CODEOWNERS` — Alpenglow modules owned by @De-ASI-INTERFACE.
- `AUDIT_REPORT.md` — Alpenglow audit entry added.
- `SECURITY.md` — Attack surface disclosure and response SLA.

### Deferred to follow-up SIMDs

- BLS signature aggregation verification (SIMD-0337)
- Rotor governance proposal (separate SIMD)
- Shadow cluster benchmarks
