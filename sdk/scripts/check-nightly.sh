#!/usr/bin/env bash

set -eo pipefail
here="$(dirname "$0")"
src_root="$(readlink -f "${here}/..")"
cd "${src_root}"

./cargo nightly check --locked --workspace --all-targets --features dummy-for-ci-check,frozen-abi
