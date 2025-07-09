#!/usr/bin/env bash
set -e

usage() {
  cat <<EOF
usage: $0 [major|minor|patch|-preXYZ]

Increments the Cargo.toml version.

Default:
* Removes the prerelease tag if present, otherwise the minor version is incremented.
EOF
  exit 0
}

here="$(dirname "$0")"
cd "$here"/..
source ci/semver_bash/semver.sh
source scripts/read-cargo-variable.sh

ignores=(
  .cache
  .cargo
  target
  node_modules
)

not_paths=()
for ignore in "${ignores[@]}"; do
  not_paths+=(-not -path "*/$ignore/*")
done

# shellcheck disable=2207
Cargo_tomls=($(find . -name Cargo.toml "${not_paths[@]}"))

# Collect the name of all the internal crates
crates=()
for Cargo_toml in "${Cargo_tomls[@]}"; do
  crates+=("$(readCargoVariable name "$Cargo_toml")")
done

# Read the current version
MAJOR=0
MINOR=0
PATCH=0
SPECIAL=""

semverParseInto "$(readCargoVariable version Cargo.toml)" MAJOR MINOR PATCH SPECIAL
[[ -n $MAJOR ]] || usage

currentVersion="$MAJOR\.$MINOR\.$PATCH$SPECIAL"

bump=$1
if [[ -z $bump ]]; then
  if [[ -n $SPECIAL ]]; then
    bump=dropspecial # Remove prerelease tag
  else
    bump=minor
  fi
fi
SPECIAL=""

# Figure out what to increment
case $bump in
patch)
  PATCH=$((PATCH + 1))
  ;;
major)
  MAJOR=$((MAJOR+ 1))
  MINOR=0
  PATCH=0
  ;;
minor)
  MINOR=$((MINOR+ 1))
  PATCH=0
  ;;
dropspecial)
  ;;
check)
  badTomls=()
  for Cargo_toml in "${Cargo_tomls[@]}"; do
    if grep "^version = { workspace = true }" "$Cargo_toml" &>/dev/null; then
      continue
    fi
    if ! grep "^version *= *\"$currentVersion\"$" "$Cargo_toml" &>/dev/null; then
      badTomls+=("$Cargo_toml")
    fi
  done
  if [[ ${#badTomls[@]} -ne 0 ]]; then
    echo "Error: Incorrect crate version specified in: ${badTomls[*]}"
    exit 1
  fi
  exit 0
  ;;
-*)
  if [[ $1 =~ ^-[A-Za-z0-9]*$ ]]; then
    SPECIAL="$1"
  else
    echo "Error: Unsupported characters found in $1"
    exit 1
  fi
  ;;
*)
  echo "Error: unknown argument: $1"
  usage
  ;;
esac

# Version bumps should occur in their own commit. Disallow bumping version
# in dirty working trees. Gate after arg parsing to prevent breaking the
# `check` subcommand.
(
  set +e
  if ! git diff --exit-code; then
    echo -e "\nError: Working tree is dirty. Commit or discard changes before bumping version." 1>&2
    exit 1
  fi
)

newVersion="$MAJOR.$MINOR.$PATCH$SPECIAL"

# Update all the Cargo.toml files
for Cargo_toml in "${Cargo_tomls[@]}"; do
  if ! grep "$currentVersion" "$Cargo_toml"; then
    echo "$Cargo_toml (skipped)"
    continue
  fi

  # Set new crate version
  (
    set -x
    sed -i "$Cargo_toml" -e "s/^version = \"$currentVersion\"$/version = \"$newVersion\"/"
  )

  # Fix up the version references to other internal crates
  for crate in "${crates[@]}"; do
    (
      set -x
      sed -i "$Cargo_toml" -e "
        s/^$crate = { *path *= *\"\([^\"]*\)\" *, *version *= *\"[^\"]*\"\(.*\)} *\$/$crate = \{ path = \"\1\", version = \"=$newVersion\"\2\}/
      "
    )
  done
done

# Update cargo lock files
scripts/cargo-for-all-lock-files.sh tree >/dev/null

# Filter out unrelated changes
# some other dependencies might change after the tree command (like hashbrown)
# the workaround is to filter out unrelated changes then apply the patch
(
  shopt -s globstar
  git diff --unified=0 ./**/Cargo.lock >cargo-lock-patch
  grep -E '^(diff|index|---|\+\+\+|@@.*@@ name = .*|-version|\+version|@@.*@@ dependencies.*|- "agave-|\+ "agave-|- "solana-|\+ "solana-)' cargo-lock-patch >filtered-cargo-lock-patch

  # there might some orphaned dependency lines in the filtered file
  # this is a workaround to filter them out
  # 1. read the filtered file into an array
  # 2. iterate over the array
  # 3. if the line is a dependency line, check the next line to see if it's an orphaned dependency line
  #     a. if it is, add the current line to the file
  #     b. if it's not, don't add the current line to the file
  tmp_file=$(mktemp)
  mapfile -t lines <filtered-cargo-lock-patch
  i=0
  total=${#lines[@]}
  while [ "$i" -lt "$total" ]; do
    line="${lines[$i]}"
    i=$((i + 1))

    # filter out orphaned dependency lines
    if [[ "$line" =~ ^@@.*dependencies ]]; then
      if [ "$i" -lt "$total" ]; then
        next_line="${lines[$i]}"
        if [[ "$next_line" =~ ^[+-] ]]; then
          echo "$line" >>"$tmp_file"
        fi
      fi
    else
      echo "$line" >>"$tmp_file"
    fi
  done
  mv "$tmp_file" filtered-cargo-lock-patch

  git ls-files -- **/Cargo.lock | xargs -I {} git checkout {}
  git apply --unidiff-zero filtered-cargo-lock-patch
  rm cargo-lock-patch filtered-cargo-lock-patch
)

echo "$currentVersion -> $newVersion"

exit 0
