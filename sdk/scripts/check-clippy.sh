#!/usr/bin/env bash

set -eo pipefail

here="$(dirname "$0")"
src_root="$(readlink -f "${here}/..")"

cd "${src_root}"

# Use nightly clippy, as frozen-abi proc-macro generates a lot of code across
# various crates in this whole monorepo (frozen-abi is enabled only under nightly
# due to the use of unstable rust feature). Likewise, frozen-abi(-macro) crates'
# unit tests are only compiled under nightly.
# Similarly, nightly is desired to run clippy over all of bench files because
# the bench itself isn't stabilized yet...
#   ref: https://github.com/rust-lang/rust/issues/66287
./cargo nightly clippy \
  --workspace --all-targets --features dummy-for-ci-check,frozen-abi -- \
  --deny=warnings \
  --deny=clippy::default_trait_access \
  --deny=clippy::arithmetic_side_effects \
  --deny=clippy::manual_let_else \
  --deny=clippy::used_underscore_binding
