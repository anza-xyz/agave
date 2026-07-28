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
  features=frozen-abi
  if [[ $package == agave-votor-messages || $package == agave-votor ]]; then
    features+=,agave-unstable-api
  fi
  cmd="cargo +$rust_nightly test -p $package --features $features --lib -- test_abi_digest test_api_digest --nocapture"
  echo "--- $cmd"
  $cmd
done
