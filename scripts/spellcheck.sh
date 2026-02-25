#!/usr/bin/env bash

# Script to run spellcheck on specific crates or directories
# Usage: ./scripts/spellcheck.sh [directory1] [directory2] ...

set -e

# Default to checking a few smaller crates if no arguments provided
if [ $# -eq 0 ]; then
    echo "No directories specified. Running spellcheck on initial test crates..."
    typos --config _typos.toml clap-utils/ clap-v3-utils/ client-test/ --format brief
else
    echo "Running spellcheck on specified directories: $*"
    typos --config _typos.toml "$@" --format brief
fi
