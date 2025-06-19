#!/usr/bin/env bash
#
# Fetches the latest Core BPF programs and produces the solana-genesis
# command-line arguments needed to install them
#

set -e

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" &>/dev/null && pwd)

source "$here"/fetch-programs.sh

PREFIX="core-bpf"

programs=()

add_core_bpf_program_to_fetch() {
  declare name=$1
  declare version=$2
  declare address=$3
  declare loader=$4

  so_name="solana_${name//-/_}_program.so"
  declare download_url="https://github.com/solana-program/$name/releases/download/program%40$version/$so_name"

  programs+=("$name $version $address $loader $download_url")
}

add_core_bpf_program_to_fetch address-lookup-table 3.0.0 AddressLookupTab1e1111111111111111111111111 BPFLoaderUpgradeab1e11111111111111111111111
add_core_bpf_program_to_fetch config 3.0.0 Config1111111111111111111111111111111111111 BPFLoaderUpgradeab1e11111111111111111111111
add_core_bpf_program_to_fetch feature-gate 0.0.1 Feature111111111111111111111111111111111111 BPFLoaderUpgradeab1e11111111111111111111111

fetch_programs "$PREFIX" "${programs[@]}"
