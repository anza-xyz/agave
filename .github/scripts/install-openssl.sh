#!/usr/bin/env bash

set -e

os_name="$1"

case "$os_name" in
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
