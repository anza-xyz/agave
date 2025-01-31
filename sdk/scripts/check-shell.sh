#!/usr/bin/env bash
#
# Reference: https://github.com/koalaman/shellcheck/wiki/Directive
set -eo pipefail
here="$(dirname "$0")"
src_root="$(readlink -f "${here}/..")"
cd "${src_root}"

set -x
git ls-files -- '*.sh' | xargs shellcheck --color=always --external-sources --shell=bash
