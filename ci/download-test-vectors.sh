#!/usr/bin/env bash
#
# Usage: ci/download-test-vectors.sh [--commit <sha>] [<set>...]
#
# Downloads Firedancer test-vectors fixture sets into fd-vectors/<set>, or
# every set in the release if none are given.

set -euo pipefail

TEST_VECTORS_COMMIT=2cf0d99b744f6560b969b995a65084a41a1ce543
TEST_VECTORS_SHA256SUMS=0062e67a580f554b3c683813d7b794a02c61630b031f1ac958bef2f75e1f49eb

usage() {
  echo "usage: $0 [--commit <sha>] [<set>...]" >&2
  exit 1
}

commit=$TEST_VECTORS_COMMIT
case ${1:-} in
--commit)
  [[ $# -ge 2 ]] || usage
  commit=$2
  shift 2
  ;;
-*) usage ;;
esac

root="$(cd "$(dirname "$0")/.." && pwd)"
dest=$root/fd-vectors
url="https://github.com/firedancer-io/test-vectors/releases/download/${commit::8}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"

curl -fsSL --retry 3 -O "$url/SHA256SUMS"
if [[ $commit == "$TEST_VECTORS_COMMIT" ]]; then
  echo "$TEST_VECTORS_SHA256SUMS  SHA256SUMS" | sha256sum --check
else
  echo "note: $commit is not pinned; fixtures are checked against its own SHA256SUMS only" >&2
fi

if [[ $# -eq 0 ]]; then
  mapfile -t sets < <(awk '$2 ~ /\.tar\.gz$/ { print substr($2, 1, length($2) - 7) }' SHA256SUMS)
else
  sets=("$@")
fi
for set in "${sets[@]}"; do
  if ! awk -v f="$set.tar.gz" '$2 == f { found = 1 } END { exit !found }' SHA256SUMS; then
    echo "error: unknown fixture set: $set" >&2
    exit 1
  fi
done

mkdir -p "$dest"
for set in "${sets[@]}"; do
  archive=$set.tar.gz
  curl -fsSL --retry 3 -O "$url/$archive"
  awk -v f="$archive" '$2 == f' SHA256SUMS | sha256sum --check
  rm -rf "${dest:?}/$set"
  tar -xzf "$archive" -C "$dest"
  rm "$archive"
done
