#!/usr/bin/env bash
set -e

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

#shellcheck source=ci/stable/common.sh
source "$here"/common.sh

_ cargo nextest run \
  --profile ci \
  --package solana-local-cluster \
  --test local_cluster \
  --partition hash:"$CURRENT/$TOTAL" \
  --test-threads=1 \
  --no-tests=warn
