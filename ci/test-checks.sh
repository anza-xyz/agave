#!/usr/bin/env bash
# To prevent usange of `./cargo` without `nightly`
# Introduce cargoNighlty and disable warning to use word splitting
# shellcheck disable=SC2086

set -e

cd "$(dirname "$0")/.."

source ci/_
source ci/rust-version.sh all
eval "$(ci/channel-info.sh)"

export RUST_BACKTRACE=1
export RUSTFLAGS="-D warnings -A incomplete_features"

if ! cargo "+${rust_nightly}" sort --version > /dev/null 2>&1; then
	if [[ -n $CI ]]; then
		echo "cargo-sort not found"
		exit 1
	else
		echo "cargo-sort not found. installing..."
		cargo "+${rust_nightly}" install cargo-sort
	fi
fi
_ scripts/cargo-for-all-lock-files.sh -- "+${rust_nightly}" sort --workspace --check

_ scripts/check-dev-context-only-utils.sh tree

if ! cargo "+${rust_nightly}" fmt --version > /dev/null 2>&1; then
	if [[ -n $CI ]]; then
		echo "rustfmt not found"
		exit 1
	else
		echo "rustfmt not found. installing..."
		rustup component add rustfmt --toolchain=$RUST_NIGHTLY_VERSION
	fi
fi
_ scripts/cargo-for-all-lock-files.sh -- "+${rust_nightly}" fmt --all -- --check

# run cargo check for all rust files in this monorepo for faster turnaround in
# case of any compilation/build error for nightly

# Only force up-to-date lock files on edge
if [[ $CI_BASE_BRANCH = "$EDGE_CHANNEL" ]]; then
  if _ scripts/cargo-for-all-lock-files.sh "+${rust_nightly}" check \
    --locked --workspace --all-targets --features dummy-for-ci-check,frozen-abi; then
    true
  else
    check_status=$?
    echo "$0: Some Cargo.lock might be outdated; sync them (or just be a compilation error?)" >&2
    echo "$0: protip: $ ./scripts/cargo-for-all-lock-files.sh [--ignore-exit-code] ... \\" >&2
    echo "$0:   [tree (for outdated Cargo.lock sync)|check (for compilation error)|update -p foo --precise x.y.z (for your Cargo.toml update)] ..." >&2
    exit "$check_status"
  fi
else
  echo "Note: cargo-for-all-lock-files.sh skipped because $CI_BASE_BRANCH != $EDGE_CHANNEL"
fi

_ ci/order-crates-for-publishing.py

_ scripts/cargo-clippy.sh

if [[ -n $CI ]]; then
  _ ci/do-audit.sh
fi

if [[ -n $CI ]] && [[ $CHANNEL = "stable" ]]; then
  _ ci/check-install-all.sh
fi

echo --- ok
