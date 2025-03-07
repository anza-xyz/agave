#!/usr/bin/env bash
#
# Fetches the latest Core BPF programs and produces the solana-genesis
# command-line arguments needed to install them
#

set -e

upgradeableLoader=BPFLoaderUpgradeab1e11111111111111111111111

fetch_program() {
  declare name=$1
  declare version=$2
  declare address=$3

  declare so=core_bpf_$name-$version.so

  genesis_args+=(--upgradeable-program "$address" "$upgradeableLoader" "$so" none)

  if [[ -r $so ]]; then
    return
  fi

  if [[ -r ~/.cache/solana-core-bpf/$so ]]; then
    cp ~/.cache/solana-core-bpf/"$so" "$so"
  else
    echo "Downloading $name $version"
    so_name="solana_${name//-/_}_program.so"
    (
      set -x
      curl -L --retry 5 --retry-delay 2 --retry-connrefused \
        -o "$so" \
        "https://github.com/solana-program/$name/releases/download/program%40$version/$so_name"
    )

    mkdir -p ~/.cache/solana-core-bpf
    cp "$so" ~/.cache/solana-core-bpf/"$so"
  fi

}

fetch_program address-lookup-table 3.0.0 AddressLookupTab1e1111111111111111111111111
fetch_program config 3.0.0 Config1111111111111111111111111111111111111
fetch_program feature-gate 0.0.1 Feature111111111111111111111111111111111111

echo "${genesis_args[@]}" > core-bpf-genesis-args.sh

echo
echo "Available SPL programs:"
ls -l core_bpf_*.so

echo
echo "solana-genesis command-line arguments (core-bpf-genesis-args.sh):"
cat core-bpf-genesis-args.sh
