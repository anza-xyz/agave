#!/usr/bin/env bash

set -euo pipefail

export PART_1_PACKAGES=(
  solana-ledger
)

export PART_2_PACKAGES=(
  solana-accounts-db
  solana-runtime
  solana-perf
  solana-core
  solana-wen-restart
  solana-gossip
)
