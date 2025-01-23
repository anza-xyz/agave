#!/usr/bin/env bash

set -eo pipefail
source ./ci/_

(unset RUSTC_WRAPPER; cargo install --force --git https://github.com/ryoqun/cargo-hack.git --branch interleaved-partition cargo-hack)

unset SCCACHE_GCS_KEY_PATH SCCACHE_GCS_BUCKET SCCACHE_GCS_RW_MODE SCCACHE_GCS_KEY_PREFIX
export SCCACHE_CACHE_SIZE="200G"
_ sccache --show-stats

scripts/check-dev-context-only-utils.sh check-all-targets "$@"
scripts/check-dev-context-only-utils.sh check-bins-and-lib "$@"

_ sccache --stop-server
