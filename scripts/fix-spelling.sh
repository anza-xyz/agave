#!/usr/bin/env bash
# Fix spelling errors in the codebase using typos

set -e

cd "$(dirname "$0")/.."

echo "Running typos to fix spelling errors..."
typos --write-changes --config typos.toml

echo "Spelling fixes complete!"

