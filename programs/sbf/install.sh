#!/usr/bin/env bash

unameOut="$(uname -s)"
case "${unameOut}" in
  Linux*)
    machine=linux;;
  Darwin*)
    machine=osx;;
  MINGW*)
    machine=windows;;
  *)
    machine=linux
esac
unameOut="$(uname -m)"
case "${unameOut}" in
  arm64*)
    arch=aarch64;;
  *)
    arch=x86_64
esac

sha256_of() {
  if hash sha256sum 2>/dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  elif hash shasum 2>/dev/null; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    echo "Error: Neither sha256sum nor shasum were found" >&2
    return 1
  fi
}

download() {
  declare url="$1/$2/$3"
  declare filename=$3
  declare sha256=$4
  declare wget_args=(
    "$url" -O "$filename"
    "--progress=dot:giga"
    "--retry-connrefused"
    "--read-timeout=30"
  )
  declare curl_args=(
    -L "$url" -o "$filename"
  )
  if hash wget 2>/dev/null; then
    wget_or_curl="wget ${wget_args[*]}"
  elif hash curl 2>/dev/null; then
    wget_or_curl="curl ${curl_args[*]}"
  else
    echo "Error: Neither curl nor wget were found" >&2
    return 1
  fi

  set -x
  if $wget_or_curl; then
    { set +x; } 2>/dev/null
    declare actual
    actual=$(sha256_of "$filename") || return 1
    if [[ $actual != "$sha256" ]]; then
      echo "Error: $filename sha256 mismatch: expected $sha256, got $actual" >&2
      rm -f "$filename"
      return 1
    fi
    set -x
    tar --strip-components 1 -jxf "$filename" || return 1
    { set +x; } 2>/dev/null
    rm -rf "$filename"
    return 0
  fi
  return 1
}

get() {
  declare version=$1
  declare dirname=$2
  declare sha256=$3
  declare job=$4
  declare cache_root=~/.cache/solana
  declare cache_dirname="$cache_root/$version/$dirname"
  declare cache_partial_dirname="$cache_dirname"_partial
  # Records the archive digest a cache entry was extracted from; entries
  # without a matching marker are treated as unverified and re-fetched.
  declare marker=.install-sh-sha256

  if [[ -r $cache_dirname/$marker && $(<"$cache_dirname/$marker") == "$sha256" ]]; then
    return 0
  fi

  rm -rf "$cache_partial_dirname" || return 1
  mkdir -p "$cache_partial_dirname" || return 1
  pushd "$cache_partial_dirname"

  if $job; then
    echo "$sha256" > "$marker" || return 1
    popd
    rm -rf "$cache_dirname" || return 1
    mv "$cache_partial_dirname" "$cache_dirname" || return 1
    return 0
  fi
  popd
  return 1
}

# Install platform tools
tools_version=v1.57
rust_version=1.95.0
case ${machine}-${arch} in
  linux-x86_64)   tools_sha256=b0f7af104adf726fff2a6a09ea2eb2f2d2965c92295f4d7388c08d140e0c2b00;;
  linux-aarch64)  tools_sha256=f2f31c9325cb2d00244df23b4a1a0904e51819cec49d39235a3ed3ee2320375d;;
  osx-x86_64)     tools_sha256=e6f6231b149e64ad6cc12605d0f993405ce55862aa44406c023563159f8c3caf;;
  osx-aarch64)    tools_sha256=48c32c2ec3ac3729b5caf1fdd6c4145496125edf043b2662b0586bfbc932b34a;;
  windows-x86_64) tools_sha256=792bc821f006e2b56aee31640110b449d5dc89f1305f16f24037465ab8cd7d5a;;
  *)
    echo "Error: no pinned platform-tools digest for ${machine}-${arch}" >&2
    exit 1
esac
tools_folder="$HOME"/.cache/solana
if [[ ! -e platform-tools-$tools_version.md || ! -e platform-tools ]]; then
  (
    set -e
    rm -rf platform-tools*
    job="download \
           https://github.com/anza-xyz/platform-tools/releases/download \
           $tools_version \
           platform-tools-${machine}-${arch}.tar.bz2 \
           $tools_sha256"
    get "$tools_version" platform-tools "$tools_sha256" "$job"
  )
  exitcode=$?
  if [[ $exitcode -ne 0 ]]; then
    exit 1
  fi

  set -ex
  rust_folder="${tools_folder}"/"${tools_version}"/platform-tools/rust

  "${rust_folder}"/bin/rustc --version
  "${rust_folder}"/bin/rustc --print sysroot

  if [[ "${BASH_VERSINFO[0]}" -lt 4 ]]; then
    # MacOS has an outdated version of bash and does not support the safer 'mapfile' alternative
    toolchains=()
    while IFS='' read -r line; do toolchains+=("$line"); done < <(rustup toolchain list)
  else
    mapfile -t toolchains < <(rustup toolchain list)
  fi

  set +e
  for item in "${toolchains[@]}"
  do
    if [[ $item == *"solana"* ]]; then
      rustup toolchain uninstall "$item"
    fi
  done
  set -e

  rustup toolchain link "$rust_version-sbpf-solana-$tools_version" "${rust_folder}"
fi

exit 0
