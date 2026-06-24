#!/usr/bin/env bash
set -e

cd "$(dirname "$0")/.."

# The CI container runs as root for network namespace privileges. Keep
# root-owned Cargo artifacts out of the mounted checkout.
export CARGO_TARGET_DIR=/tmp/agave-xdp-target
cargo xtask xdp-test --release-with-debug
