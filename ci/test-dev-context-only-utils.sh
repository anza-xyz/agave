#!/usr/bin/env bash

set -eo pipefail
source ./ci/_

(unset RUSTC_WRAPPER; cargo install --force --git https://github.com/anza-xyz/cargo-hack.git --rev 5e59c3ec6c661c02601487c0d4b2a2649fe06c9f cargo-hack)

# Here, experimentally switch the sccache storage from GCS to local disk by
# `unset`-ing gcs credentials so that sccache automatically falls back to the
# local disk storage.
unset SCCACHE_GCS_KEY_PATH SCCACHE_GCS_BUCKET SCCACHE_GCS_RW_MODE SCCACHE_GCS_KEY_PREFIX

# sccache's default is 10G, but our boxes have far more storage. :)
export SCCACHE_CACHE_SIZE="200G"

# Disable incremental compilation as this is documented as not-compatible with
# sccache at https://github.com/mozilla/sccache/blob/v0.9.1/README.md#rust
# > Incrementally compiled crates cannot be cached.
export CARGO_INCREMENTAL=0

_ sccache --show-stats

scripts/check-dev-context-only-utils.sh check-all-targets "$@"
scripts/check-dev-context-only-utils.sh check-bins-and-lib "$@"

# This shows final stats while stopping the sccache background server as well
# for later normal sccache use (if any). Remember that sccache is now
# temporarily and experimtally running with the local disk storage.
_ sccache --stop-server
