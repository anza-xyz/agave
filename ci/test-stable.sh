#!/usr/bin/env bash
set -eo pipefail
cd "$(dirname "$0")/.."

cargo="$(readlink -f "./cargo")"

source ci/_

annotate() {
  ${BUILDKITE:-false} && {
    buildkite-agent annotate "$@"
  }
}

# Run the appropriate test based on entrypoint
testName=$(basename "$0" .sh)

source ci/rust-version.sh stable

export RUST_BACKTRACE=1
export RUSTFLAGS="-D warnings"
source scripts/ulimit-n.sh

#shellcheck source=ci/common/limit-threads.sh
source ci/common/limit-threads.sh

# get channel info
eval "$(ci/channel-info.sh)"

#shellcheck source=ci/common/shared-functions.sh
source ci/common/shared-functions.sh

echo "Executing $testName"
case $testName in
test-stable)
  _ ci/intercept.sh cargo test --jobs "$JOBS" --all --tests --exclude solana-local-cluster ${V:+--verbose} -- --nocapture
  ;;
test-stable-sbf)
  # Clear the C dependency files, if dependency moves these files are not regenerated
  test -d target/debug/sbf && find target/debug/sbf -name '*.d' -delete
  test -d target/release/sbf && find target/release/sbf -name '*.d' -delete

  # rustfilt required for dumping SBF assembly listings
  "$cargo" install rustfilt

  # solana-keygen required when building C programs
  _ "$cargo" build --manifest-path=keygen/Cargo.toml

  export PATH="$PWD/target/debug":$PATH
  cargo_build_sbf="$(realpath ./cargo-build-sbf)"

  # platform-tools version
  "$cargo_build_sbf" --version

  # Install the platform tools
  _ platform-tools-sdk/sbf/scripts/install.sh

  # SBPFv0 program tests
  _ make -C programs/sbf test-v0

  # SBPFv1 program tests
  _ make -C programs/sbf clean-all test-v1

  # SBPFv2 program tests
  _ make -C programs/sbf clean-all test-v2

  # SBPFv3 program tests
  _ make -C programs/sbf clean-all test-v3

  exit 0
  ;;
test-docs)
  _ cargo test --jobs "$JOBS" --all --doc --exclude solana-local-cluster ${V:+--verbose} -- --nocapture
  exit 0
  ;;
*)
  echo "Error: Unknown test: $testName"
  ;;
esac

(
  export CARGO_TOOLCHAIN=+"$rust_stable"
  export RUST_LOG="solana_metrics=warn,info,$RUST_LOG"
  echo --- ci/localnet-sanity.sh
  ci/localnet-sanity.sh -x

  echo --- ci/run-sanity.sh
  ci/run-sanity.sh -x
)
