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

partition="${1:-1/1}"

exclude_features=(
	# [agave-xdp-ebpf]
	#     it needs aya-ebpf which is only available when target_arch = "bpf"
	ebpf
)

cargo +"$rust_nightly" hack check \
	--each-feature \
	--exclude-features "$(IFS=,; echo "${exclude_features[*]}")" \
	--exclude-all-features \
	--partition "$partition"
