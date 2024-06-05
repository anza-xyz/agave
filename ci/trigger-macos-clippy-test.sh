#!/usr/bin/env bash

set -e

: "${GH_TOKEN:?}"
: "${CI_BRANCH:?}"
: "${CI_COMMIT:?}"

curl -L -X POST --fail \
  -H "Accept: application/vnd.github+json" \
  -H "Authorization: Bearer $GH_TOKEN" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  https://api.github.com/repos/anza-xyz/agave/actions/workflows/cargo.yml/dispatches \
  -d '{"ref":"master","inputs":{"ref":'"$CI_BRANCH"',"sha":'"$CI_COMMIT"'}}'
