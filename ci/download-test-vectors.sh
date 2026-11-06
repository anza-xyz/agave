#!/usr/bin/env bash
#
# Usage: ci/download-test-vectors.sh [--commit <sha>] [<set>...]
#
# Downloads Firedancer test-vectors fixture sets into fd-vectors/<set>, or
# every set in the release if none are given.
# Uses the per-set release pins unless --commit overrides them.

set -euo pipefail

TEST_VECTORS_COMMIT=0a73bfe28040da825fb88fa79bb132a561cf440f
TEST_VECTORS_SHA256SUMS=1c35f9af714e4885d1a5d60c47bfacc7ff1b8a1b2006f445b104b4ee06b57872

# Gossip deprecation fixtures from test-vectors #287. Other updated sets in
# this release depend on the vote-program changes in Agave #16828.
GOSSIP_TEST_VECTORS_COMMIT=edf867a2ec172c7d2a4718f9fafdea1cd57fa5e2
GOSSIP_TEST_VECTORS_SHA256SUMS=d1af986de97c5cf41ecfcbd7b62922bfff9b7d6385a664b83fc60d0ae24195b5

usage() {
  echo "usage: $0 [--commit <sha>] [<set>...]" >&2
  exit 1
}

commit=$TEST_VECTORS_COMMIT
gossip_commit=$GOSSIP_TEST_VECTORS_COMMIT
case ${1:-} in
--commit)
  [[ $# -ge 2 ]] || usage
  commit=$2
  gossip_commit=$commit
  shift 2
  ;;
-*) usage ;;
esac

if [[ $# -eq 1 && $1 == gossip ]]; then
  commit=$gossip_commit
fi

root="$(cd "$(dirname "$0")/.." && pwd)"
dest=$root/fd-vectors
url="https://github.com/firedancer-io/test-vectors/releases/download/${commit::8}"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cd "$tmp"

curl -fsSL --retry 3 -O "$url/SHA256SUMS"
if [[ $commit == "$TEST_VECTORS_COMMIT" ]]; then
  echo "$TEST_VECTORS_SHA256SUMS  SHA256SUMS" | sha256sum --check
elif [[ $commit == "$GOSSIP_TEST_VECTORS_COMMIT" ]]; then
  echo "$GOSSIP_TEST_VECTORS_SHA256SUMS  SHA256SUMS" | sha256sum --check
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
  if [[ $set == gossip && $commit != "$gossip_commit" ]]; then
    "$root/ci/download-test-vectors.sh" --commit "$gossip_commit" gossip
    continue
  fi
  archive=$set.tar.gz
  curl -fsSL --retry 3 -O "$url/$archive"
  awk -v f="$archive" '$2 == f' SHA256SUMS | sha256sum --check
  rm -rf "${dest:?}/$set"
  tar -xzf "$archive" -C "$dest"
  rm "$archive"
done
