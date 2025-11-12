#!/usr/bin/env bash

set -euo pipefail
git_root=$(git rev-parse --show-toplevel)

# shellcheck source=ci/coverage/common.sh
source "$git_root"/ci/coverage/common.sh

packages=()
for package in "${PART_1_PACKAGES[@]}"; do
  packages+=(--package "$package")
done

echo "--- coverage: root (part 1)"
"$git_root"/ci/test-coverage.sh \
  --features frozen-abi \
  --features dev-context-only-utils \
  --lib \
  "${packages[@]}"
