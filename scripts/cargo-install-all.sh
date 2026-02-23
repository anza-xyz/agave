#!/usr/bin/env bash
#
# |cargo install| of the top-level crate will not install binaries for
# other workspace crates or native program crates.
here="$(dirname "$0")"
readlink_cmd="readlink"
echo "OSTYPE IS: $OSTYPE"
if [[ $OSTYPE == darwin* ]]; then
  # Mac OS X's version of `readlink` does not support the -f option,
  # But `greadlink` does, which you can get with `brew install coreutils`
  readlink_cmd="greadlink"

  if ! command -v ${readlink_cmd} &> /dev/null
  then
    echo "${readlink_cmd} could not be found. You may need to install coreutils: \`brew install coreutils\`"
    exit 1
  fi
fi

SOLANA_ROOT="$("${readlink_cmd}" -f "${here}/..")"
cargo="${SOLANA_ROOT}/cargo"

set -e

usage() {
  exitcode=0
  if [[ -n "$1" ]]; then
    exitcode=1
    echo "Error: $*"
  fi
  cat <<EOF
usage: $0 [+<cargo version>] [options] <install directory>
  +<cargo version>      Build using <cargo version> instead of the version defined in rust-toolchain.toml.

  Options:
    --debug                     Build with debug profile instead of release profile.
    --release-with-debug        Build with release-with-debug profile instead of release profile.
    --release-with-lto          Build with release-with-lto profile instead of release profile.
    --no-build-dcou-bins        Do not build DCOU binaries.
    --no-build-deprecated-bins  Do not build deprecated binaries.
    --no-build-dev-bins         Do not build development binaries.
    --no-build-end-user-bins    Do not build end user binaries.
    --no-build-platform-tools   Do not build solana-platform-tools.
    --no-build-validator-bins   Do not build validator binaries.
    --no-perf-libs              Do not fetch and install perf-libs. (Note: Not using this flag may require internet at build time)
    --no-spl-token              Do not fetch and install SPL-Token. (Note: Not using this flag requires internet at build time)
    --help                      Show this help information and exit.
EOF
  exit $exitcode
}

maybeRustVersion=
installDir=
# buildProfileArg and buildProfile duplicate some information because cargo
# doesn't allow '--profile debug' but we still need to know that the binaries
# will be in target/debug
buildProfileArg='--profile release'
buildProfile='release'

# Build selection
noBuildDCOUBins=
noBuildDeprecatedBins=
noBuildDevBins=
noBuildEndUserBins=
noBuildPlatformTools=
noBuildValidatorBins=
noPerfLibs=
noSPLToken=

while [[ -n $1 ]]; do
  if [[ ${1:0:1} = - ]]; then
    if [[ $1 = --debug ]]; then
      buildProfileArg=      # the default cargo profile is 'debug'
      buildProfile='debug'
      shift
    elif [[ $1 = --release-with-debug ]]; then
      buildProfileArg='--profile release-with-debug'
      buildProfile='release-with-debug'
      shift
    elif [[ $1 = --release-with-lto ]]; then
      buildProfileArg='--profile release-with-lto'
      buildProfile='release-with-lto'
      shift
    elif [[ $1 = --no-build-dcou-bins ]]; then
      noBuildDCOUBins=true
      shift
    elif [[ $1 = --no-build-deprecated-bins ]]; then
      noBuildDeprecatedBins=true
      shift
    elif [[ $1 = --no-build-dev-bins ]]; then
      noBuildDevBins=true
      shift
    elif [[ $1 = --no-build-end-user-bins ]]; then
      noBuildEndUserBins=true
      shift
    elif [[ $1 = --no-build-platform-tools ]]; then
      noBuildPlatformTools=true
      shift
    elif [[ $1 = --no-build-validator-bins ]]; then
      noBuildValidatorBins=true
      shift
    elif [[ $1 = --no-perf-libs ]]; then
      noPerfLibs=true
      shift
    elif [[ $1 = --no-spl-token ]]; then
      noSPLToken=true
      shift
    elif [[ $1 = --help ]]; then
      usage
    elif [[ $1 = --validator-only ]]; then
      echo "WARNING: $1 has been deprecated, use a combination of --no-build-{dev,deprecated}-bins and --no-build-platform-tools instead."
      noBuildDevBins=true
      noBuildDeprecatedBins=true
      noBuildPlatformTools=true
      shift
    else
      usage "Unknown option: $1"
    fi
  elif [[ ${1:0:1} = \+ ]]; then
    maybeRustVersion=$1
    shift
  else
    installDir=$1
    shift
  fi
done

if [[ -z "$installDir" ]]; then
  usage "Install directory not specified"
  exit 1
fi

installDir="$(mkdir -p "$installDir"; cd "$installDir"; pwd)"
mkdir -p "$installDir/bin/deps"

echo "Install location: $installDir ($buildProfile)"

cd "$(dirname "$0")"/..

SECONDS=0

source "$SOLANA_ROOT"/scripts/agave-build-lists.sh

BINS=()
DCOU_BINS=()

# Binary selection
if [[ -z "$noBuildDCOUBins" && $OSTYPE != msys ]]; then
  DCOU_BINS+=("${AGAVE_BINS_DCOU[@]}")
fi
if [[ -z "$noBuildDeprecatedBins" ]]; then
  BINS+=("${AGAVE_BINS_DEPRECATED[@]}")
fi
if [[ -z "$noBuildDevBins" ]]; then
  BINS+=("${AGAVE_BINS_DEV[@]}")
fi
if [[ -z "$noBuildEndUserBins" ]]; then
  BINS+=("${AGAVE_BINS_END_USER[@]}")
fi
if [[ -z "$noBuildValidatorBins" && $OSTYPE != msys ]]; then
  BINS+=("${AGAVE_BINS_VAL_OP[@]}")
fi

echo "Building binaries: ${BINS[*]} ${DCOU_BINS[*]}"

binArgs=()
for bin in "${BINS[@]}"; do
  binArgs+=(--bin "$bin")
done

dcouBinArgs=()
for bin in "${DCOU_BINS[@]}"; do
  dcouBinArgs+=(--bin "$bin")
done

cargo_build() {
  # shellcheck disable=SC2086 # Don't want to double quote $maybeRustVersion
  "$cargo" $maybeRustVersion build $buildProfileArg "$@"
}

# This is called to detect both of unintended activation AND deactivation of
# dcou, in order to make this rather fragile grep more resilient to bitrot...
check_dcou() {
  RUSTC_BOOTSTRAP=1 \
    cargo_build -Z unstable-options --unit-graph "$@" | \
    jq -r 'any(.units[].features[]?; . == "dev-context-only-utils")' | \
    grep -q -F "true"
}

# Some binaries (like the notable agave-ledger-tool) need to activate
# the dev-context-only-utils feature flag to build.
# Build those binaries separately to avoid the unwanted feature unification.
# Note that `--workspace --exclude <dcou tainted packages>` is needed to really
# inhibit the feature unification due to a cargo bug. Otherwise, feature
# unification happens even if cargo build is run only with `--bin` targets
# which don't depend on dcou as part of dependencies at all.
(
  set -x
  # Make sure dcou is really disabled by peeking the (unstable) build plan
  # output after turning rustc into the nightly mode with RUSTC_BOOTSTRAP=1.
  # In this way, additional requirement of nightly rustc toolchian is avoided.
  # Note that `cargo tree` can't be used, because it doesn't support `--bin`.
  if check_dcou "${binArgs[@]}" --workspace; then
     echo 'dcou feature activation is incorrectly activated!'
     exit 1
  fi

  # Build our production binaries without dcou.
  if [[ ${#binArgs[@]} -gt 0 ]]; then
    cargo_build "${binArgs[@]}" --workspace
  fi

  # Finally, build the remaining dev tools with dcou.
  if [[ ${#dcouBinArgs[@]} -gt 0 ]]; then
    if ! check_dcou --manifest-path "dev-bins/Cargo.toml" "${dcouBinArgs[@]}"; then
       echo 'dcou feature activation is incorrectly remain to be deactivated!'
       exit 1
    fi
    cargo_build --manifest-path "dev-bins/Cargo.toml" "${dcouBinArgs[@]}"
  fi

  # Exclude `spl-token` if requested
  if [[ -z "$noSPLToken" ]]; then
    # shellcheck source=scripts/spl-token-cli-version.sh
    source "$SOLANA_ROOT"/scripts/spl-token-cli-version.sh

    # shellcheck disable=SC2086
    "$cargo" $maybeRustVersion install --locked spl-token-cli --root "$installDir" $maybeSplTokenCliVersionArg
  fi
)

for bin in "${BINS[@]}"; do
  cp -fv "target/$buildProfile/$bin" "$installDir"/bin
done

for bin in "${DCOU_BINS[@]}"; do
  cp -fv "dev-bins/target/$buildProfile/$bin" "$installDir"/bin
done

if [[ -z "$noPerfLibs" && $OSTYPE != msys ]]; then
  ./fetch-perf-libs.sh

  if [[ -d target/perf-libs ]]; then
    cp -a target/perf-libs "$installDir"/bin/perf-libs
  fi
fi

if [[ -z "$noBuildPlatformTools" ]]; then
  # shellcheck disable=SC2086 # Don't want to double quote $rust_version
  "$cargo" $maybeRustVersion build --manifest-path syscalls/gen-syscall-list/Cargo.toml

  # shellcheck source=scripts/cargo-build-sbf-version.sh
  source "$SOLANA_ROOT"/scripts/cargo-build-sbf-version.sh

  # shellcheck disable=SC2086
  "$cargo" $maybeRustVersion install --locked cargo-build-sbf --root "$installDir" $maybeCargoBuildSbfVersionArg
  # shellcheck disable=SC2086
  "$cargo" $maybeRustVersion install --locked cargo-test-sbf --root "$installDir" $maybeCargoTestSbfVersionArg
fi

(
  set -x
  # deps dir can be empty
  shopt -s nullglob
  for dep in target/"$buildProfile"/deps/libsolana*program.*; do
    cp -fv "$dep" "$installDir"/bin/deps
  done
)

echo "Done after $SECONDS seconds"
echo
echo "To use these binaries:"
echo "  export PATH=\"$installDir\"/bin:\"\$PATH\""
