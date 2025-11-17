#!/usr/bin/env bash

set -euo pipefail
git_root=$(git rev-parse --show-toplevel)

# shellcheck source=ci/coverage/common.sh
source "$git_root"/ci/coverage/common.sh

exclude_packages=()
for package in "${PART_1_PACKAGES[@]}"; do
  exclude_packages+=(--exclude "$package")
done
for package in "${PART_2_PACKAGES[@]}"; do
  exclude_packages+=(--exclude "$package")
done
exclude_packages+=(--exclude solana-local-cluster)

echo "--- coverage: coverage (part 3)"
"$git_root"/ci/test-coverage.sh \
  --features frozen-abi \
  --features dev-context-only-utils \
  --workspace \
  --lib \
  "${exclude_packages[@]}"

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
