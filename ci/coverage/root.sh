#!/usr/bin/env bash

set -e
git_root=$(git rev-parse --show-toplevel)

"$git_root"/ci/test-coverage.sh
