#!/usr/bin/env bash
set -eo pipefail

here="$(dirname "$0")"

#shellcheck source=ci/common/shared-functions.sh
source "$here"/../common/shared-functions.sh

JOBS="$(cargo xtask jobs)"
export JOBS

#shellcheck source=ci/stable/common.sh
source "$here"/common.sh

_ ci/intercept.sh cargo test --jobs "$JOBS" --workspace --tests --verbose -- --nocapture
