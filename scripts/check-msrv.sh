#!/usr/bin/env bash

# Helper to make sure that rust-toolchain.toml declares the same rust version
# as the top-level workspace Cargo.toml

set -eo pipefail
cd "$(dirname "$0")/.."

exec cargo xtask check-msrv
