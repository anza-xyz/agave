#!/usr/bin/env bash

set -e
git_root=$(git rev-parse --show-toplevel)

echo "--- coverage: root (part 1)"
"$git_root"/ci/test-coverage.sh \
  --features frozen-abi \
  --features dev-context-only-utils \
  --lib \
  --package solana-ledger
