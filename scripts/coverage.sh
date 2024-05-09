#!/usr/bin/env bash
#
# Run tests and collect code coverage
#
# Run all:
#   $ ./script/coverage.sh
#
# Run for specific packages
#   $ ./script/coverage.sh -p solana-account-decoder
#   $ ./script/coverage.sh -p solana-account-decoder -p solana-accounts-db [-p ...]

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

# Clean up
rm -rf "$here/../target" "$here/../coverage"
find "$here/.." -type f -name '*.prof*' -exec rm {} +
find "$here/.." -type f -name '*lcov*' -exec rm {} +

export RUSTFLAGS="-C instrument-coverage $RUSTFLAGS"
export LLVM_PROFILE_FILE="default-%p-%m.profraw"

if [[ -z $1 ]]; then
  PACKAGES=(--lib --all --exclude solana-local-cluster)
else
  PACKAGES=("$@")
fi

TEST_ARGS=(
  --skip shred::merkle::test::test_make_shreds_from_data::
  --skip shred::merkle::test::test_make_shreds_from_data_rand::
  --skip shred::merkle::test::test_recover_merkle_shreds::
)

cargo +"$rust_nightly" test "${PACKAGES[@]}" -- "${TEST_ARGS[@]}"

# Generate test reports
echo "--- grcov"
grcov_common_args=(
  .
  --binary-path "$here/../target/debug/"
  --llvm
  --ignore \*.cargo\*
  --ignore \*build.rs
  --ignore bench-tps\*
  --ignore upload-perf\*
  --ignore bench-streamer\*
  --ignore local-cluster\*
  -s .
  --branch --ignore-not-existing
)

grcov "${grcov_common_args[@]}" -t html -o "$here/../coverage/"
echo "html: $here/../coverage/"

grcov "${grcov_common_args[@]}" -t lcov -o "$here/../lcov.info"
echo "lcov: $here/../lcov.info"
