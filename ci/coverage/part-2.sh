#!/usr/bin/env bash

set -euo pipefail
git_root=$(git rev-parse --show-toplevel)

# shellcheck source=ci/coverage/common.sh
source "$git_root"/ci/coverage/common.sh

packages=()
for package in "${PART_2_PACKAGES[@]}"; do
  packages+=(--package "$package")
done

echo "--- coverage: root (part 2)"
"$git_root"/ci/test-coverage.sh \
  --features dev-context-only-utils \
  --lib \
  "${packages[@]}"
