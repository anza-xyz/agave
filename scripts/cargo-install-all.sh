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
usage: $0 [+<cargo version>] [--debug] [--validator-only] [--release-with-debug] <install directory>
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
validatorOnly=

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
    elif [[ $1 = --validator-only ]]; then
      validatorOnly=true
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
if [[ -n "$validatorOnly" ]]; then
  echo "Building binaries for net.sh deploys: ${AGAVE_BINS_END_USER[*]} ${AGAVE_BINS_VAL_OP[*]} ${AGAVE_BINS_DCOU[*]}"
  BINS+=("${AGAVE_BINS_END_USER[@]}" "${AGAVE_BINS_VAL_OP[@]}")
  DCOU_BINS+=("${AGAVE_BINS_DCOU[@]}")
else
  echo "Building binaries for all platforms: ${AGAVE_BINS_DEV[*]} ${AGAVE_BINS_END_USER[*]} ${AGAVE_BINS_DEPRECATED[*]}"
  BINS+=("${AGAVE_BINS_DEV[@]}" "${AGAVE_BINS_END_USER[@]}" "${AGAVE_BINS_DEPRECATED[@]}")

  if [[ $CI_OS_NAME != windows ]]; then
    echo "Building binaries for linux and osx only: ${AGAVE_BINS_VAL_OP[*]}, ${AGAVE_BINS_DCOU[*]}"
    BINS+=("${AGAVE_BINS_VAL_OP[@]}")
    DCOU_BINS+=("${AGAVE_BINS_DCOU[@]}")
  fi
fi

binArgs=()
for bin in "${BINS[@]}"; do
  binArgs+=(--bin "$bin")
done

dcouBinArgs=()
for bin in "${DCOU_BINS[@]}"; do
  dcouBinArgs+=(--bin "$bin")
done

excludeArgs=()
for package in "${DCOU_TAINTED_PACKAGES[@]}"; do
  excludeArgs+=(--exclude "$package")
done

cargo_build() {
  # shellcheck disable=SC2086 # Don't want to double quote $maybeRustVersion
  "$cargo" $maybeRustVersion build $buildProfileArg "$@"
}

# This is called to detect both of unintended activation AND deactivation of
# dcou, in order to make this rather fragile grep more resilient to bitrot...
check_dcou() {
  RUSTC_BOOTSTRAP=1 \
    cargo_build -Z unstable-options --build-plan "$@" | \
    grep -q -F '"feature=\"dev-context-only-utils\""'
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
  if check_dcou "${binArgs[@]}" --workspace "${excludeArgs[@]}"; then
     echo 'dcou feature activation is incorrectly activated!'
     exit 1
  fi

  # Build our production binaries without dcou.
  cargo_build "${binArgs[@]}" --workspace "${excludeArgs[@]}"

  # Finally, build the remaining dev tools with dcou.
  if [[ ${#dcouBinArgs[@]} -gt 0 ]]; then
    if ! check_dcou "${dcouBinArgs[@]}"; then
       echo 'dcou feature activation is incorrectly remain to be deactivated!'
       exit 1
    fi
    cargo_build "${dcouBinArgs[@]}"
  fi

  # Exclude `spl-token` binary for net.sh builds
  if [[ -z "$validatorOnly" ]]; then
    # shellcheck source=scripts/spl-token-cli-version.sh
    source "$SOLANA_ROOT"/scripts/spl-token-cli-version.sh

    # shellcheck disable=SC2086
    "$cargo" $maybeRustVersion install --locked spl-token-cli --root "$installDir" $maybeSplTokenCliVersionArg
  fi
)

for bin in "${BINS[@]}" "${DCOU_BINS[@]}"; do
  cp -fv "target/$buildProfile/$bin" "$installDir"/bin
done

if [[ $CI_OS_NAME != windows ]]; then
  ./fetch-perf-libs.sh

  if [[ -d target/perf-libs ]]; then
    cp -a target/perf-libs "$installDir"/bin/perf-libs
  fi
fi

if [[ -z "$validatorOnly" ]]; then
  # shellcheck disable=SC2086 # Don't want to double quote $rust_version
  "$cargo" $maybeRustVersion build --manifest-path syscalls/gen-syscall-list/Cargo.toml
  # shellcheck disable=SC2086 # Don't want to double quote $rust_version
  "$cargo" $maybeRustVersion run --bin gen-headers
  mkdir -p "$installDir"/bin/platform-tools-sdk/sbf
  cp -a platform-tools-sdk/sbf/* "$installDir"/bin/platform-tools-sdk/sbf
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
