#!/usr/bin/env bash

# Runs `cargo clippy` in all individual workspaces in the repository.
#
# We have a number of clippy parameters that we want to enforce across the
# code base.  They are defined here.
#
# This script is run by the CI, so if you want to replicate what the CI is
# doing, better run this script, rather than calling `cargo clippy` manually.
#
# TODO It would be nice to provide arguments to narrow clippy checks to a single
# workspace and/or package.  To speed up the interactive workflow.

set -o errexit

here="$(dirname "$0")"

"$here/cargo-for-all-lock-files.sh" -- clippy \
  --workspace --all-targets --features dummy-for-ci-check,stable-abi -- \
  --deny=warnings
