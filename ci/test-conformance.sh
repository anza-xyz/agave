#!/usr/bin/env bash
#
# Usage: ci/test-conformance.sh <package> <bin> <fixtures-dir>

set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <package> <bin> <fixtures-dir>" >&2
  exit 1
fi
package=$1
bin=$2
fixtures_dir=$3

root="$(cd "$(dirname "$0")/.." && pwd)"

"$root"/cargo stable build --release --locked \
  --manifest-path "$root/dev-bins/Cargo.toml" --package "$package" --bin "$bin"
harness="${CARGO_TARGET_DIR:-$root/dev-bins/target}/release/$bin"

total=$(find "$fixtures_dir" -type f -name '*.fix' | wc -l)
if ((total == 0)); then
  echo "error: no fixtures in $fixtures_dir" >&2
  exit 1
fi

log=$(mktemp)
batch_logs=$(mktemp -d)
echo "Running $bin on $total fixtures in $fixtures_dir"

# shellcheck disable=SC2016 # expanded by the inner sh, not here
find "$fixtures_dir" -type f -name '*.fix' -print0 |
  BATCH_LOGS="$batch_logs" xargs -0 -n 256 -P "$(nproc)" sh -c \
    'exec "$0" "$@" >"$(mktemp -p "$BATCH_LOGS")" 2>&1' "$harness" || true
cat "$batch_logs"/* >"$log"

passed=$(grep -c '^OK: ' "$log" || true)
if ((passed != total)); then
  grep -v '^OK: ' "$log" | head -n 2000 || true
  echo "error: $bin passed $passed/$total fixtures in $fixtures_dir" >&2
  exit 1
fi
echo "$bin passed $passed/$total fixtures in $fixtures_dir"
