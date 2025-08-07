#!/usr/bin/env bash

cat <<EOF | tee /dev/tty | buildkite-agent pipeline upload
steps:
  - name: "checks"
    command: "ci/docker-run-default-image.sh ci/test-checks.sh"
    timeout_in_minutes: 20
    agents:
      queue: "default"

  - name: "frozen-abi"
    command: "ci/docker-run-default-image.sh ./test-abi.sh"
    timeout_in_minutes: 15
    agents:
      queue: "default"

  - wait

  - group: "stable"
    steps:
      - name: "partitions"
        command: "ci/docker-run-default-image.sh ci/stable/run-partition.sh"
        timeout_in_minutes: 40
        agents:
          queue: "default"
        parallelism: 3
        retry:
          automatic:
            - limit: 3

      - name: "local-cluster"
        command: "ci/docker-run-default-image.sh ci/stable/run-local-cluster-partially.sh"
        timeout_in_minutes: 30
        agents:
          queue: "default"
        parallelism: 4
        retry:
          automatic:
            - limit: 3

      - name: "localnet"
        command: "ci/docker-run-default-image.sh ci/stable/run-localnet.sh"
        timeout_in_minutes: 30
        agents:
          queue: "default"
EOF
