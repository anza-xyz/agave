#!/usr/bin/env bash

set -e
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)

if ! wget --version &>/dev/null; then
  echo "wget is not installed. Please install wget to proceed."
  exit 1
fi

rm -rf "$here"/GOSSIP_PACKETS
wget -r -np -nH  -R "index.html*"  147.28.133.67/GOSSIP_PACKETS
GOSSIP_WIRE_FORMAT_PACKETS="$here/GOSSIP_PACKETS" cargo test --package solana-gossip -- wire_format_tests::tests::test_gossip_wire_format --exact --show-output
