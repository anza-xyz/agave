#!/usr/bin/env bash

# shellcheck disable=SC2317
cleanup() {
  ec=$?
  docker container stop kellnr || true
  docker container prune -f
  exit "$ec"
}

err_handler() {
    ec=$?
    echo "ERROR  line $1: $BASH_COMMAND"
    exit "$ec"
}

trap cleanup EXIT
trap err_handler ERR SIGINT

set -Eeuo pipefail

cd "$(dirname "$0")/.."
source ci/semver_bash/semver.sh
source ci/rust-version.sh stable

DRY_RUN=false
if [[ $1 = --dry-run ]]; then
  DRY_RUN=true
  export CRATE_PUBLISH_TEST=true
  shift
fi

# shellcheck disable=SC2086
is_crate_version_uploaded() {
  name=$1
  version=$2
  if $DRY_RUN; then
    curl http://127.0.0.1:8000/api/v1/crates/${name}/crate_versions | \
    python3 -c "import sys,json; print(len(json.load(sys.stdin)['versions']) > 0);"
  else
    curl https://crates.io/api/v1/crates/${name}/${version} | \
    python3 -c "import sys,json; print('version' in json.load(sys.stdin));"
  fi
}

# Only package/publish if this is a tagged release
[[ -n $CI_TAG ]] || {
if $DRY_RUN; then
  CI_TAG=$(grep '^version = "' Cargo.toml | cut -d "=" -f2 | xargs)
  CRATES_IO_TOKEN="test"
else
  echo CI_TAG unset, skipped
  exit 0
fi
}

semverParseInto "$CI_TAG" MAJOR MINOR PATCH SPECIAL
expectedCrateVersion="$MAJOR.$MINOR.$PATCH$SPECIAL"

[[ -n "$CRATES_IO_TOKEN" ]] || {
  echo CRATES_IO_TOKEN undefined
  exit 1
}

# check workspace.version for worksapce root
workspace_cargo_tomls=(Cargo.toml programs/sbf/Cargo.toml)
for cargo_toml in "${workspace_cargo_tomls[@]}"; do
  if ! grep -q "^version = \"$expectedCrateVersion\"$" "$cargo_toml"; then
    echo "Error: Cargo.toml version is not $expectedCrateVersion"
    exit 1
  fi
done

Cargo_tomls=$(ci/order-crates-for-publishing.py)

if $DRY_RUN; then
  docker run -p 8000:8000 --name kellnr -d ghcr.io/kellnr/kellnr:5
fi

sleep 5

for Cargo_toml in $Cargo_tomls; do
  echo "--- $Cargo_toml"

  # check the version which doesn't inherit from workspace
  if ! grep -q "^version = { workspace = true }$" "$Cargo_toml"; then
    echo "Warn: $Cargo_toml doesn't use the inherited version"
    grep -q "^version = \"$expectedCrateVersion\"$" "$Cargo_toml" || {
      echo "Error: $Cargo_toml version is not $expectedCrateVersion"
      exit 1
    }
  fi

  crate_name=$(grep -m 1 '^name = ' "$Cargo_toml" | cut -f 3 -d ' ' | tr -d \")

  if grep -q "^publish = false" "$Cargo_toml"; then
    echo "$crate_name is marked as unpublishable"
    continue
  fi

  if [[ $(is_crate_version_uploaded "$crate_name" "$expectedCrateVersion") = True ]] ; then
    echo "${crate_name} version ${expectedCrateVersion} is already on crates.io"
    continue
  fi

  (
    crate=$(dirname "$Cargo_toml")
    if $DRY_RUN; then
      ci/change-crate-deps.py "$Cargo_toml" "$crate_name"

      # token is a default value from the kellnr image https://kellnr.io/documentation#config-values
      # registry value is defined in docker-run.sh script
      cargoCommand="cargo publish --registry kellnr --token Zy9HhJ02RJmg0GCrgLfaCVfU6IwDfhXD --allow-dirty"
    else
      cargoCommand="cargo publish --token $CRATES_IO_TOKEN"
    fi

    numRetries=10
    for ((i = 1; i <= numRetries; i++)); do
      echo "Attempt ${i} of ${numRetries}"
      # The rocksdb package does not build with the stock rust docker image so use
      # the solana rust docker image
      if ci/docker-run-default-image.sh bash -exc "cd $crate; $cargoCommand"; then
        break
      fi

      if [ "$i" -lt "$numRetries" ]; then
        sleep 3
      else
        echo "couldn't publish '$crate_name'"
        exit 1
      fi
    done
  )
done

exit 0
