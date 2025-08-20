#!/usr/bin/env bash
set -eo pipefail

CURRENT=$1
: "${CURRENT:?}"

TOTAL=$2
: "${TOTAL:?}"

if [ "$CURRENT" -gt "$TOTAL" ]; then
  echo "Error: The value of CURRENT (\$1) cannot be greater than the value of TOTAL (\$2)."
  exit 1
fi

here="$(dirname "$0")"

#shellcheck source=ci/common/shared-functions.sh
source "$here"/../common/shared-functions.sh

#shellcheck source=ci/common/limit-threads.sh
source "$here"/../common/limit-threads.sh

#shellcheck source=ci/stable/common.sh
source "$here"/common.sh

ARGS=(
  --profile ci
  --workspace
  --tests
  --jobs "$JOBS"
  --partition hash:"$CURRENT/$TOTAL"
  --verbose
  --exclude solana-local-cluster
  --no-tests=warn
)

_ cargo nextest run "${ARGS[@]}"
