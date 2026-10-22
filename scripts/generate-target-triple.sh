#!/usr/bin/env bash

# |source| this file

_arch="$(uname -m)"
if [[ $_arch = arm64 ]]; then
  _arch=aarch64
fi

case $(uname | tr '[:upper:]' '[:lower:]') in
linux*)
  export BUILD_TARGET_TRIPLE="$_arch-unknown-linux-gnu"
  ;;
darwin*)
  export BUILD_TARGET_TRIPLE="$_arch-apple-darwin"
  ;;
msys*|mingw*)
  export BUILD_TARGET_TRIPLE="$_arch-pc-windows-msvc"
  ;;
*)
  ;;
esac
