#!/usr/bin/env bash
#
# This script is used to upload the full buildkite pipeline. The steps defined
# in the buildkite UI should simply be:
#
#   steps:
#    - command: ".buildkite/pipeline-upload.sh"
#

set -e

gitroot=$(git rev-parse --show-toplevel)
"$gitroot"/ci/docker-run-default-image.sh sh -c "cd ci/buildkitegen && go run main.go > pipeline.json"

pipeline_json_path="$gitroot"/ci/buildkitegen/pipeline.json
echo "+++ pipeline"
cat "$pipeline_json_path"
buildkite-agent pipeline upload "$pipeline_json_path"
