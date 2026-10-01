#!/usr/bin/env bash
# shellcheck disable=SC2034
# Defines reusable lists of Agave binary names for use across scripts.
# The lists live in [workspace.metadata.agave-build-lists] in Cargo.toml.

# Source this file to access the arrays
# Example:
#   source "scripts/agave-build-lists.sh"
#   printf '%s\n' "${AGAVE_BINS_DEV[@]}"

AGAVE_BINS_DEV=()
AGAVE_BINS_END_USER=()
AGAVE_BINS_VAL_OP=()
AGAVE_BINS_DCOU=()
AGAVE_BINS_DEPRECATED=()
DCOU_TAINTED_PACKAGES=()

_manifest="$(dirname "${BASH_SOURCE[0]:-$0}")/../Cargo.toml"

if ! _metadata="$(cargo metadata --no-deps --format-version=1 --manifest-path "$_manifest")"; then
  echo "error: cargo metadata failed for $_manifest" >&2
  return 1 2>/dev/null || exit 1
fi

# One `<list> <name>` line per entry, e.g. `dev solana-test-validator`
if ! _lists="$(jq -er '.metadata["agave-build-lists"] | to_entries[] | "\(.key) \(.value[])"' <<< "$_metadata")"; then
  echo "error: no [workspace.metadata.agave-build-lists] in $_manifest" >&2
  return 1 2>/dev/null || exit 1
fi

while read -r _list _name; do
  case "$_list" in
    dev) AGAVE_BINS_DEV+=("$_name") ;;
    end-user) AGAVE_BINS_END_USER+=("$_name") ;;
    val-op) AGAVE_BINS_VAL_OP+=("$_name") ;;
    dcou) AGAVE_BINS_DCOU+=("$_name") ;;
    deprecated) AGAVE_BINS_DEPRECATED+=("$_name") ;;
    dcou-tainted-packages) DCOU_TAINTED_PACKAGES+=("$_name") ;;
    *)
      echo "error: unknown build list in $_manifest: $_list" >&2
      return 1 2>/dev/null || exit 1
      ;;
  esac
done <<< "$_lists"

unset _manifest _metadata _lists _list _name
