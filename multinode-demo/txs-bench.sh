#!/usr/bin/env bash
set -e

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=multinode-demo/common.sh
source "$here"/common.sh

program="${solana_transaction_bench}"
default_staked_identity_file_count=1

usage() {
  if [[ -n $1 ]]; then
    echo "$*"
    echo
  fi
  cat <<EOF
usage: $0 [extra args]

 Run solana-transaction-bench against the local multinode demo.

   extra args: additional arguments are passed along to solana-transaction-bench.

 Defaults:
   --url http://127.0.0.1:8899
   --authority ${SOLANA_CONFIG_DIR}/faucet.json
   run
   --num-payers 1024
   --payer-account-balance 10SOL
   --duration 90
   --bind 127.0.0.1:0
   --num-max-open-connections 8
   --workers-pull-size 8
   --lamports-to-transfer 65536
   --num-send-instructions-per-tx 1
   --send-fanout 1
   --staked-identity-file ${SOLANA_CONFIG_DIR}/bootstrap-validator/identity.json
     (repeated ${default_staked_identity_file_count} times)
   --transfer-tx-cu-budget 400
   pinned-leader-tracker <TPU from getClusterNodes tpuQuic>

 Examples:
   $0 --duration 30 --target-tps 5000
   $0 --num-payers 1024 --send-fanout 2 ws-leader-tracker

EOF
  exit 1
}

contains_arg() {
  declare name=$1
  shift

  for arg in "$@"; do
    if [[ $arg = "$name" || $arg = "$name="* ]]; then
      return 0
    fi
  done

  return 1
}

contains_url_arg() {
  for arg in "$@"; do
    if [[ $arg = --url || $arg = --url=* || $arg = -u || $arg = -u?* ]]; then
      return 0
    fi
  done

  return 1
}

add_default_arg() {
  declare -n target_args=$1
  declare name=$2
  declare value=$3

  if ! contains_arg "$name" "${target_args[@]}"; then
    target_args+=("$name" "$value")
  fi
}

get_rpc_url() {
  declare arg
  declare -i expect_value=0

  for arg in "${global_args[@]}"; do
    if ((expect_value)); then
      printf '%s' "$arg"
      return 0
    fi
    case "$arg" in
    --url=*)
      printf '%s' "${arg#--url=}"
      return 0
      ;;
    -u?*)
      printf '%s' "${arg#-u}"
      return 0
      ;;
    --url | -u)
      expect_value=1
      ;;
    esac
  done

  printf '%s' 'http://127.0.0.1:8899'
}

# Query getClusterNodes over RPC and return host:port for the bootstrap validator's
# QUIC TPU (tpuQuic), falling back to UDP tpu. Requires curl and jq.
resolve_pinned_leader_target() {
  declare rpc_url=$1
  declare identity="${SOLANA_CONFIG_DIR}/bootstrap-validator/identity.json"
  declare identity_pubkey=
  declare nodes_json tpu

  if [[ -f $identity ]]; then
    identity_pubkey=$($solana_keygen pubkey "$identity" 2> /dev/null) || identity_pubkey=
  fi

  if ! command -v jq > /dev/null 2>&1; then
    echo "$0: jq is required to parse getClusterNodes (e.g. brew install jq)" >&2
    return 1
  fi

  if ! nodes_json=$(curl -sf -X POST -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"getClusterNodes"}' \
    "$rpc_url"); then
    echo "$0: getClusterNodes request to $rpc_url failed (is the validator RPC up?)" >&2
    return 1
  fi

  if [[ -n $identity_pubkey ]]; then
    tpu=$(echo "$nodes_json" | jq -r --arg pk "$identity_pubkey" '
      [.result[] | select(.pubkey == $pk) | .tpuQuic // .tpu // empty][0] // empty
    ')
  fi

  if [[ -z $tpu || $tpu = null ]]; then
    tpu=$(echo "$nodes_json" | jq -r '
      [.result[] | .tpuQuic // .tpu // empty][0] // empty
    ')
  fi

  if [[ -z $tpu || $tpu = null ]]; then
    echo "$0: getClusterNodes returned no tpuQuic/tpu address" >&2
    return 1
  fi

  printf '%s' "$tpu"
}

global_args=()
run_args=()
leader_tracker_args=()

while [[ -n $1 ]]; do
  case "$1" in
  -h | --help)
    usage
    ;;
  --url | -u | --commitment-config | --authority)
    [[ -n ${2:-} ]] || usage "Missing value for $1"
    global_args+=("$1" "$2")
    shift 2
    ;;
  --url=* | --commitment-config=* | --authority=* | -u?*)
    global_args+=("$1")
    shift
    ;;
  --validate-accounts | --mock-rpc)
    global_args+=("$1")
    shift
    ;;
  run | read-accounts-run | write-accounts)
    usage "Do not pass a solana-transaction-bench subcommand; this wrapper always uses run."
    ;;
  pinned-leader-tracker | legacy-leader-tracker | ws-leader-tracker | \
    yellowstone-leader-tracker | custom-leader-tracker)
    leader_tracker_args=("$@")
    break
    ;;
  *)
    run_args+=("$1")
    shift
    ;;
  esac
done

if ! command -v "$program" > /dev/null 2>&1; then
  echo "$program not found."
  echo "Install it with: cargo install solana-transaction-bench"
  echo "Or set SOLANA_TRANSACTION_BENCH=/path/to/solana-transaction-bench."
  exit 1
fi

if ! contains_url_arg "${global_args[@]}"; then
  global_args+=(--url "http://127.0.0.1:8899")
fi
add_default_arg global_args --authority "${SOLANA_CONFIG_DIR}/faucet.json"

add_default_arg run_args --num-payers 1024
add_default_arg run_args --payer-account-balance 10SOL
add_default_arg run_args --duration 90
add_default_arg run_args --bind "127.0.0.1:0"
add_default_arg run_args --num-max-open-connections 8
add_default_arg run_args --workers-pull-size 8
add_default_arg run_args --lamports-to-transfer 65536
add_default_arg run_args --num-send-instructions-per-tx 1
add_default_arg run_args --send-fanout 1

for ((i = 0; i < default_staked_identity_file_count; i++)); do
  run_args+=(--staked-identity-file "${SOLANA_CONFIG_DIR}/bootstrap-validator/identity.json")
done

add_default_arg run_args --transfer-tx-cu-budget 400

if [[ ${#leader_tracker_args[@]} -eq 0 ]]; then
  rpc_url=$(get_rpc_url)
  pinned_leader_target=$(resolve_pinned_leader_target "$rpc_url") || exit 1
  leader_tracker_args=(pinned-leader-tracker "$pinned_leader_target")
fi

"$program" "${global_args[@]}" run "${run_args[@]}" "${leader_tracker_args[@]}"
