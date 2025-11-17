#!/usr/bin/env bash
#
# Easily run the ABI tests for the entire repo or a subset
#

set -euo pipefail
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)

# shellcheck source=ci/rust-version.sh
source "$here/rust-version.sh" nightly

packages=$(cargo +"$rust_nightly" metadata --no-deps --format-version=1 | jq -r '.packages[] | select(.features | has("frozen-abi")) | .name')
for package in $packages; do
  cmd="cargo +$rust_nightly test -p $package --features frozen-abi --lib -- test_abi_ --nocapture"
  echo "--- $cmd"
  $cmd
done
