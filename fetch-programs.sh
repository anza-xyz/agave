# Source this file.
#
# Fetches on-chain programs and produces the solana-genesis command-line
# arguments needed to install them
#

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

fetch_programs() {
  declare prefix=$1
  shift

  declare -a programs=("$@")

  for program in "${programs[@]}"; do
    # shellcheck disable=SC2086
    fetch_program "$prefix" $program
  done

  echo "${genesis_args[@]}" > "$prefix"-genesis-args.sh

  echo
  echo "Available $prefix programs:"
  ls -l "$prefix"-*.so

  echo
  echo "solana-genesis command-line arguments ($prefix-genesis-args.sh):"
  cat "$prefix"-genesis-args.sh
}
