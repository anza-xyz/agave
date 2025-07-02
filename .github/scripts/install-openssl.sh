#!/usr/bin/env bash

set -e

os_name="$1"

case "$os_name" in
"Windows")
  echo "Downloading OpenSSL installer..."
  curl -L -o "openssl-installer.exe" "https://slproweb.com/download/Win64OpenSSL-3_4_2.exe"
  echo "Installing OpenSSL..."
  cmd.exe /c "openssl-installer.exe /verysilent /sp- /suppressmsgboxes /norestart /DIR=C:\OpenSSL"
  export OPENSSL_LIB_DIR="C:\OpenSSL\lib\VC\x64\MT"
  export OPENSSL_INCLUDE_DIR="C:\OpenSSL\include"
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
