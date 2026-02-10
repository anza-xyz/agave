#!/usr/bin/env bash

set -e

# limit jobs to 4gb/thread
if [[ -f "/proc/meminfo" ]]; then
  SYS_JOBS=$(grep MemTotal /proc/meminfo | awk '{printf "%.0f", ($2 / (4 * 1024 * 1024))}')
else
  SYS_JOBS=$(sysctl hw.memsize | awk '{printf "%.0f", ($2 / (4 * 1024**3))}')
fi

NPROC=$(nproc)
SYS_JOBS=$((SYS_JOBS > NPROC ? NPROC : SYS_JOBS))
: "${JOBS:=$SYS_JOBS}"

export NPROC
export JOBS
