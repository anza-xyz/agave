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
echo "Running $bin on $total fixtures in $fixtures_dir"

find "$fixtures_dir" -type f -name '*.fix' -print0 |
  xargs -0 -n 256 -P "$(nproc)" "$harness" >"$log" 2>&1 || true

passed=$(grep -c '^OK: ' "$log" || true)
if ((passed != total)); then
  grep -v '^OK: ' "$log" | head -n 2000 || true
  echo "error: $bin passed $passed/$total fixtures in $fixtures_dir" >&2
  exit 1
fi
echo "$bin passed $passed/$total fixtures in $fixtures_dir"
