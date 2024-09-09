#!/usr/bin/env bash
set -ex

cd "$(dirname "$0")"

# shellcheck source=ci/env.sh
source ../ci/env.sh

# shellcheck source=ci/rust-version.sh
echo "build.sh starting build cli usage"
source ../ci/rust-version.sh
../ci/docker-run-default-image.sh docs/build-cli-usage.sh
echo "build.sh starting convert ascii to svg"
../ci/docker-run-default-image.sh docs/convert-ascii-to-svg.sh
./set-solana-release-tag.sh
echo "build.sh starting npm run build"
# Build from /src into /build
npm run build
echo $?
echo "build.sh checking if publish"
CI="TODO remove"
# Publish only from merge commits and beta release tags
if [[ -n $CI ]]; then
  if [[ -z $CI_PULL_REQUEST ]]; then
    if [[ -n $CI_TAG ]] && [[ $CI_TAG != $BETA_CHANNEL* ]]; then
      echo "not a beta tag"
      exit 0
    fi
    ./publish-docs.sh
  fi
fi
