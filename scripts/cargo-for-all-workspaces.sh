#!/usr/bin/env bash

scripts_base="$(dirname "$0")"
cargo_for_all_lock_files="$scripts_base"/cargo-for-all-lock-files.sh

"$cargo_for_all_lock_files" --only-workspaces "${@}"
