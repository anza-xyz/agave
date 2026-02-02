#!/usr/bin/env bash
set -ex

cd "$(dirname "$0")"/..
eval "$(ci/channel-info.sh)"
source ci/rust-version.sh
source ci/docker/env.sh

CHANNEL_OR_TAG=
if [[ -n "$CI_TAG" ]]; then
  CHANNEL_OR_TAG=$CI_TAG
else
  CHANNEL_OR_TAG=$CHANNEL
fi

if [[ -z $CHANNEL_OR_TAG ]]; then
  echo Unable to determine channel or tag to publish into, exiting.
  echo "^^^ +++"
  exit 0
fi

cd "$(dirname "$0")"

# download from release.anza.xyz
rm -rf /tmp/docker-solana
sh -c "$(curl -sSfL "https://release.anza.xyz/${CHANNEL_OR_TAG}/install")" init "${CHANNEL_OR_TAG}" --data-dir /tmp/docker-solana

# prepare usr/bin
rm -rf usr/
mkdir -p usr/bin
cp -r /tmp/docker-solana/active_release/bin/* usr/bin/

docker build \
  --build-arg "BASE_IMAGE=${CI_DOCKER_ARG_BASE_IMAGE}" \
  -t anzaxyz/agave:"$CHANNEL_OR_TAG" .

maybeEcho=
if [[ -z $CI ]]; then
  echo "Not CI, skipping |docker push|"
  maybeEcho="echo"
else
  (
    set +x
    if [[ -n $DOCKER_PASSWORD && -n $DOCKER_USERNAME ]]; then
      echo "$DOCKER_PASSWORD" | docker login --username "$DOCKER_USERNAME" --password-stdin
    fi
  )
fi
$maybeEcho docker push anzaxyz/agave:"$CHANNEL_OR_TAG"
