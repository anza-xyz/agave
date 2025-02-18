#!/usr/bin/env bash

set -e

echo "branch: $BUILDKITE_BRANCH"
echo "commit: $BUILDKITE_COMMIT"

curl -L \
  -X POST \
  -H "Accept: application/vnd.github+json" \
  -H "Authorization: Bearer $GITHUB_TOKEN" \
  -H "X-GitHub-Api-Version: 2022-11-28" \
  https://api.github.com/repos/anza-xyz/agave/actions/workflows/publish-windows-tarball.yml/dispatches \
  -d '{"ref":"'"$BUILDKITE_BRANCH"'","inputs":{"commit":"'"$BUILDKITE_COMMIT"'"}}'
