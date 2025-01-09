#!/usr/bin/env bash
set -e

here="$(dirname "${BASH_SOURCE[0]}")"
program="$1"

#shellcheck source=ci/downstream-projects/common.sh
source "$here"/../../ci/downstream-projects/common.sh

set -x
rm -rf "${program}"
git clone https://github.com/solana-program/"${program}".git

# copy toolchain file to use solana's rust version
cp "$SOLANA_DIR"/rust-toolchain.toml "${program}"/
cd "${program}" || exit 1
echo "HEAD: $(git rev-parse HEAD)"

project_used_solana_version=$(sed -nE 's/solana = \"(.*)\"/\1/p' <"Cargo.toml")
echo "used solana version: $project_used_solana_version"
if semverGT "$project_used_solana_version" "$SOLANA_VER"; then
  echo "skip"
  export SKIP_SPL_DOWNSTREAM_PROJECT_TEST=1
  return
fi

update_solana_dependencies . "$SOLANA_VER"
patch_crates_io_solana Cargo.toml "$SOLANA_DIR"
