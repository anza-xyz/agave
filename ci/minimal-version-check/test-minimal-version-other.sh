#!/usr/bin/env bash

set -euox pipefail
here="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)"

if ! cargo minimal-versions --version >/dev/null 2>&1; then
	cat >&2 <<EOF
ERROR: cargo minimal-versions failed.
       install 'cargo-minimal-versions' with 'cargo install cargo-minimal-versions --locked'
EOF
	exit 1
fi

# shellcheck source=ci/rust-version.sh
source "$here"/../rust-version.sh nightly

export RUSTFLAGS="-D warnings"

manifests=(
	dev-bins/Cargo.toml
	ci/xtask/Cargo.toml
	programs/sbf/Cargo.toml
)

for manifest in "${manifests[@]}"; do
	cargo +"$rust_nightly" minimal-versions check \
		--direct \
		--manifest-path "$here/../../$manifest" \
		--workspace \
		--all-features
done
