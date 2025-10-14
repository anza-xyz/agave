# |source| me

upload-ci-artifact() {
  echo "--- artifact: $1"
  if [[ -r "$1" ]]; then
    ls -l "$1"
    if ${BUILDKITE:-false}; then
      (
        set -x
        buildkite-agent artifact upload "$1"
      )
    fi
  else
    echo ^^^ +++
    echo "$1 not found"
  fi
}

upload-gcs-artifact() {
  echo "--- artifact: $1 to $2"
  docker run --rm \
    -v "$GCS_RELEASE_BUCKET_WRITER_CREDIENTIAL:/application_default_credentials.json" \
    -v "$PWD:/solana" \
    -e CLOUDSDK_AUTH_CREDENTIAL_FILE_OVERRIDE=/application_default_credentials.json \
    gcr.io/google.com/cloudsdktool/google-cloud-cli:latest \
    gcloud storage cp "$1" "$2"
}
