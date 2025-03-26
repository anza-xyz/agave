#!/usr/bin/env bash

# input:
#   env:
#     - CRATE_TOKEN
#     - COMMIT_RANGE

if [[ -z $COMMIT_RANGE ]]; then
  echo "COMMIT_RANGE should be provided"
  exit 1
fi

if ! command -v toml &>/dev/null; then
  echo "not found toml-cli"
  cargo install toml-cli
fi

declare skip_patterns=(
  "Cargo.toml"
  "programs/sbf"
)

declare -A verified_crate_owners=(
  ["anza-team"]=1
)

# get Cargo.toml from git diff
readarray -t files <<<"$(git diff "$COMMIT_RANGE" --diff-filter=AM --name-only | grep Cargo.toml)"
printf "%s\n" "${files[@]}"

error_count=0
for file in "${files[@]}"; do
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

  for skip_pattern in "${skip_patterns[@]}"; do
    if [[ $file =~ ^$skip_pattern ]]; then
      echo -e "⏩ skip (match skip patterns)\n"
      continue 2
    fi
  done

  # crates.io will reject publication if certain fields are not populated
  # https://doc.rust-lang.org/cargo/reference/publishing.html#before-publishing-a-new-crate
  IFS=$'\t' read -r lic licf desc home repo < <(toml get "$file" . | jq -r "
    (.package.license | tojson)\
    +\"\t\"+(.package.license_file | tojson)\
    +\"\t\"+(.package.description | tojson)\
    +\"\t\"+(.package.homepage | tojson)\
    +\"\t\"+(.package.repository | tojson)\
  ")

  declare missing_metadata=()
  if [ "$lic" = "null" ] && [ "$licf" = "null" ]; then
    missing_metadata+=( "license" )
  else
    echo "✅ license"
  fi
  if [ "$desc" = "null" ]; then
    missing_metadata+=( "description" )
  else
    echo "✅ description"
  fi
  if [ "$home" = "null" ]; then
    missing_metadata+=( "homepage" )
  else
    echo "✅ homepage"
  fi
  if [ "$repo" = "null" ]; then
    missing_metadata+=( "repository" )
  else
    echo "✅ repository"
  fi

  if [ ${#missing_metadata[@]} -ne 0 ]; then
    echo "❌ $crate_name is missing the following metadata fields: ${missing_metadata[*]}"
    exit 1
  fi

  response=$(curl -s https://crates.io/api/v1/crates/"$crate_name"/owners)
  errors=$(echo "$response" | jq .errors)
  if [[ $errors != "null" ]]; then
    details=$(echo "$response" | jq .errors | jq -r ".[0].detail")
    if [[ $details = *"does not exist"* ]]; then
      ((error_count++))
      echo "❌ new crate $crate_name not found on crates.io. you can either

1. mark it as not for publication in its Cargo.toml

    [package]
    ...
    publish = false

or

2. make a dummy publication.

  example:
  scripts/reserve-cratesio-package-name.sh \
    --token <GRIMES_CRATESIO_TOKEN> \
    lib solana-new-lib-crate

  see also: scripts/reserve-cratesio-package-name.sh --help
"
    else
      ((error_count++))
      echo "❌ $response"
    fi
  else
    readarray -t owners <<<"$(echo "$response" | jq .users | jq -r ".[] | .login")"

    verified_owner_count=0
    unverified_owner_count=0
    for owner in "${owners[@]}"; do
      if [[ -z $owner ]]; then
        continue
      fi
      owner_id="$(echo "$owner" | awk '{print $1}')"
      if [[ ${verified_crate_owners[$owner_id]} ]]; then
        ((verified_owner_count++))
        echo "✅ $owner"
      else
        ((unverified_owner_count++))
        echo "❌ $owner"
      fi
    done

    if [[ ($unverified_owner_count -gt 0) ]]; then
      ((error_count++))
      echo "error: found unverified owner(s)"
    elif [[ ($verified_owner_count -le 0) ]]; then
      ((error_count++))
      echo "error: there are no verified owners"
    fi
  fi
  echo ""

done

if [ "$error_count" -eq 0 ]; then
  echo "success"
  exit 0
else
  exit 1
fi
