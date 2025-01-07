#!/usr/bin/env bash

ci_docker_env_sh_here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck disable=SC1091
source "${ci_docker_env_sh_here}/../rust-version.sh"

if [[ -z "${rust_stable}" || -z "${rust_nightly}" ]]; then
  echo "Error: rust_stable or rust_nightly is empty. Please check rust-version.sh." >&2
  exit 1
fi

export CI_DOCKER_ARG_BASE_IMAGE=ubuntu:22.04
export CI_DOCKER_ARG_RUST_VERSION="${rust_stable}"
export CI_DOCKER_ARG_RUST_NIGHTLY_VERSION="${rust_nightly}"
export CI_DOCKER_ARG_GOLANG_VERSION=1.21.3
export CI_DOCKER_ARG_NODE_MAJOR=18
export CI_DOCKER_ARG_SCCACHE_VERSION=v0.8.1
export CI_DOCKER_ARG_GRCOV_VERSION=v0.8.18

hash_vars=(
  "${CI_DOCKER_ARG_BASE_IMAGE}"
  "${CI_DOCKER_ARG_RUST_VERSION}"
  "${CI_DOCKER_ARG_RUST_NIGHTLY_VERSION}"
  "${CI_DOCKER_ARG_GOLANG_VERSION}"
  "${CI_DOCKER_ARG_NODE_MAJOR}"
  "${CI_DOCKER_ARG_SCCACHE_VERSION}"
  "${CI_DOCKER_ARG_GRCOV_VERSION}"
)
hash_input=$(IFS="_"; echo "${hash_vars[*]}")
ci_docker_hash=$(echo -n "${hash_input}" | sha256sum | head -c 8)

CI_DOCKER_SANITIZED_BASE_IMAGE="${CI_DOCKER_ARG_BASE_IMAGE//:/-}"
export CI_DOCKER_IMAGE="anzaxyz/ci:${CI_DOCKER_SANITIZED_BASE_IMAGE}_rust-${CI_DOCKER_ARG_RUST_VERSION}_${CI_DOCKER_ARG_RUST_NIGHTLY_VERSION}_${ci_docker_hash}"
