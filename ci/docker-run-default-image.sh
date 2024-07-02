#!/usr/bin/env bash

set -e

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck disable=SC1091
source "$here/rust-version.sh"

ci_docker_image=anzaxyz/ci:rust_1.78.0_nightly-2024-05-02_rpbf_fuzz
"$here/docker-run.sh" "${ci_docker_image:?}" "$@"
