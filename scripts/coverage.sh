#!/usr/bin/env bash
#
# Run tests and collect code coverage
#
# == Usage:
#
# Run all:
#   $ ./script/coverage.sh
#
# Run for specific packages
#   $ ./script/coverage.sh -p solana-account-decoder
#   $ ./script/coverage.sh -p solana-account-decoder -p solana-accounts-db [-p ...]
#
# Custom folder name. (default: $(git rev-parse --short=9 HEAD))
#   $ COMMIT_HASH=xxx ./script/coverage.sh -p solana-account-decoder
#

set -e
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)

# Check for grcov commands
if ! command -v grcov >/dev/null 2>&1; then
  echo "Error: grcov not found.  Try |cargo install grcov|"
  exit 1
fi

# Use nightly as we have some nightly-only tests (frozen-abi)
# shellcheck source=ci/rust-version.sh
source "$here/../ci/rust-version.sh" nightly

# Check llvm path
llvm_profdata="$(find "$(rustc +"$rust_nightly" --print sysroot)" -name llvm-profdata)"
if [ -z "$llvm_profdata" ]; then
  echo "Error: couldn't find llvm-profdata. Try installing the llvm-tools component with \`rustup component add llvm-tools-preview --toolchain=$rust_nightly\`"
  exit 1
fi
llvm_path="$(dirname "$llvm_profdata")"

# get commit hash. it will be used to name output folder
if [ -z "$COMMIT_HASH" ]; then
  COMMIT_HASH=$(git rev-parse --short=9 HEAD)
fi

# Clean up
rm -rf "$here/../target/cov/$COMMIT_HASH"

# https://doc.rust-lang.org/rustc/instrument-coverage.html
export RUSTFLAGS="-C instrument-coverage $RUSTFLAGS"
export RUSTFLAGS="--cfg curve25519_dalek_backend=\"serial\" $RUSTFLAGS"
export LLVM_PROFILE_FILE="$here/../target/cov/${COMMIT_HASH}/profraw/default-%p-%m.profraw"

if [[ -z $1 ]]; then
  EXTRA_ARGS=(
    --features frozen-abi
    --lib
    --all
    --exclude solana-local-cluster
    --
    --skip shred::merkle::test::test_make_shreds_from_data::
    --skip shred::merkle::test::test_make_shreds_from_data_rand::
    --skip shred::merkle::test::test_recover_merkle_shreds::
  )
else
  EXTRA_ARGS=("$@")
fi

# Most verbose log level (trace) is enabled for all solana code to make log!
# macro code green always. Also, forcibly discard the vast amount of log by
# redirecting the stderr altogether on CI, where all tests are run unlike
# developing.
RUST_LOG="solana=trace,agave=trace,$RUST_LOG" INTERCEPT_OUTPUT=/dev/null "$here/../ci/intercept.sh" \
  cargo +"$rust_nightly" test --target-dir "$here/../target/cov" "${EXTRA_ARGS[@]}"

# Generate test reports
echo "--- grcov"
grcov_common_args=(
  "$here/../target/cov/${COMMIT_HASH}"
  --source-dir "$here/.."
  --binary-path "$here/../target/cov/debug"
  --llvm
  --llvm-path "$llvm_path"
  --ignore /\*
  --ignore \*/tests/\*
)

grcov "${grcov_common_args[@]}" -t html -o "$here/../target/cov/${COMMIT_HASH}/coverage/html"
echo "html: $here/../target/cov/${COMMIT_HASH}/coverage/html"

grcov "${grcov_common_args[@]}" -t lcov -o "$here/../target/cov/${COMMIT_HASH}/coverage/lcov.info"
echo "lcov: $here/../target/cov/${COMMIT_HASH}/coverage/lcov.info"

ln -sfT "$here/../target/cov/${COMMIT_HASH}" "$here/../target/cov/LATEST"
