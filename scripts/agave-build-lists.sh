#!/usr/bin/env bash
# Defines reusable lists of Agave binary names for use across scripts.

# Source this file to access the arrays
# Example:
#   source "scripts/agave-build-lists.sh"
#   printf '%s\n' "${AGAVE_BINS_DEV[@]}"


# Groups with binary names to be built, based on their intended audience
# Keep names in sync with build/install scripts that consume these lists.

# shellcheck disable=SC2034
AGAVE_BINS_DEV=(
  cargo-build-sbf
  cargo-test-sbf
  solana-test-validator
)

AGAVE_BINS_END_USER=(
  agave-install
  solana
  solana-keygen
)

AGAVE_BINS_VAL_OP=(
  agave-validator
  agave-watchtower
  solana-gossip
  solana-genesis
  solana-faucet
)

AGAVE_BINS_DCOU=(
  agave-ledger-tool
)

# These bins are deprecated and will be removed in a future release
AGAVE_BINS_DEPRECATED=(
  solana-stake-accounts
  solana-tokens
  agave-install-init
)

DCOU_TAINTED_PACKAGES=(
  agave-ledger-tool
  agave-store-histogram
  agave-store-tool
  solana-accounts-cluster-bench
  solana-banking-bench
  solana-bench-tps
  solana-dos
  solana-local-cluster
  solana-transaction-dos
  solana-vortexor
)
