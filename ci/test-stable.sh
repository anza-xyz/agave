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

cargo_build_sbf_sanity() {
  cargo_build_sbf="$(realpath ./cargo-build-sbf)"

  pushd programs/sbf
  # Generate the sanity programs list
  if [ ! -f target/sanity_programs.txt ]; then
    cargo test --features="sbf_rust,sbf_sanity_list" --test programs test_program_sbf_sanity
  fi
  mapfile -t rust_programs < <(cat target/sanity_programs.txt)

  pushd rust
  # This is done in a loop to mock how developers invoke `cargo-build-sbf`
  for program in "${rust_programs[@]}"
  do
    pushd "$program"
    $cargo_build_sbf --arch "$1"
    popd
  done
  popd

  SBF_OUT_DIR=target/deploy cargo test --features=sbf_rust --test programs test_program_sbf_sanity
  popd
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
  _ make -C programs/sbf clean-all
  _ cargo_build_sbf_sanity "v0"

  # SBPFv1 program tests
  _ make -C programs/sbf clean-all test-v1
  _ make -C programs/sbf clean-all
  _ cargo_build_sbf_sanity "v1"

  # SBPFv2 program tests
  _ make -C programs/sbf clean-all test-v2
  _ make -C programs/sbf clean-all
  _ cargo_build_sbf_sanity "v2"

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
