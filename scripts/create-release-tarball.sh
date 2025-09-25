#!/usr/bin/env bash
# create-release-tarball.sh
# Description:
#   Builds binaries into a clean build directory, generates version.yml, and
#   packages the directory into a bzip2-compressed tarball. Supports customizing
#   build directory, channel/tag, target triple, tarball base name, and whether
#   to include validator/operator binaries.

set -euo pipefail

print_usage() {
  cat <<'EOF'
Usage:
  create-release-tarball.sh [options] --target TRIPLE

Options:
  --build-dir DIR           Build directory (default: solana-release)
  --channel-or-tag VAL      Channel or tag to embed in version.yml (default: "unknown")
  --target TRIPLE           Target triple for version.yml and artifact names (required)
  --tarball-basename NAME   Base name for tarball and .yml outputs (default: build-dir)
  --include-val-bins        Include validator/operator binaries in the tarball (default: exclude)
  --help, -h                Show this help and exit
EOF
}

cd "$(dirname "$0")/.."

build_dir="solana-release"
channel_or_tag="unknown"
target=""
tarball_basename=""
include_val_bins=0
commit="$(git rev-parse HEAD)"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --build-dir)
      if [[ -n "$2" ]]; then
        build_dir="$2"
        shift 2
      else
        echo "Error: --build-dir requires a value"
        print_usage
        exit 1
      fi
      ;;
    --channel-or-tag)
      if [[ -n "$2" ]]; then
        channel_or_tag="$2"
        shift 2
      else
        echo "Error: --channel-or-tag requires a value"
        print_usage
        exit 1
      fi
      ;;
    --target)
      if [[ -n "$2" ]]; then
        target="$2"
        shift 2
      else
        echo "Error: --target requires a value"
        print_usage
        exit 1
      fi
      ;;
    --tarball-basename)
      if [[ -n "$2" ]]; then
        tarball_basename="$2"
        shift 2
      else
        echo "Error: --tarball-basename requires a value"
        print_usage
        exit 1
      fi
      ;;
    --include-val-bins)
      include_val_bins=1
      shift
      ;;
    --help|-h)
      print_usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1"
      print_usage
      exit 1
      ;;
  esac
done

if [[ -z "$target" ]]; then
  echo "Error: --target is required"
  print_usage
  exit 1
fi

tarball_basename="${tarball_basename:-${build_dir}}"

rm -rf "${build_dir:?}"/
mkdir -p "${build_dir}"/

cat > "${build_dir}/version.yml" <<EOF
channel: ${channel_or_tag}
commit: ${commit}
target: ${target}
EOF

source ci/rust-version.sh stable
scripts/cargo-install-all.sh stable "${build_dir}"

source scripts/agave-build-lists.sh
tmp_excludes="$(mktemp)"
trap 'rm -f "$tmp_excludes"' EXIT

if [[ "$include_val_bins" -eq 0 ]]; then
  for bin in "${AGAVE_BINS_VAL_OP[@]}"; do
    find "${build_dir}" -type f -name "$bin" -print -quit >> "$tmp_excludes"
  done
fi

output_tar="${tarball_basename}-${target}.tar.bz2"
echo --- Creating tarball
tar -I bzip2 -X "$tmp_excludes" -cvf "$output_tar" "${build_dir}"

echo "Done. Output: $(pwd)/$output_tar"
