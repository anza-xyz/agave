#!/usr/bin/env bash

set -e
git_root=$(git rev-parse --show-toplevel)

echo "--- coverage: coverage (part 3)"
"$git_root"/ci/test-coverage.sh \
  --features frozen-abi \
  --features dev-context-only-utils \
  --workspace \
  --lib \
  --exclude solana-ledger \
  --exclude solana-accounts-db \
  --exclude solana-runtime \
  --exclude solana-perf \
  --exclude solana-core \
  --exclude solana-wen-restart \
  --exclude solana-local-cluster \
  --exclude solana-gossip

# Clean up
cargo clean

echo "--- coverage: dev-bins"
"$git_root"/ci/test-coverage.sh \
  --features dev-context-only-utils \
  --manifest-path "$git_root"/dev-bins/Cargo.toml \
  --workspace \
  --lib

# Clean up
cargo clean

echo "--- coverage: xtask"
"$git_root"/ci/test-coverage.sh --manifest-path "$git_root"/ci/xtask/Cargo.toml
