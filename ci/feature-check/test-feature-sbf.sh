#!/usr/bin/env bash

set -euox pipefail
here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"

if ! cargo hack --version >/dev/null 2>&1; then
	cat >&2 <<EOF
ERROR: cargo hack failed.
       install 'cargo hack' with 'cargo install cargo-hack'
EOF
	exit 1
fi

export RUSTFLAGS="-D warnings"

cargo hack clippy \
	--manifest-path "$here/../../programs/sbf/Cargo.toml" \
	--workspace \
	--features agave-unstable-api \
	--ignore-unknown-features \
	--each-feature \
	--exclude-all-features \
	--all-targets
