#!/usr/bin/env bash

set -e

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck disable=SC1091
source "$here/env.sh"

platform=()
if [[ $(uname -m) = arm64 ]]; then
  # Ref: https://blog.jaimyn.dev/how-to-build-multi-architecture-docker-images-on-an-m1-mac/#tldr
  platform+=(--platform linux/amd64)
fi

echo "build image: ${CI_DOCKER_IMAGE:?}"
docker build "${platform[@]}" \
  -f "$here/Dockerfile" \
  --build-arg "BASE_IMAGE=${CI_DOCKER_ARG_BASE_IMAGE}" \
  --build-arg "RUST_VERSION=${CI_DOCKER_ARG_RUST_VERSION}" \
  --build-arg "RUST_NIGHTLY_VERSION=${CI_DOCKER_ARG_RUST_NIGHTLY_VERSION}" \
  --build-arg "SCCACHE_VERSION=${CI_DOCKER_ARG_SCCACHE_VERSION}" \
  --build-arg "GRCOV_VERSION=${CI_DOCKER_ARG_GRCOV_VERSION}" \
  -t "$CI_DOCKER_IMAGE" .

docker push "$CI_DOCKER_IMAGE"
