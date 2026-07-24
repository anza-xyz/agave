#!/usr/bin/env bash

# Helper to make sure that rust-toolchain.toml declares the same rust version
# as the top-level workspace Cargo.toml

set -eo pipefail
cd "$(dirname "$0")/.."

if ! command -v toml &>/dev/null; then
  echo "not found toml-cli"
  cargo install toml-cli
fi

rust_toolchain=$(toml get rust-toolchain.toml toolchain.channel)
rust_version=$(toml get Cargo.toml workspace.package.rust-version)

if [[ "$rust_toolchain" != "$rust_version" ]]; then
  echo "Toolchain $rust_toolchain and rust-version $rust_version are out of sync, they must be changed together"
  exit 1
fi
