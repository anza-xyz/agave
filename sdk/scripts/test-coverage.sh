#!/usr/bin/env bash

# Run tests and collect code coverage
#
# == Usage:
#
# Run all:
#   $ ./scripts/test-coverage.sh
#
# Run for specific packages
#   $ ./scripts/test-coverage.sh -p solana-pubkey
#   $ ./scripts/test-coverage.sh -p solana-pubkey -p solana-sdk-ids [-p ...]
#
# Custom folder name. (default: $(git rev-parse --short=9 HEAD))
#   $ COMMIT_HASH=xxx ./script/coverage.sh -p solana-account-decoder

set -eo pipefail
here="$(dirname "$0")"

# pacify shellcheck: cannot follow dynamic path
# shellcheck disable=SC1090,SC1091
source "$here"/rust-version.sh nightly

src_root="$(readlink -f "${here}/..")"
cd "${src_root}"

# Check for grcov commands
if ! command -v grcov >/dev/null 2>&1; then
  echo "Error: grcov not found.  Try |cargo install grcov|"
  exit 1
fi

# Check llvm path
llvm_profdata="$(find "$(./cargo nightly -Z unstable-options rustc --print sysroot)" -name llvm-profdata)"
if [ -z "$llvm_profdata" ]; then
  # pacify shellcheck: rust_nightly is referenced but not assigned
  # shellcheck disable=SC2154
  echo "Error: couldn't find llvm-profdata. Try installing the llvm-tools component with \`rustup component add llvm-tools-preview --toolchain=$rust_nightly\`"
  exit 1
fi
llvm_path="$(dirname "$llvm_profdata")"

# get commit hash. it will be used to name output folder
if [ -z "$COMMIT_HASH" ]; then
  COMMIT_HASH=$(git rev-parse --short=9 HEAD)
fi

# Clean up
rm -rf "./target/cov/$COMMIT_HASH"

# https://doc.rust-lang.org/rustc/instrument-coverage.html
export RUSTFLAGS="-C instrument-coverage $RUSTFLAGS"
export LLVM_PROFILE_FILE="./target/cov/${COMMIT_HASH}/profraw/default-%p-%m.profraw"

if [[ -z $1 ]]; then
  PACKAGES=(--lib --all)
else
  PACKAGES=("$@")
fi

# Most verbose log level (trace) is enabled for all solana code to make log!
# macro code green always. Also, forcibly discard the vast amount of log by
# redirecting the stderr.
RUST_LOG="solana=trace,$RUST_LOG" \
  ./cargo nightly test --features frozen-abi --target-dir "./target/cov" "${PACKAGES[@]}" 2>/dev/null

# Generate test reports
echo "--- grcov"
grcov_common_args=(
  "./target/cov/${COMMIT_HASH}"
  --source-dir .
  --binary-path "./target/cov/debug"
  --llvm
  --llvm-path "$llvm_path"
  --ignore \*.cargo\*
)

grcov "${grcov_common_args[@]}" -t html -o "./target/cov/${COMMIT_HASH}/coverage/html"
echo "html: ./target/cov/${COMMIT_HASH}/coverage/html"

grcov "${grcov_common_args[@]}" -t lcov -o "./target/cov/${COMMIT_HASH}/coverage/lcov.info"
echo "lcov: ./target/cov/${COMMIT_HASH}/coverage/lcov.info"

ln -sfT "./target/cov/${COMMIT_HASH}" "./target/cov/LATEST"
