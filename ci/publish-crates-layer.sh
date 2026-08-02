#!/usr/bin/env bash
#
# Publishes a list of workspace crates to crates.io using a trusted-publisher OIDC token.
# Reads crate names from stdin, one per line.
#
# Required env vars:
#   CARGO_REGISTRY_TOKEN  Short-lived crates.io token from rust-lang/crates-io-auth-action
#   CRATE_VERSION         Version expected on crates.io (used for already-published check)
#   DOCKER_IMAGE          CI Docker image (rust + C deps for rocksdb, etc.)

set -euo pipefail
cd "$(dirname "$0")/.."

: "${CARGO_REGISTRY_TOKEN:?required}"
: "${CRATE_VERSION:?required}"
: "${DOCKER_IMAGE:?required}"

is_published() {
  local name=$1 version=$2
  # crates.io API docs require a user-agent header: https://crates.io/data-access#api
  curl -fsSL \
    --user-agent 'Anza (https://github.com/anza-xyz/agave)' \
    "https://crates.io/api/v1/crates/${name}/${version}" 2>/dev/null \
    | jq -e 'has("version")' >/dev/null 2>&1
}

while IFS= read -r crate_name; do
  [[ -z "${crate_name}" ]] && continue

  if is_published "${crate_name}" "${CRATE_VERSION}"; then
    echo "${crate_name} ${CRATE_VERSION} already on crates.io, skipping"
    continue
  fi

  echo "::group::Publish ${crate_name}"
  attempt=0
  max_attempts=5
  while true; do
    attempt=$((attempt + 1))
    if output=$(docker run --rm \
      -v "${PWD}:/solana" -w /solana \
      -e CARGO_REGISTRY_TOKEN \
      "${DOCKER_IMAGE}" \
      cargo publish -p "${crate_name}" --no-verify 2>&1 | tee /dev/fd/2); then
      break
    fi

    if grep -q "already exists on crates.io index" <<< "${output}"; then
      echo "${crate_name} already published on retry, continuing"
      break
    fi

    if [[ ${attempt} -ge ${max_attempts} ]]; then
      echo "::error::Failed to publish ${crate_name} after ${attempt} attempts"
      exit 1
    fi

    # crates.io rate-limits with "Please try again after <RFC1123 timestamp>"; fall back
    # to 60s, matching the 1/min leaky-bucket refill rate.
    retry_after=$(sed -n 's/.*Please try again after \(.*\) or email.*/\1/p' <<< "${output}")
    if [[ -n "${retry_after}" ]]; then
      backoff=$(( $(date -d "${retry_after}" +%s) - $(date +%s) ))
      [[ ${backoff} -gt 0 ]] && sleep "${backoff}"
    else
      sleep 60
    fi
  done
  echo "::endgroup::"
done
