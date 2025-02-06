#!/usr/bin/env bash

# input:
#   env:
#     - COMMIT_RANGE

set -eo pipefail
here="$(dirname "$0")"
src_root="$(readlink -f "${here}/..")"
cd "${src_root}"

if [[ -z $COMMIT_RANGE ]]; then
  echo "COMMIT_RANGE should be provided"
  exit 1
fi

declare -A verified_crate_owners=(
  ["anza-team"]=1
)

# get Cargo.toml from git diff
# NOTE: update this to remove the "sdk" portion when moving to a new repo
readarray -t files <<<"$(git diff "$COMMIT_RANGE" --diff-filter=AM --name-only | grep Cargo.toml | grep "^sdk/" | sed 's#sdk/##')"
printf "%s\n" "${files[@]}"

has_error=0
for file in "${files[@]}"; do
  if [ -z "$file" ]; then
    continue
  fi
  read -r crate_name package_publish workspace < <(toml get "$file" . | jq -r '(.package.name | tostring)+" "+(.package.publish | tostring)+" "+(.workspace | tostring)')
  echo "=== $crate_name ($file) ==="

  if [[ $package_publish = 'false' ]]; then
    echo -e "⏩ skip (package_publish: $package_publish)\n"
    continue
  fi

  if [[ "$workspace" != "null" ]]; then
    echo -e "⏩ skip (is a workspace root)\n"
    continue
  fi

  response=$(curl -s https://crates.io/api/v1/crates/"$crate_name"/owners)
  errors=$(echo "$response" | jq .errors)
  if [[ $errors != "null" ]]; then
    details=$(echo "$response" | jq .errors | jq -r ".[0].detail")
    if [[ $details = *"does not exist"* ]]; then
      has_error=1
      echo "❌ new crate $crate_name not found on crates.io. you can either

1. mark it as not for publication in its Cargo.toml

    [package]
    ...
    publish = false

or

2. make a dummy publication and assign it to anza-team

  example:
  scripts/reserve-cratesio-package-name.sh \
    --token <CRATESIO_TOKEN> \
    lib solana-new-lib-crate

  see also: scripts/reserve-cratesio-package-name.sh --help
"
    else
      has_error=1
      echo "❌ $response"
    fi
  else
    readarray -t owners <<<"$(echo "$response" | jq .users | jq -r ".[] | .login")"

    has_verified_owner=0
    has_unverified_owner=0
    for owner in "${owners[@]}"; do
      if [[ -z $owner ]]; then
        continue
      fi
      owner_id="$(echo "$owner" | awk '{print $1}')"
      if [[ ${verified_crate_owners[$owner_id]} ]]; then
        has_verified_owner=1
        echo "✅ $owner"
      else
        has_unverified_owner=1
        echo "❌ $owner"
      fi
    done

    if [[ ($has_unverified_owner -gt 0) ]]; then
      has_error=1
      echo "error: found unverified owner(s)"
    elif [[ ($has_verified_owner -le 0) ]]; then
      has_error=1
      echo "error: there are no verified owners"
    fi
  fi
  echo ""
done

if [ "$has_error" -eq 0 ]; then
  echo "success"
  exit 0
else
  exit 1
fi
