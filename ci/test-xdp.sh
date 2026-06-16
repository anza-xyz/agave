#!/usr/bin/env bash
set -e

cd "$(dirname "$0")/.."

if ! command -v ip >/dev/null 2>&1 && [ ! -x /usr/sbin/ip ] && [ ! -x /sbin/ip ]; then
  apt-get update
  apt-get install --no-install-recommends -y iproute2
fi

# The CI container runs as root for network namespace privileges. Keep
# root-owned Cargo artifacts out of the mounted checkout.
export CARGO_TARGET_DIR=/tmp/agave-xdp-target
cargo xtask xdp-test --release-with-debug
