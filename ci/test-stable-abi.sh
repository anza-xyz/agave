#!/usr/bin/env bash
#
# Easily run the ABI tests for the entire repo or a subset
#

set -euo pipefail

packages=$(cargo metadata --no-deps --format-version=1 | jq -r '.packages[] | select(.features | has("stable-abi")) | .name')
for package in $packages; do
  cmd="cargo test -p $package --features stable-abi,agave-unstable-api --lib -- test_abi_digest --nocapture"
  echo "--- $cmd"
  $cmd
done
