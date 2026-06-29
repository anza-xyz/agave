#!/usr/bin/env bash
#
# Conformance test-vectors table builder (stub).
#
# Detects which workspace crates changed in a pull request, maps them to
# conformance fixture sets via an "anchor crate" model, and prints a JSON
# array of { harness, fixtures_dir } entries to stdout. An empty array means
# no fixture sets are affected.
#
# This is a stub: it only identifies what *would* run once the test-vectors
# project publishes release artifacts. No fixtures are downloaded or executed.
# See: https://github.com/anza-xyz/agave/issues/13387
#
# Model: each fixture set declares one anchor crate. A fixture set is selected
# when any changed crate is in {anchor} ∪ {direct normal dependencies of anchor}.
# Those direct deps are read from the live cargo graph (cargo tree --depth 1)
# so they stay correct as deps evolve. Dev-dependencies are excluded, and the
# graph is walked only one level deep to avoid pulling low-level crates like
# solana-program-runtime into nearly every anchor.
#
# Usage:
#   ci/conformance-table.sh [CRATE...]
#
# When called with arguments, CRATE... is the list of changed workspace crate
# names (space-separated or multiple args). When called with no arguments the
# script computes changed crates itself from COMMIT_RANGE (default:
# origin/master...HEAD) using git diff + cargo metadata.
#
# Output: JSON array to stdout, one object per selected fixture set.
#   []                                                  -- nothing affected
#   [{"harness":"sol_compat_instr_v1","fixtures_dir":"instr"},...]

set -eo pipefail

# ---------------------------------------------------------------------------
# Fixture set -> anchor crate. Keep in sync with the conformance harnesses.
# ---------------------------------------------------------------------------
declare -A anchor_crates=(
  ["instr"]="solana-svm"
  ["txn"]="solana-runtime"
  ["block"]="solana-ledger"
  ["elf_loader"]="solana-program-runtime"
  ["syscall"]="solana-program-runtime"
  ["vm_serialization"]="solana-program-runtime"
  ["cost"]="solana-cost-model"
  ["shred"]="solana-core"
  ["gossip"]="solana-gossip"
)

# Harness binary name for each fixture set.
declare -A harness_names=(
  ["instr"]="sol_compat_instr_v1"
  ["txn"]="sol_compat_txn_v1"
  ["block"]="sol_compat_block_v1"
  ["elf_loader"]="sol_compat_elf_loader_v1"
  ["syscall"]="sol_compat_syscall_v1"
  ["vm_serialization"]="sol_compat_vm_serialization_v1"
  ["cost"]="sol_compat_cost_v1"
  ["shred"]="sol_compat_shred_v1"
  ["gossip"]="sol_compat_gossip_v1"
)

# Stable iteration order for deterministic output.
declare -a fixtures=(
  instr txn block elf_loader syscall vm_serialization cost shred gossip
)

# ---------------------------------------------------------------------------
# 1. Determine the set of changed workspace crates.
# ---------------------------------------------------------------------------
declare -A changed_crates=()

if [[ $# -gt 0 ]]; then
  # Caller supplied the crate list directly.
  for crate in "$@"; do
    changed_crates["$crate"]=1
  done
else
  # Derive from git diff + cargo metadata.
  commit_range="${COMMIT_RANGE:-origin/master...HEAD}"
  echo "Using commit range: $commit_range" >&2

  mapfile -t changed_files < <(git diff "$commit_range" --diff-filter=ACMR --name-only)
  if [[ ${#changed_files[@]} -eq 0 ]]; then
    echo "No changed files detected; nothing to dispatch." >&2
    echo "[]"
    exit 0
  fi

  # Map repo-relative dirs to crate names (longest prefix wins).
  mapfile -t crate_dirs < <(
    cargo metadata --no-deps --format-version 1 |
      jq -r '
        .workspace_root as $root
        | .packages[]
        | (.manifest_path | rtrimstr("/Cargo.toml")) as $dir
        | "\($dir | ltrimstr($root + "/"))\t\(.name)"
      ' |
      awk -F'\t' '{ print length($1), $0 }' |
      sort -rn |
      cut -d' ' -f2-
  )

  for file in "${changed_files[@]}"; do
    for entry in "${crate_dirs[@]}"; do
      dir="${entry%%$'\t'*}"
      name="${entry#*$'\t'}"
      if [[ "$file" == "$dir" || "$file" == "$dir"/* ]]; then
        changed_crates["$name"]=1
        break
      fi
    done
  done

  if [[ ${#changed_crates[@]} -eq 0 ]]; then
    echo "No changed files map to a workspace crate; nothing to dispatch." >&2
    echo "[]"
    exit 0
  fi
fi

echo "Changed workspace crates: ${!changed_crates[*]}" >&2

# ---------------------------------------------------------------------------
# 2. Precompute anchor trigger sets (anchor + its direct normal deps).
# ---------------------------------------------------------------------------
declare -A anchor_deps=()
for fixture in "${fixtures[@]}"; do
  anchor="${anchor_crates[$fixture]}"
  if [[ -z "${anchor_deps[$anchor]+set}" ]]; then
    anchor_deps["$anchor"]="$(
      cargo tree --package "$anchor" --edges normal --depth 1 --prefix none |
        awk '{ print $1 }' |
        sort -u
    )"
  fi
done

# ---------------------------------------------------------------------------
# 3. Select fixture sets and emit JSON.
# ---------------------------------------------------------------------------
declare -a entries=()
for fixture in "${fixtures[@]}"; do
  anchor="${anchor_crates[$fixture]}"
  trigger_crates="${anchor_deps[$anchor]}"
  for crate in "${!changed_crates[@]}"; do
    if grep -qxF -- "$crate" <<<"$trigger_crates"; then
      harness="${harness_names[$fixture]}"
      entries+=("{\"harness\":\"$harness\",\"fixtures_dir\":\"$fixture\"}")
      echo "selected: $fixture (harness=$harness, matched crate=$crate)" >&2
      break
    fi
  done
done

# Build JSON array.
if [[ ${#entries[@]} -eq 0 ]]; then
  echo "[]"
else
  (
    IFS=','
    echo "[${entries[*]}]"
  )
fi
