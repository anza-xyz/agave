#!/usr/bin/env bash
set -ex

cd "$(dirname "$0")"

make -C ../../../programs/sbf/ test-v3
cp ../../../programs/sbf/target/deploy/solana_sbf_rust_noop.so noop.so
cat noop.so noop.so noop.so > noop_large.so
cp ../../../programs/sbf/target/deploy/solana_sbf_rust_alt_bn128.so alt_bn128.so
