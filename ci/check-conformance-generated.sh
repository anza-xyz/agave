#!/usr/bin/env bash
#
# Usage: ci/check-conformance-generated.sh <base-commit>
#
# Emits instruction fixtures from the Loader V3 tests at <base-commit> and in
# this checkout, and fails if the two sets differ. A fixture holds both the
# input and the effects, so this catches changes to the tests as well as to
# behavior. Fixtures only emitted at <base-commit> are replayed against this
# checkout, to show whether their effects changed.

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <base-commit>" >&2
  exit 1
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
base=$(git rev-parse --verify "$1^{commit}")

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
base_tree=$work/base
mkdir -p "$base_tree"
git archive "$base" | tar -x -C "$base_tree"

if ! grep -q EMIT_CONFORMANCE_FIXTURES "$base_tree/svm/src/conformance/instr/harness.rs"; then
  echo "skipping: ${base:0:10} can't emit conformance fixtures"
  exit 0
fi

# Single-threaded, so each run emits the same fixtures.
emit() {
  local tree=$1 target=$2 out=$3
  EMIT_CONFORMANCE_FIXTURES=$out CARGO_TARGET_DIR=$target \
    "$tree"/cargo stable test --locked --manifest-path "$tree/Cargo.toml" \
    --package solana-bpf-loader-program --tests -- --test-threads=1
}

# The base gets its own target directory. Cargo can't tell the base's sources
# apart from this checkout's, so a shared one would rerun this checkout's tests.
echo "Emitting fixtures at ${base:0:10}"
emit "$base_tree" "$work/target" "$work/base"
echo "Emitting fixtures in this checkout"
emit "$root" "${CARGO_TARGET_DIR:-$root/target}" "$work/head"

# Fixtures are named by a hash of their contents, so comparing names compares
# contents.
list() { find "$1" -type f -name '*.fix' -printf '%f\n' | sort; }
removed=$(comm -23 <(list "$work/base") <(list "$work/head"))
added=$(comm -13 <(list "$work/base") <(list "$work/head"))

if [[ -z $removed && -z $added ]]; then
  echo "$(list "$work/head" | wc -l) fixtures, identical at ${base:0:10} and in this checkout"
  exit 0
fi

echo "Fixtures differ from ${base:0:10}:" \
  "$(grep -c . <<<"$removed") only at the base, $(grep -c . <<<"$added") only in this checkout"
if [[ -n $removed ]]; then
  mkdir "$work/removed"
  while IFS= read -r fixture; do
    cp "$work/base/$fixture" "$work/removed/"
  done <<<"$removed"
  echo "Replaying the base-only fixtures in this checkout; those that pass changed only their inputs:"
  "$root"/ci/test-conformance.sh solana-svm-conformance test_exec_instr "$work/removed" || true
fi

echo "error: the Loader V3 test fixtures changed" >&2
exit 1
