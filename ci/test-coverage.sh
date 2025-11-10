#!/usr/bin/env bash

set -e
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)

if [[ -z $CI ]]; then
  echo "This script is used by CI environment. \
Use \`scripts/coverage.sh\` directly if you only want to obtain the coverage report"
  exit 1
fi

# run coverage for all
SHORT_CI_COMMIT=${CI_COMMIT:0:9}
COMMIT_HASH=$SHORT_CI_COMMIT "$here/../scripts/coverage.sh" "$@"

echo "--- codecov.io report"
if [[ -z "$CODECOV_TOKEN" ]]; then
  echo "^^^ +++"
  echo CODECOV_TOKEN undefined, codecov.io upload skipped
else
  codecov -t "${CODECOV_TOKEN}" --dir "$here/../target/cov/${SHORT_CI_COMMIT}"

  if [[ -n "$BUILDKITE" ]]; then
    buildkite-agent annotate --style success --context codecov.io \
      "CodeCov report: https://codecov.io/github/anza-xyz/agave/commit/$CI_COMMIT"
  fi
fi
