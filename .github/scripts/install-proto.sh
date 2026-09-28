#!/usr/bin/env bash

set -e

os_name="$1"

case "$os_name" in
"macOS")
  brew install protobuf
  ;;
"Linux")
  if grep "Alpine" /etc/os-release ; then
    apk add protoc
  fi
  ;;
*)
  echo "Unknown Operating System"
  ;;
esac
