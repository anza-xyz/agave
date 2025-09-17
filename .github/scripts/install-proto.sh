#!/usr/bin/env bash

set -e

os_name="$1"

case "$os_name" in
"Windows")
  choco install protoc
  export PROTOC='C:\ProgramData\chocolatey\lib\protoc\tools\bin\protoc.exe'
  ;;
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
