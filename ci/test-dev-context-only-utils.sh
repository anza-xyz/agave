#!/usr/bin/env bash

set -eo pipefail

(unset RUSTC_WRAPPER; cargo install --force --git https://github.com/ryoqun/cargo-hack.git --branch interleaved-partition cargo-hack)
unset RUSTC_WRAPPER

scripts/check-dev-context-only-utils.sh check-all-targets "$@"
scripts/check-dev-context-only-utils.sh check-bins-and-lib "$@"
