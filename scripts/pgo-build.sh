#!/usr/bin/env bash
#
# Profile-guided optimization (PGO): a two-pass release build with a training
# run in between.
#
#   scripts/pgo-build.sh instrument --bin agave-validator   # pass 1
#   <run the instrumented binary on a workload that resembles yours, stop it>
#   scripts/pgo-build.sh build --bin agave-validator        # pass 2
#
# Any arguments after the subcommand are passed to `cargo build --release`
# (e.g. `-p solana-poh-bench`, `--bin agave-validator`). The two passes are
# separate commands because the training run is yours to perform: a profile
# only helps if it was recorded on representative traffic, so there is no
# useful profile to ship, and one trained on the wrong workload can be slower
# than none.
#
# Environment:
#   PGO_PROFILE_DIR         where .profraw files and the merged profile go
#                           (default: target/pgo)
#   PGO_TARGET              target triple (default: the host's)
#   PGO_INSTRUMENT_LDFLAGS  extra RUSTFLAGS for pass 1 only (default: use lld on
#                           aarch64-unknown-linux-*, see below)
#   RUSTFLAGS               kept and extended in both passes; must be identical
#                           between them apart from the profile flags
set -euo pipefail

usage() {
  echo "usage: $0 {instrument|build} [cargo build args...]" >&2
  exit 1
}
[[ $# -ge 1 ]] || usage
cmd=$1
shift

cd "$(dirname "$0")/.."

PGO_PROFILE_DIR=${PGO_PROFILE_DIR:-target/pgo}
mkdir -p "$PGO_PROFILE_DIR"
profile_dir=$(cd "$PGO_PROFILE_DIR" && pwd)
profdata="$profile_dir/merged.profdata"

# An explicit --target is required, not cosmetic: without it RUSTFLAGS also
# applies to host artifacts, so cargo builds instrumented build scripts and proc
# macros and then runs them during the build. The binaries therefore land in
# target/<triple>/release/, not target/release/.
PGO_TARGET=${PGO_TARGET:-$(rustc -vV | awk '/^host: /{print $2}')}

# Setting RUSTFLAGS replaces the target rustflags in .cargo/config.toml, which
# pin x86_64 Linux builds to -Ctarget-cpu=x86-64-v2. Keep that baseline when the
# caller didn't choose a target CPU.
if [[ -z ${RUSTFLAGS:-} && $PGO_TARGET == x86_64-unknown-linux-gnu ]]; then
  RUSTFLAGS="-Ctarget-cpu=x86-64-v2"
fi
base_rustflags=${RUSTFLAGS:-}

# The instrumented validator can outgrow aarch64's +/-128 MB branch range, and
# GNU ld then fails with R_AARCH64_CALL26 instead of inserting range-extension
# thunks; lld inserts them. Pass 1 only: it just has to run the training
# workload. aarch64-apple-darwin links with ld64 and is unaffected.
if [[ -z ${PGO_INSTRUMENT_LDFLAGS+set} ]]; then
  PGO_INSTRUMENT_LDFLAGS=""
  if [[ $PGO_TARGET == aarch64-unknown-linux-* ]]; then
    PGO_INSTRUMENT_LDFLAGS="-Clink-arg=-fuse-ld=lld"
  fi
fi

llvm_profdata=$(find "$(rustc --print sysroot)" -name llvm-profdata -type f 2>/dev/null | head -1)
if [[ -z $llvm_profdata ]]; then
  echo "llvm-profdata not found; run: rustup component add llvm-tools-preview" >&2
  exit 1
fi

case $cmd in
  instrument)
    rm -rf "$profile_dir"
    mkdir -p "$profile_dir"
    RUSTFLAGS="$base_rustflags -Cprofile-generate=$profile_dir $PGO_INSTRUMENT_LDFLAGS" \
      cargo build --release --target "$PGO_TARGET" "$@"
    echo
    echo "Instrumented binaries are in target/$PGO_TARGET/release/."
    echo "Run them on a representative workload, stop them, then run:"
    echo "  $0 build $*"
    ;;
  build)
    if ! compgen -G "$profile_dir/*.profraw" >/dev/null; then
      echo "no .profraw files in $profile_dir" >&2
      echo "run '$0 instrument ...' and exercise the instrumented binary first" >&2
      exit 1
    fi
    "$llvm_profdata" merge -o "$profdata" "$profile_dir"
    RUSTFLAGS="$base_rustflags -Cprofile-use=$profdata" \
      cargo build --release --target "$PGO_TARGET" "$@"
    echo
    echo "Optimized binaries are in target/$PGO_TARGET/release/."
    ;;
  *)
    usage
    ;;
esac
