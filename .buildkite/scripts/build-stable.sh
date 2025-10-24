#!/usr/bin/env bash

set -e
here=$(dirname "$0")

# shellcheck source=.buildkite/scripts/common.sh
source "$here"/common.sh

agent="${1-solana}"

parallelism=5
partitions=()
for i in $(seq 1 $parallelism); do
  partitions+=("$(
    cat <<EOF
{
  "name": "partition-$i",
  "command": "ci/docker-run-default-image.sh ci/stable/run-partition.sh $i $parallelism",
  "timeout_in_minutes": 25,
  "agent": "$agent",
  "retry": 3
}
EOF
  )")
done

# add dev-bins
partitions+=(
  "$(
    cat <<EOF
{
  "name": "dev-bins",
  "command": "ci/docker-run-default-image.sh cargo nextest run --profile ci --manifest-path ./dev-bins/Cargo.toml",
  "timeout_in_minutes": 35,
  "agent": "$agent"
}
EOF
  )")

parallelism=10
local_cluster_partitions=()
for i in $(seq 1 $parallelism); do
  local_cluster_partitions+=("$(
    cat <<EOF
{
  "name": "local-cluster-$i",
  "command": "ci/docker-run-default-image.sh ci/stable/run-local-cluster-partially.sh $i $parallelism",
  "timeout_in_minutes": 15,
  "agent": "$agent",
  "retry": 3
}
EOF
  )")
done

localnet=$(
  cat <<EOF
{
  "name": "localnet",
  "command": "ci/docker-run-default-image.sh ci/stable/run-localnet.sh",
  "timeout_in_minutes": 30,
  "agent": "$agent"
}
EOF
)

# shellcheck disable=SC2016
group "stable" "${partitions[@]}"
group "local-cluster" "${local_cluster_partitions[@]}"
group "localnet" "$localnet"
