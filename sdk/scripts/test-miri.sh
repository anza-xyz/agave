#!/usr/bin/env bash

set -eo pipefail
here="$(dirname "$0")"
src_root="$(readlink -f "${here}/..")"
cd "${src_root}"
# miri is very slow; so only run very few of selective tests!
./cargo nightly miri test -p solana-program -- hash:: account_info::
