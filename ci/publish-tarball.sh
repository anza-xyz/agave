#!/usr/bin/env bash
set -e

cd "$(dirname "$0")/.."

DRYRUN=
if [[ -z $CI_BRANCH ]]; then
  DRYRUN="echo"
  CHANNEL=unknown
fi

eval "$(ci/channel-info.sh)"

TAG=
if [[ -n "$CI_TAG" ]]; then
  CHANNEL_OR_TAG=$CI_TAG
  TAG="$CI_TAG"
else
  CHANNEL_OR_TAG=$CHANNEL
fi

if [[ -z $CHANNEL_OR_TAG ]]; then
  echo +++ Unable to determine channel or tag to publish into, exiting.
  exit 0
fi

source scripts/generate-target-triple.sh

TARGET="$BUILD_TARGET_TRIPLE"

if [[ $TARGET == *windows* ]]; then
  (
    set -x
    git --version
    git config core.symlinks true
    find . -type l -delete
    git reset --hard
    # patched crossbeam doesn't build on windows
    sed -i 's/^crossbeam-epoch/#crossbeam-epoch/' Cargo.toml
  )
fi

RELEASE_BASENAME="${RELEASE_BASENAME:=solana-release}"
TARBALL_BASENAME="${TARBALL_BASENAME:="$RELEASE_BASENAME"}"

echo --- Creating release tarball
scripts/create-release-tarball.sh --build-dir "$RELEASE_BASENAME" \
                                  --channel-or-tag "$TAG" \
                                  --target "$TARGET" \
                                  --tarball-basename "$TARBALL_BASENAME"

# Maybe tarballs are platform agnostic, only publish them from the Linux build
MAYBE_TARBALLS=
if [[ $TARGET == *linux* ]]; then
  (
    set -x
    platform-tools-sdk/sbf/scripts/package.sh
    [[ -f sbf-sdk.tar.bz2 ]]
  )
  MAYBE_TARBALLS="sbf-sdk.tar.bz2"
fi

source ci/upload-ci-artifact.sh

for file in "${TARBALL_BASENAME}"-$TARGET.tar.bz2 "${TARBALL_BASENAME}"-$TARGET.yml agave-install-init-"$TARGET"* $MAYBE_TARBALLS; do
  if [[ -n $DO_NOT_PUBLISH_TAR ]]; then
    upload-ci-artifact "$file"
    echo "Skipped $file due to DO_NOT_PUBLISH_TAR"
    continue
  fi

  if [[ -n $BUILDKITE ]]; then
    echo --- GCS Store: "$file"
    upload-gcs-artifact "/solana/$file" gs://anza-release/"$CHANNEL_OR_TAG"/"$file"

    echo Published to:
    $DRYRUN ci/format-url.sh https://release.anza.xyz/"$CHANNEL_OR_TAG"/"$file"

    if [[ -n $TAG ]]; then
      ci/upload-github-release-asset.sh "$file"
    fi
  elif [[ -n $GITHUB_ACTIONS ]]; then
    mkdir -p github-action-s3-upload/"$CHANNEL_OR_TAG"
    cp -v "$file" github-action-s3-upload/"$CHANNEL_OR_TAG"/

    if [[ -n $TAG ]]; then
      mkdir -p github-action-release-upload/
      cp -v "$file" github-action-release-upload/
    fi
  fi
done

echo --- ok
