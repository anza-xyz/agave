#!/usr/bin/env bash
set -e

cd "$(dirname "$0")/.."

source ci/rust-version.sh stable
source ci/rust-version.sh nightly

scripts/cargo-for-all-lock-files.sh -- "+${rust_nightly}" fmt --all -- "${@}"
