#!/usr/bin/env bash

set -eo pipefail
here="$(dirname "$0")"
src_root="$(readlink -f "${here}/..")"
cd "${src_root}"

# Logging hygiene: Please don't print from --lib, use the `log` crate instead
declare prints=(
  'print!'
  'println!'
  'eprint!'
  'eprintln!'
  'dbg!'
)

# Parts of the tree that are expected to be print free
declare print_free_tree=(
  ':**.rs'
  ':^msg/src/lib.rs'
  ':^program-option/src/lib.rs'
  ':^pubkey/src/lib.rs'
  ':^sysvar/src/program_stubs.rs'
  ':^**bin**.rs'
  ':^**bench**.rs'
  ':^**test**.rs'
  ':^**/build.rs'
)

if git --no-pager grep -n "${prints[@]/#/-e}" -- "${print_free_tree[@]}"; then
    exit 1
fi

# Github Issues should be used to track outstanding work items instead of
# marking up the code
#
# Ref: https://github.com/solana-labs/solana/issues/6474
#
# shellcheck disable=1001
declare useGithubIssueInsteadOf=(
  X\XX
  T\BD
  F\IXME
  #T\ODO  # TODO: Disable TODOs once all other TODOs are purged
)

if git --no-pager grep -n --max-depth=0 "${useGithubIssueInsteadOf[@]/#/-e }" -- '*.rs' '*.sh' '*.md'; then
    exit 1
fi

# TODO: Remove this `git grep` once TODOs are banned above
#       (this command is only used to highlight the current offenders)
git --no-pager grep -n --max-depth=0 "-e TODO" -- '*.rs' '*.sh' '*.md' || true
# END TODO
