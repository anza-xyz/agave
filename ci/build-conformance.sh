#!/usr/bin/env bash
#
# Usage: ci/build-conformance.sh <package>...
#
# Builds the harnesses of each conformance package into
# ${CARGO_TARGET_DIR:-dev-bins/target}/release.

set -euo pipefail

if [[ $# -eq 0 ]]; then
  echo "usage: $0 <package>..." >&2
  exit 1
fi

root="$(cd "$(dirname "$0")/.." && pwd)"

# Build each package separately so features don't unify across them.
for package in "$@"; do
  "$root"/cargo stable build --release --locked \
    --manifest-path "$root/dev-bins/Cargo.toml" --package "$package" --bins
done
