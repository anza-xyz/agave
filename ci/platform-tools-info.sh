#!/usr/bin/env bash
#
# Finds the version of platform-tools used by this source tree.
#
# stdout of this script may be eval-ed.
#

here="$(dirname "$0")"

PLATFORM_TOOLS_VERSION=unknown

cargo_build_sbf_main="${here}/../cargo-build-sbf"
version=$(${cargo_build_sbf_main} --version | grep platform-tools | sed 's/^platform-tools[[:space:]]*//')
if [[ ${version} != '' ]]; then
    PLATFORM_TOOLS_VERSION="${version}"
else
    echo '--- unable to parse PLATFORM_TOOLS_VERSION'
fi

echo PLATFORM_TOOLS_VERSION="${PLATFORM_TOOLS_VERSION}"
