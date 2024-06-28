#!/usr/bin/env bash
set -e

here="$(dirname "${BASH_SOURCE[0]}")"

#shellcheck source=ci/downstream-projects/common.sh
source "$here"/../../ci/downstream-projects/common.sh

set -x
rm -rf spl
git clone https://github.com/solana-labs/solana-program-library.git spl

# copy toolchain file to use solana's rust version
cp "$SOLANA_DIR"/rust-toolchain.toml spl/
cd spl || exit 1
echo "HEAD: $(git rev-parse HEAD)"

project_used_solana_version=$(sed -nE 's/solana-sdk = \"[>=<~]*(.*)\"/\1/p' <"token/program/Cargo.toml")
echo "used solana version: $project_used_solana_version"
if semverGT "$project_used_solana_version" "$SOLANA_VER"; then
  echo "skip"
  return
fi

./patch.crates-io.sh "$SOLANA_DIR"

# anza migration stopgap. can be removed when agave is fully recommended for public usage.
sed -i 's/solana-geyser-plugin-interface/agave-geyser-plugin-interface/g' ./Cargo.toml

# should be removed when spl project upgrade to agave 2.0
sed -i '/^curve25519-dalek = "3/d' token/client/Cargo.toml
sed -i 's/    curve25519_dalek::scalar::Scalar\,/    spl_token_2022::solana_zk_token_sdk::curve25519_dalek::scalar::Scalar,/' token/client/src/proof_generation.rs

# ignore these tests temporarily. see: https://github.com/anza-xyz/agave/pull/1693#issuecomment-2182615788
sed -i 's/\([ \t]*\)async_trial!(confidential_transfer,/\1\/\/ async_trial!(confidential_transfer,/' token/cli/tests/command.rs
sed -i '/async fn confidential_transfer_transfer_with_fee_and_split_proof_context_in_parallel(/i #[ignore]' token/program-2022-test/tests/confidential_transfer.rs