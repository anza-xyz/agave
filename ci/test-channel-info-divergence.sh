#!/usr/bin/env bash
#
# Runs `ci/channel-info.sh` (current authoritative source) and
# `cargo xtask channel-info` (auto-resolver), diffs their output, and
# annotates the build on mismatch. Never blocks the pipeline.

set -u
cd "$(dirname "$0")/.." || exit 1

bash_out=$(ci/channel-info.sh 2>&1) || {
  echo "ci/channel-info.sh failed (exit $?); skipping divergence check" >&2
  echo "$bash_out" >&2
  exit 0
}

xtask_out=$(cargo run --quiet --manifest-path ci/xtask/Cargo.toml -- channel-info 2>&1) || {
  echo "cargo xtask channel-info failed (exit $?); skipping divergence check" >&2
  echo "$xtask_out" >&2
  exit 0
}

bash_sorted=$(printf '%s\n' "$bash_out" | LC_ALL=C sort)
xtask_sorted=$(printf '%s\n' "$xtask_out" | LC_ALL=C sort)

if [[ "$bash_sorted" == "$xtask_sorted" ]]; then
  echo "channel-info parity OK"
  exit 0
fi

echo "channel-info divergence detected" >&2
echo "--- bash (authoritative)" >&2
echo "$bash_out" >&2
echo "--- xtask" >&2
echo "$xtask_out" >&2

if command -v buildkite-agent >/dev/null 2>&1; then
  {
    echo "**channel-info parity divergence**"
    echo
    echo '```'
    echo "bash (authoritative):"
    echo "$bash_out"
    echo
    echo "xtask:"
    echo "$xtask_out"
    echo '```'
  } | buildkite-agent annotate --style warning --context channel-info-divergence || true
fi

exit 0
