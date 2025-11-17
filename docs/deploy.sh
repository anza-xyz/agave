#!/usr/bin/env bash

set -euo pipefail

cd "$(dirname "$0")"

# Publish only from merge commits and beta release tags
if [[ -n $CI ]]; then
  if [[ -z $CI_PULL_REQUEST ]]; then
    eval "$(../ci/channel-info.sh)"
    if [[ -n $CI_TAG ]] && [[ $CI_TAG != $BETA_CHANNEL* ]]; then
      echo "not a beta tag"
      exit 0
    fi
    ./publish-docs.sh
  fi
fi
