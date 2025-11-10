#!/usr/bin/env bash

set -o errexit

here="$(dirname "$0")"
cargo="$(readlink -f "${here}/../cargo")"

if [[ -z $cargo ]]; then
  echo >&2 "Failed to find cargo. Mac readlink doesn't support -f. Consider switching
  to gnu readlink with 'brew install coreutils' and then symlink greadlink as
  /usr/local/bin/readlink."
  exit 1
fi

# shellcheck source=ci/rust-version.sh
source "$here/../ci/rust-version.sh" nightly

# Run clippy specifically for remote-wallet package
cargo clippy \
  -p solana-remote-wallet \
  --all-targets \
  -- \
  --deny=warnings \
  --deny=clippy::default_trait_access \
  --deny=clippy::arithmetic_side_effects \
  --deny=clippy::manual_let_else \
  --deny=clippy::uninlined-format-args \
  --deny=clippy::used_underscore_binding
