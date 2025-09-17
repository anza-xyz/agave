#!/usr/bin/env bash

set -e

os_name="$1"

case "$os_name" in
"Windows")
  # initialize vcpkg.json. the builtin-baseline is the commit that contains required version.
  # the commit hash can be found in https://github.com/microsoft/vcpkg with `git log --pretty=format:"%H %s" | grep openssl`
  # can be verified with:
  # web:
  #     https://github.com/microsoft/vcpkg/blob/__COMMIT_HASH__/versions/o-/openssl.json
  # command:
  #     git show __COMMIT_HASH__:versions/o-/openssl.json
  cat > vcpkg.json <<EOL
{
  "dependencies": ["openssl"],
  "overrides": [
    {
      "name": "openssl",
      "version": "3.4.1"
    }
  ],
  "builtin-baseline": "5ee5eee0d3e9c6098b24d263e9099edcdcef6631"
}
EOL
  vcpkg install --triplet x64-windows-static-md
  rm vcpkg.json
  export OPENSSL_LIB_DIR="$PWD/vcpkg_installed/x64-windows-static-md/lib"
  export OPENSSL_INCLUDE_DIR="$PWD/vcpkg_installed/x64-windows-static-md/include"
  ;;
"macOS") ;;
"Linux")
  if grep "Alpine" /etc/os-release ; then
    apk add openssl-dev openssl-libs-static
  fi
  ;;
*)
  echo "Unknown Operating System"
  ;;
esac
