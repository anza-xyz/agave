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

  # SBF program tests
  export SBF_OUT_DIR=target/sbf-solana-solana/release
  _ make -C programs/sbf test

  # SBF program instruction count assertion
  sbf_target_path=programs/sbf/target
  mkdir -p $sbf_target_path/deploy
  _ cargo test \
    --manifest-path programs/sbf/Cargo.toml \
    --features=sbf_c,sbf_rust assert_instruction_count \
    -- --nocapture &> $sbf_target_path/deploy/instruction_counts.txt

  sbf_dump_archive="sbf-dumps.tar.bz2"
  rm -f "$sbf_dump_archive"
  tar cjvf "$sbf_dump_archive" $sbf_target_path/{deploy/*.txt,sbf-solana-solana/release/*.so}
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
