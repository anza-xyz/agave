#!/usr/bin/env bash
#
# Fetches the latest SPL programs and produces the solana-genesis command-line
# arguments needed to install them
#

set -e

upgradeableLoader=BPFLoaderUpgradeab1e11111111111111111111111

fetch_program() {
  declare prefix=$1
  declare name=$2
  declare version=$3
  declare address=$4
  declare loader=$5
  declare download_url=$6

  declare so=$prefix-$name-$version.so

  if [[ $loader == "$upgradeableLoader" ]]; then
    genesis_args+=(--upgradeable-program "$address" "$loader" "$so" none)
  else
    genesis_args+=(--bpf-program "$address" "$loader" "$so")
  fi

  if [[ -r $so ]]; then
    return
  fi

  if [[ -r ~/.cache/solana-$prefix/$so ]]; then
    cp ~/.cache/solana-"$prefix"/"$so" "$so"
  else
    echo "Downloading $name $version"
    (
      set -x
      curl -L --retry 5 --retry-delay 2 --retry-connrefused -o "$so" "$download_url"
    )

    mkdir -p ~/.cache/solana-"$prefix"
    cp "$so" ~/.cache/solana-"$prefix"/"$so"
  fi

}

PREFIX="spl"

fetch_spl_program() {
  declare name=$1
  declare version=$2
  declare address=$3
  declare loader=$4
  
  so_name="${PREFIX}_${name//-/_}.so"
  download_url="https://github.com/solana-labs/solana-program-library/releases/download/$name-v$version/$so_name"
  
  fetch_program "$PREFIX" "$name" "$version" "$address" "$loader" "$download_url"
}

fetch_spl_program token 3.5.0 TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA BPFLoader2111111111111111111111111111111111
fetch_spl_program token-2022 5.0.2 TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb BPFLoaderUpgradeab1e11111111111111111111111
fetch_spl_program memo  1.0.0 Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo BPFLoader1111111111111111111111111111111111
fetch_spl_program memo  3.0.0 MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr BPFLoader2111111111111111111111111111111111
fetch_spl_program associated-token-account 1.1.2 ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL BPFLoader2111111111111111111111111111111111
fetch_spl_program feature-proposal 1.0.0 Feat1YXHhH6t1juaWF74WLcfv4XoNocjXA6sPWHNgAse BPFLoader2111111111111111111111111111111111

echo "${genesis_args[@]}" > $PREFIX-genesis-args.sh

echo
echo "Available $PREFIX programs:"
ls -l "$PREFIX"-*.so

echo
echo "solana-genesis command-line arguments ($PREFIX-genesis-args.sh):"
cat $PREFIX-genesis-args.sh
