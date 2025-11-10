#!/usr/bin/env bash

set -e
git_root=$(git rev-parse --show-toplevel)

echo "--- coverage: root (part 2)"
"$git_root"/ci/test-coverage.sh \
  --features dev-context-only-utils \
  --lib \
  --package solana-accounts-db \
  --package solana-runtime \
  --package solana-perf \
  --package solana-core \
  --package solana-wen-restart \
  --package solana-gossip
