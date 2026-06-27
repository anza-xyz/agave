#!/usr/bin/env bash
#
# Conformance test-vectors CI dispatcher (stub).
#
# Maps the crates changed in a pull request to the conformance fixture sets they
# affect, then *logs* which fixture sets would run. It does not download fixtures
# or execute any conformance tests yet -- that lands once the test-vectors
# project publishes release artifacts.
# See: https://github.com/anza-xyz/agave/issues/13387
#
# Model: each fixture set declares one "anchor" crate. A fixture set is selected
# when any changed crate is in {anchor} ∪ {direct normal dependencies of anchor}.
# Those direct dependencies are read from the live cargo graph (`cargo tree
# --edges normal --depth 1`) so they stay correct as dependencies evolve.
# Dev-dependencies are excluded, and the graph is walked only one level deep on
# purpose: a full transitive closure would pull low-level crates (e.g.
# solana-program-runtime) into nearly every anchor and select almost every
# fixture set.
#
# input:
#   env:
#     - COMMIT_RANGE   git revision range to diff (e.g. <base>..<head>), set by
#                      the workflow. Defaults to `origin/master...HEAD` for local
#                      runs.

set -eo pipefail

# Fixture set -> anchor crate. Keep in sync with the conformance harnesses.
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

# Stable iteration order for deterministic logs.
declare -a fixtures=(
  instr txn block elf_loader syscall vm_serialization cost shred gossip
)

commit_range="${COMMIT_RANGE:-origin/master...HEAD}"
echo "Using commit range: $commit_range"

# 1. Files changed in the range (Added/Copied/Modified/Renamed).
mapfile -t changed_files < <(git diff "$commit_range" --diff-filter=ACMR --name-only)
if [[ ${#changed_files[@]} -eq 0 ]]; then
  echo "No changed files detected; nothing to dispatch."
  exit 0
fi

# 2. Map each workspace crate to its repo-relative directory. Lines are
#    "<relative-dir>\t<crate-name>", sorted longest dir first so the first prefix
#    match wins (handles nested crates like accounts-db/store-histogram).
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

# 3. Attribute each changed file to the crate whose directory owns it.
declare -A changed_crates=()
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
  echo "No changed files map to a workspace crate; nothing to dispatch."
  exit 0
fi

echo "Changed workspace crates:"
printf '%s\n' "${!changed_crates[@]}" | sort | sed 's/^/  - /'

# 4. Precompute the trigger set (anchor + its direct normal deps) for each unique
#    anchor crate.
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

# 5. Select a fixture set when a changed crate is its anchor or a direct dep.
declare -a selected=()
for fixture in "${fixtures[@]}"; do
  anchor="${anchor_crates[$fixture]}"
  trigger_crates="${anchor_deps[$anchor]}"
  matched=()
  for crate in "${!changed_crates[@]}"; do
    if grep -qxF -- "$crate" <<<"$trigger_crates"; then
      matched+=("$crate")
    fi
  done
  if [[ ${#matched[@]} -gt 0 ]]; then
    selected+=("$fixture")
    mapfile -t matched_sorted < <(printf '%s\n' "${matched[@]}" | sort)
    echo "would run fixture '$fixture' (anchor $anchor; matched: ${matched_sorted[*]})"
  fi
done

if [[ ${#selected[@]} -eq 0 ]]; then
  echo "No conformance fixture sets affected by these changes."
else
  echo "Selected fixture sets: ${selected[*]}"
fi

# Optional human-readable summary when running on GitHub Actions.
if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
  {
    echo "### Conformance dispatch (stub)"
    if [[ ${#selected[@]} -eq 0 ]]; then
      echo "_No fixture sets affected by these changes._"
    else
      echo "Would run: \`${selected[*]}\`"
    fi
    echo ""
    echo "_Logging only; no fixtures were downloaded or executed._"
  } >>"$GITHUB_STEP_SUMMARY"
fi

echo "Done (stub: no fixtures downloaded or executed)."
