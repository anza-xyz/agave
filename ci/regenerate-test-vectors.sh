#!/usr/bin/env bash
#
# Usage: ci/regenerate-test-vectors.sh <set>...
#
# Regenerates the fixtures in each test-vectors set that fail against this
# checkout, then prints a decoded diff of every regenerated fixture.
#
# Fixtures are fetched into fd-vectors/<set> and regenerated ones are written
# to fd-vectors/regenerated/vectors, mirroring the test-vectors layout. If
# TEST_VECTORS_REPO points at a test-vectors checkout, fixtures are read from
# it instead and regenerated ones overwrite the originals, ready to commit.
#
# The rest of the run (logs, fixtures, decoded text, diff) is kept in
# fd-vectors/regenerated for inspection until the next run.
#
# Requires solana-conformance: https://github.com/anza-xyz/agave/wiki/Conformance-Tools

set -euo pipefail

# Mirrors FIXTURE_ANCHORS in ci/xtask/src/commands/conformance_table.rs.
harness() {
  case $1 in
    block | cost | gossip | shred | txn) echo "agave-conformance test_exec_$1" ;;
    elf_loader | instr) echo "solana-svm-conformance test_exec_$1" ;;
    syscall | vm_serialization) echo "solana-svm-conformance test_exec_vm_${1#vm_}" ;;
    *) return 1 ;;
  esac
}

stage() {
  echo -e "\n==> $*"
}

if [[ $# -eq 0 ]]; then
  echo "usage: [TEST_VECTORS_REPO=<path>] $0 <set>..." >&2
  exit 1
fi
for set in "$@"; do
  harness "$set" >/dev/null || {
    echo "error: unknown fixture set: $set" >&2
    exit 1
  }
done
if ! command -v solana-conformance >/dev/null; then
  echo "error: solana-conformance not found; see https://github.com/anza-xyz/agave/wiki/Conformance-Tools" >&2
  exit 1
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
out=$root/fd-vectors/regenerated
rm -rf "$out"
mkdir -p "$out"

if [[ -n ${TEST_VECTORS_REPO:-} ]]; then
  src=$(cd "$TEST_VECTORS_REPO" && pwd)
  dest=$src
else
  stage "Fetching fixtures: $*"
  "$root"/ci/download-test-vectors.sh "$@"
  src=$root/fd-vectors
  dest=$out/vectors
fi

for set in "$@"; do
  read -r package bin < <(harness "$set")
  work=$out/$set

  stage "[$set] Running $bin on $src/$set"
  mkdir -p "$work"
  if "$root"/ci/test-conformance.sh "$package" "$bin" "$src/$set" >"$work/check.log" 2>&1; then
    tail -n 1 "$work/check.log"
    continue
  fi
  mapfile -t failed < <(sed -n 's/^FAIL: "\(.*\)"$/\1/p' "$work/check.log")
  if ((${#failed[@]} == 0)); then
    cat "$work/check.log" >&2
    echo "error: $bin failed without reporting failing fixtures" >&2
    exit 1
  fi
  echo "${#failed[@]} fixture(s) failed:"
  printf '  %s\n' "${failed[@]#"$src"/}"

  # create-fixtures writes its output flat, so stage the failures flat too.
  mkdir "$work/old"
  for fixture in "${failed[@]}"; do
    if [[ -e $work/old/$(basename "$fixture") ]]; then
      echo "error: duplicate fixture name: $fixture" >&2
      exit 1
    fi
    cp "$fixture" "$work/old/"
  done

  stage "[$set] Regenerating ${#failed[@]} fixture(s) with $package"
  "$root"/cargo stable build --release --locked \
    --manifest-path "$root/dev-bins/Cargo.toml" --package "$package" --lib
  solana-conformance create-fixtures -q -i "$work/old" -o "$work/new" \
    -t "${CARGO_TARGET_DIR:-$root/dev-bins/target}/release/lib${package//-/_}.so"
  if ! "$root"/ci/test-conformance.sh "$package" "$bin" "$work/new" >"$work/verify.log" 2>&1; then
    cat "$work/verify.log" >&2
    echo "error: regenerated $set fixtures still fail" >&2
    exit 1
  fi

  stage "[$set] Writing ${#failed[@]} fixture(s) to $dest/$set"
  for fixture in "${failed[@]}"; do
    rel=${fixture#"$src"/}
    mkdir -p "$(dirname "$dest/$rel")"
    cp "$work/new/$(basename "$fixture")" "$dest/$rel"
  done
  solana-conformance decode-protobufs -q -i "$work/old" -o "$out/decoded/old/$set"
  solana-conformance decode-protobufs -q -i "$work/new" -o "$out/decoded/new/$set"
done

if [[ ! -d $out/decoded ]]; then
  stage "Nothing to regenerate"
  exit 0
fi

stage "Diff of decoded fixtures"
(cd "$out/decoded" && git --no-pager diff --no-index old new | tee ../diff.txt) || true

stage "Done"
echo "Regenerated fixtures: $dest"
echo "Run artifacts (logs, fixtures, decoded text, diff.txt): $out"
