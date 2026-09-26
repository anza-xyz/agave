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

export RUSTFLAGS="-D warnings"

manifest_paths=(
	"$here/../../dev-bins/Cargo.toml"
	"$here/../../ci/xtask/Cargo.toml"
	"$here/../../programs/sbf/Cargo.toml"
)

for manifest_path in "${manifest_paths[@]}"; do
	cargo +"$rust_nightly" hack clippy \
		--manifest-path "$manifest_path" \
		--each-feature \
		--exclude-all-features \
		--all-targets
done
