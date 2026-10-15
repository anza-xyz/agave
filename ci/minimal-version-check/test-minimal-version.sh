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

partition="${1:-1/1}"

export RUSTFLAGS="-D warnings"

# Its `ebpf` feature requires aya-ebpf, which is only available for target_arch = "bpf".
cargo +"$rust_nightly" minimal-versions check \
	--direct \
	--workspace \
	--all-features \
	--exclude agave-xdp-ebpf \
	--partition "$partition"
