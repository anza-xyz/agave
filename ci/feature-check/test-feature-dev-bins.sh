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

# shellcheck source=ci/rust-version.sh
source "$here"/../rust-version.sh nightly

cargo +"$rust_nightly" hack --manifest-path "$here/../../dev-bins/Cargo.toml" check
cargo +"$rust_nightly" hack --manifest-path "$here/../../dev-bins/Cargo.toml" check --all-features
