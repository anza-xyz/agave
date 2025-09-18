#!/usr/bin/env bash
#
# Easily run the ABI tests for the entire repo or a subset
#

here="$(dirname "$0")"
cargo="$(readlink -f "${here}/../cargo")"

set -x
exec "$cargo" nightly test --features frozen-abi --lib -- test_abi_ --nocapture
