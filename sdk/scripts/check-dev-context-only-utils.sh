#!/usr/bin/env bash

set -eo pipefail
here="$(dirname "$0")"
src_root="$(readlink -f "${here}/..")"
cd "${src_root}"

# There's a special common feature called `dev-context-only-utils` to
# overcome cargo's issue: https://github.com/rust-lang/cargo/issues/8379
# This feature is like `cfg(test)`, which works between crates.
#
# Unfortunately, this in turn needs some special checks to avoid common
# pitfalls of `dev-context-only-utils` itself.
#
# Firstly, detect any misuse of dev-context-only-utils as normal/build
# dependencies.  Also, allow some exceptions for special purpose crates. This
# white-listing mechanism can be used for core-development-oriented crates like
# bench bins.
#
# Put differently, use of dev-context-only-utils is forbidden for non-dev
# dependencies in general. However, allow its use for non-dev dependencies only
# if its use is confined under a dep. subgraph with all nodes being marked as
# dev-context-only-utils.

# Add your troubled package which seems to want to use `dev-context-only-utils`
# as normal (not dev) dependencies, only if you're sure that there's good
# reason to bend dev-context-only-utils's original intention and that listed
# package isn't part of released binaries.
query=$(cat <<EOF
.packages
| map(.name as \$crate
  | (.dependencies
    | map(select((.kind // "normal") == "normal"))
    | map({
      "crate" : \$crate,
      "dependency" : .name,
      "dependencyFeatures" : .features,
    })
  )
)
| flatten
| map(select(
  (.dependencyFeatures
    | index("dev-context-only-utils")
  )
))
| map([.crate, .dependency] | join(": "))
| join("\n    ")
EOF
)

abusers="$(./cargo nightly metadata --format-version=1 | jq -r "$query")"
if [[ -n "$abusers" ]]; then
  cat <<EOF 1>&2
\`dev-context-only-utils\` must not be used as normal dependencies, but is by \
"([crate]: [dependency])":
  $abusers
EOF
  exit 1
fi
