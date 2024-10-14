#!/usr/bin/env bash
"$(dirname "${BASH_SOURCE[0]}")"/stop.sh
rm -rf "$(dirname "${BASH_SOURCE[0]}")"/lib
"$(dirname "${BASH_SOURCE[0]}")"/start.sh