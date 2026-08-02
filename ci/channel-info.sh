#!/usr/bin/env bash
#
# Computes the current branch names of the edge, beta and stable
# channels, as well as the latest tagged release for beta and stable.
#
# stdout of this script may be eval-ed
#

here="$(dirname "$0")"

# shellcheck source=ci/semver_bash/semver.sh
source "$here"/semver_bash/semver.sh

remote=https://github.com/anza-xyz/agave.git

# Fetch all vX.Y.Z tags
#
# NOTE: pre-release tags are explicitly ignored
#
# shellcheck disable=SC2207
tags=( \
  $(git ls-remote --tags $remote \
    | cut -c52- \
    | grep '^v[[:digit:]][[:digit:]]*\.[[:digit:]][[:digit:]]*.[[:digit:]][[:digit:]]*$' \
    | cut -c2- \
  ) \
)

# Fetch all the vX.Y branches
#
# shellcheck disable=SC2207
heads=( \
  $(git ls-remote --heads $remote \
    | cut -c53- \
    | grep '^v[[:digit:]][[:digit:]]*\.[[:digit:]][[:digit:]]*$' \
    | cut -c2- \
  ) \
)

# Figure the beta channel by looking for the largest vX.Y branch
beta=
for head in "${heads[@]}"; do
  if [[ -n $beta ]]; then
    if semverLT "$head.0" "$beta.0"; then
      continue
    fi
  fi
  beta=$head
done

# Figure the stable channel by looking for the second largest vX.Y branch
stable=
for head in "${heads[@]}"; do
  if [[ $head = "$beta" ]]; then
    continue
  fi
  if [[ -n $stable ]]; then
    if semverLT "$head.0" "$stable.0"; then
      continue
    fi
  fi
  stable=$head
done

# Apply channel pin overrides fetched from master. Single source of truth:
# every branch's CI honors the same file. Hard-fail on transport errors so
# misconfiguration surfaces immediately. Parse strictly with awk to avoid
# arbitrary execution.
overrides_url="https://raw.githubusercontent.com/anza-xyz/agave/master/ci/channel-overrides?ts=$(date +%s)"
overrides=$(curl -fsSL --retry 5 --retry-delay 2 --retry-connrefused "$overrides_url") || {
  echo "error: failed to fetch channel overrides from $overrides_url" >&2
  exit 1
}
extract_pin() {
  awk -F= -v k="$1" '
    $1==k && $2 ~ /^(v[0-9a-z.]+)?$/ { print $2; exit }
  ' <<<"$overrides"
}
beta_pin=$(extract_pin PINNED_BETA_CHANNEL)
stable_pin=$(extract_pin PINNED_STABLE_CHANNEL)
[[ -n $beta_pin ]]   && beta=${beta_pin#v}
[[ -n $stable_pin ]] && stable=${stable_pin#v}

for tag in "${tags[@]}"; do
  if [[ -n $beta && $tag = $beta* ]]; then
    if [[ -n $beta_tag ]]; then
      if semverLT "$tag" "$beta_tag"; then
        continue
      fi
    fi
    beta_tag=$tag
  fi

  if [[ -n $stable && $tag = $stable* ]]; then
    if [[ -n $stable_tag ]]; then
      if semverLT "$tag" "$stable_tag"; then
        continue
      fi
    fi
    stable_tag=$tag
  fi
done

EDGE_CHANNEL=master
BETA_CHANNEL=${beta:+v$beta}
STABLE_CHANNEL=${stable:+v$stable}
BETA_CHANNEL_LATEST_TAG=${beta_tag:+v$beta_tag}
STABLE_CHANNEL_LATEST_TAG=${stable_tag:+v$stable_tag}


if [[ -n $CI_BASE_BRANCH ]]; then
  BRANCH="$CI_BASE_BRANCH"
elif [[ -n $CI_BRANCH ]]; then
  BRANCH="$CI_BRANCH"
fi

if [[ -z "$CHANNEL" ]]; then
  if [[ $BRANCH = "$STABLE_CHANNEL" ]]; then
    CHANNEL=stable
  elif [[ $BRANCH = "$EDGE_CHANNEL" ]]; then
    CHANNEL=edge
  elif [[ $BRANCH = "$BETA_CHANNEL" ]]; then
    CHANNEL=beta
  fi
fi

if [[ $CHANNEL = beta ]]; then
  CHANNEL_LATEST_TAG="$BETA_CHANNEL_LATEST_TAG"
elif [[ $CHANNEL = stable ]]; then
  CHANNEL_LATEST_TAG="$STABLE_CHANNEL_LATEST_TAG"
fi

echo EDGE_CHANNEL="$EDGE_CHANNEL"
echo BETA_CHANNEL="$BETA_CHANNEL"
echo BETA_CHANNEL_LATEST_TAG="$BETA_CHANNEL_LATEST_TAG"
echo STABLE_CHANNEL="$STABLE_CHANNEL"
echo STABLE_CHANNEL_LATEST_TAG="$STABLE_CHANNEL_LATEST_TAG"
echo CHANNEL="$CHANNEL"
echo CHANNEL_LATEST_TAG="$CHANNEL_LATEST_TAG"

exit 0
