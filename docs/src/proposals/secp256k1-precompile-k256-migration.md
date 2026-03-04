# secp256k1 precompile: migrate recovery backend from libsecp256k1 to k256

## Summary

This proposal migrates secp256k1 precompile public key recovery from
`libsecp256k1` to `k256`, while preserving runtime behavior by default and
activating the new backend behind a feature gate.

## Motivation

- `libsecp256k1` is no longer the preferred maintenance path for Solana
  ecosystem crates.
- `k256` is actively maintained and already used by
  `solana-secp256k1-recover`.
- Aligning the precompile backend with `k256` reduces maintenance risk and
  keeps cryptographic dependencies consistent.

## Behavior Compatibility Analysis

The secp256k1 precompile currently performs:

1. Parse 64-byte compact signature.
2. Parse recovery id from the byte after the signature.
3. Keccak hash of message bytes.
4. Recover uncompressed secp256k1 public key.
5. Compare recovered Ethereum address with expected address.

The `k256` migration path uses `solana-secp256k1-recover::secp256k1_recover`,
which already maps to `k256` off-chain and preserves Solana-compatible
semantics.

Compatibility expectations:

- Recovery IDs:
  - `0..=3`: accepted for parsing.
  - `>=4`: rejected as `InvalidRecoveryId`.
- Signature format:
  - Invalid/overflowing signatures: rejected as `InvalidSignature`.
- Malleability:
  - `libsecp256k1` accepts high-S signatures.
  - `k256` (via `solana-secp256k1-recover`) rejects high-S signatures.
  - This is the primary externally visible behavior difference.
- Message hash:
  - Precompile always hashes message data to 32-byte Keccak; no behavior change.

Backend errors continue to map to existing precompile error variants
(`InvalidSignature` or `InvalidRecoveryId`).

## Proposal

1. Add a new runtime feature gate:
   - `secp256k1_precompile_use_k256`
2. Keep `libsecp256k1` as the default precompile backend before activation.
3. Switch to `k256` recovery when the feature gate is active.
4. Validate parity with unit tests for:
   - valid signatures,
   - high-S malleable signatures (expected divergence),
   - invalid recovery IDs.

## Rollout Plan

1. Merge implementation with feature gate disabled by default.
2. Run additional cluster-level soak testing with gate enabled.
3. Activate the feature gate on target clusters via standard feature rollout.
4. After full activation and observation, remove legacy backend path in a
   follow-up cleanup.

## Risks

- Subtle edge-case differences in cryptographic parsing/recovery.
- Potential consensus impact if behavior diverges for malformed signatures.

Mitigations:

- Feature-gated activation.
- Parity-focused tests against legacy backend behavior.
- Staged rollout with observation before broad activation.
