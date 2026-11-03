#!/usr/bin/env bash
#
# Usage: ci/download-test-vectors.sh [--commit <sha>] [<set>...]
#
# Downloads Firedancer test-vectors fixture sets into fd-vectors/<set>, or
# every set in the release if none are given.

set -euo pipefail

TEST_VECTORS_COMMIT=b464d548e855f6914569ada419d0ca4ba4188fec
TEST_VECTORS_SHA256SUMS=7557a1bb13732d8e11109d162999b005623ea9388405feb9548b50cac80b3096

# The newer shred fixtures require an Alpenglow-aware harness, and the txn
# fixtures require SIMD-0602 (https://github.com/anza-xyz/agave/pull/16338).
# Keep these sets on their last compatible release until those changes land.
COMPAT_TEST_VECTORS_COMMIT=b95ca35cbdde488e80a71ebb25b628c1bd62e178
COMPAT_TEST_VECTORS_SHA256SUMS=c7e399616095c3aadaba8175572f058326a9be135aed5941885323b96530e4b7

usage() {
  echo "usage: $0 [--commit <sha>] [<set>...]" >&2
  exit 1
}

commit=$TEST_VECTORS_COMMIT
use_compat_pins=true
case ${1:-} in
--commit)
  [[ $# -ge 2 ]] || usage
  commit=$2
  use_compat_pins=false
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
  archive_url=$url
  checksums=SHA256SUMS
  if [[ $use_compat_pins == true && ( $set == txn || $set == shred ) ]]; then
    archive_url="https://github.com/firedancer-io/test-vectors/releases/download/${COMPAT_TEST_VECTORS_COMMIT::8}"
    checksums=COMPAT_SHA256SUMS
    if [[ ! -f $checksums ]]; then
      curl -fsSL --retry 3 -o "$checksums" "$archive_url/SHA256SUMS"
      echo "$COMPAT_TEST_VECTORS_SHA256SUMS  $checksums" | sha256sum --check
    fi
  fi
  curl -fsSL --retry 3 -O "$archive_url/$archive"
  awk -v f="$archive" '$2 == f' "$checksums" | sha256sum --check
  rm -rf "${dest:?}/$set"
  tar -xzf "$archive" -C "$dest"
  rm "$archive"
done
