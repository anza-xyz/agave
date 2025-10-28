#!/usr/bin/env bash
#
# This script is used to upload the full buildkite pipeline. The steps defined
# in the buildkite UI should simply be:
#
#   steps:
#    - command: ".buildkite/pipeline-upload.sh"
#

set -e
cd "$(dirname "$0")"/..
source ci/_

if [[ $BUILDKITE_BRANCH == gh-readonly-queue* ]]; then
  # github merge queue
  cat <<EOF | tee /dev/tty | buildkite-agent pipeline upload
priority: 10
steps:
  - name: "sanity"
    command: "ci/docker-run-default-image.sh ci/test-sanity.sh"
    timeout_in_minutes: 5
    agents:
      queue: "check"
  - name: "checks"
    command: "ci/docker-run-default-image.sh ci/test-checks.sh"
    timeout_in_minutes: 30
    agents:
      queue: "check"
EOF

else
  _ ci/buildkite-pipeline.sh pipeline.yml
  echo +++ pipeline
  cat pipeline.yml

  _ buildkite-agent pipeline upload pipeline.yml
fi
