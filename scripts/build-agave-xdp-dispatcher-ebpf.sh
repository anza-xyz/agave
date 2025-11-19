#!/usr/bin/env bash

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BEFORE_HASH=$(sha256sum "$REPO_ROOT/xdp-dispatcher-ebpf/agave-xdp-dispatcher-prog" | awk '{print $1}')
echo "Hash before rebuild: $BEFORE_HASH"

# shellcheck disable=SC1091
source "$REPO_ROOT/ci/rust-version.sh"

echo "Using nightly toolchain: $rust_nightly"

if ! command -v bpf-linker &> /dev/null; then
    echo "Installing bpf-linker..."
    cargo install bpf-linker==0.9.15
fi

rustup component add rust-src --toolchain "$rust_nightly"

RUSTFLAGS="-C debuginfo=2 -C link-arg=--btf" cargo +"$rust_nightly" rustc --manifest-path "$REPO_ROOT/xdp-dispatcher-ebpf/Cargo.toml" \
    --target bpfel-unknown-none --release --features ebpf \
    -Z build-std=core

# this is needed to strip FILE symbols which have paths that differ between rebuilds
llvm-objcopy --strip-unneeded "$REPO_ROOT/target/bpfel-unknown-none/release/agave-xdp-dispatcher-prog" "$REPO_ROOT/xdp-dispatcher-ebpf/agave-xdp-dispatcher-prog"

AFTER_HASH=$(sha256sum "$REPO_ROOT/xdp-dispatcher-ebpf/agave-xdp-dispatcher-prog" | awk '{print $1}')
echo "Hash after rebuild:  $AFTER_HASH"

if [ "$BEFORE_HASH" = "$AFTER_HASH" ]; then
    echo "✓ Hash unchanged"
else
    echo "✗ Hash changed"
fi

