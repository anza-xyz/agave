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

# Divide the thread budget among jobs sharing the host. Defaults to 1 (no-op)
# until an agent sets CI_HOST_SLOTS.
: "${CI_HOST_SLOTS:=1}"
if ((CI_HOST_SLOTS > 1)); then
  SYS_JOBS=$(((SYS_JOBS + CI_HOST_SLOTS - 1) / CI_HOST_SLOTS))
  ((SYS_JOBS < 1)) && SYS_JOBS=1
fi

: "${JOBS:=$SYS_JOBS}"

export NPROC
export JOBS
