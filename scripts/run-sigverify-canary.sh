#!/usr/bin/env bash
#
# Launch agave-validator with an explicit transaction sigverify mode and
# isolated state directories for A/B experiments.
#
# Usage:
#   scripts/run-sigverify-canary.sh \
#     --mode heea \
#     --cluster mainnet-beta \
#     --name mainnet-canary \
#     --identity /path/to/identity.json \
#     --vote-account /path/to/vote-account.json \
#     --rpc-port 8899 \
#     --gossip-port 8001 \
#     --dynamic-port-range 8000-8020 \
#     --full-rpc-api \
#     --private-rpc \
#     --no-voting
#
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  run-sigverify-canary.sh --mode <ed25519|heea> --cluster <mainnet-beta|testnet|devnet> \
    --name <run-name> \
    --identity <path> [--vote-account <path>] [--base-dir <path>] [--binary <path>] \
    [--rpc-port <port>] [--gossip-port <port>] [--dynamic-port-range <range>] \
    [--log-suffix <suffix>] [--private-rpc] [--full-rpc-api] [--no-voting] \
    [--] [additional agave-validator args...]

Description:
  Runs agave-validator with:
  - --tx-sigverify-mode set to the selected mode
  - per-run ledger/accounts/log directories under <base-dir>/<name>-<mode>/
  - the repository's documented bootstrap entrypoints, known validators, and genesis hash
    for the selected cluster
  - the supplied validator flags forwarded unchanged after --

Examples:
  scripts/run-sigverify-canary.sh \
    --mode ed25519 \
    --cluster mainnet-beta \
    --name canary \
    --identity /home/sol/id.json \
    --vote-account /home/sol/vote.json \
    --rpc-port 8899 \
    --gossip-port 8001 \
    --dynamic-port-range 8000-8020 \
    --private-rpc \
    --full-rpc-api \
    --no-voting \
    --

  scripts/run-sigverify-canary.sh \
    --mode heea \
    --cluster testnet \
    --name canary \
    --identity /home/sol/id.json \
    --vote-account /home/sol/vote.json \
    --base-dir /mnt/agave-sigverify \
    --rpc-port 8899 \
    --dynamic-port-range 8000-8020 \
    --no-voting \
    -- \
    --limit-ledger-size
EOF
}

script_dir="$(readlink -f "$(dirname "$0")")"
repo_dir="$(readlink -f "$script_dir/..")"

mode=""
cluster=""
name=""
identity=""
vote_account=""
binary=""
base_dir="${AGAVE_SIGVERIFY_BASE_DIR:-$repo_dir/run/sigverify-canary}"
log_suffix=""
rpc_port="8899"
gossip_port=""
dynamic_port_range="8000-8020"
private_rpc=false
full_rpc_api=false
no_voting=false

while (($#)); do
  case "$1" in
    --mode)
      mode="${2:-}"
      shift 2
      ;;
    --name)
      name="${2:-}"
      shift 2
      ;;
    --cluster)
      cluster="${2:-}"
      shift 2
      ;;
    --identity)
      identity="${2:-}"
      shift 2
      ;;
    --vote-account)
      vote_account="${2:-}"
      shift 2
      ;;
    --base-dir)
      base_dir="${2:-}"
      shift 2
      ;;
    --binary)
      binary="${2:-}"
      shift 2
      ;;
    --log-suffix)
      log_suffix="${2:-}"
      shift 2
      ;;
    --rpc-port)
      rpc_port="${2:-}"
      shift 2
      ;;
    --gossip-port)
      gossip_port="${2:-}"
      shift 2
      ;;
    --dynamic-port-range)
      dynamic_port_range="${2:-}"
      shift 2
      ;;
    --private-rpc)
      private_rpc=true
      shift
      ;;
    --full-rpc-api)
      full_rpc_api=true
      shift
      ;;
    --no-voting)
      no_voting=true
      shift
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    --)
      shift
      break
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

case "$mode" in
  ed25519|heea) ;;
  *)
    echo "--mode must be one of: ed25519, heea" >&2
    exit 1
    ;;
esac

[[ -n "$name" ]] || {
  echo "--name is required" >&2
  exit 1
}

case "$cluster" in
  mainnet-beta|testnet|devnet) ;;
  *)
    echo "--cluster must be one of: mainnet-beta, testnet, devnet" >&2
    exit 1
    ;;
esac

[[ -n "$identity" ]] || {
  echo "--identity is required" >&2
  exit 1
}

[[ -f "$identity" ]] || {
  echo "identity keypair not found: $identity" >&2
  exit 1
}

if [[ -n "$vote_account" && ! -f "$vote_account" ]]; then
  echo "vote account keypair not found: $vote_account" >&2
  exit 1
fi

if [[ -z "$binary" ]]; then
  profile="${CARGO_BUILD_PROFILE:-release}"
  candidate="$repo_dir/target/$profile/agave-validator"
  if [[ -x "$candidate" ]]; then
    binary="$candidate"
  else
    binary="$(command -v agave-validator || true)"
  fi
fi

[[ -n "$binary" && -x "$binary" ]] || {
  echo "agave-validator binary not found; pass --binary or build target/release/agave-validator" >&2
  exit 1
}

run_dir="$base_dir/$name-$mode"
ledger_dir="$run_dir/ledger"
accounts_dir="$run_dir/accounts"
log_file="$run_dir/agave-validator${log_suffix:+-$log_suffix}.log"

mkdir -p "$ledger_dir" "$accounts_dir"

cluster_args=()
case "$cluster" in
  mainnet-beta)
    cluster_args+=(
      --known-validator 7Np41oeYqPefeNQEHSv1UDhYrehxin3NStELsSKCT4K2
      --known-validator GdnSyH3YtwcxFvQrVVJMm1JhTS4QVX7MFsX56uJLUfiZ
      --known-validator DE1bawNcRJB9rVm3buyMVfr8mBEoyyu73NBovf2oXJsJ
      --known-validator CakcnaRDHka2gXyfbEd2d3xsvkJkqsLw2akB3zsN1D2S
      --only-known-rpc
      --entrypoint entrypoint.mainnet-beta.solana.com:8001
      --entrypoint entrypoint2.mainnet-beta.solana.com:8001
      --entrypoint entrypoint3.mainnet-beta.solana.com:8001
      --entrypoint entrypoint4.mainnet-beta.solana.com:8001
      --entrypoint entrypoint5.mainnet-beta.solana.com:8001
      --expected-genesis-hash 5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d
    )
    ;;
  testnet)
    cluster_args+=(
      --known-validator 5D1fNXzvv5NjV1ysLjirC4WY92RNsVH18vjmcszZd8on
      --known-validator dDzy5SR3AXdYWVqbDEkVFdvSPCtS9ihF5kJkHCtXoFs
      --known-validator Ft5fbkqNa76vnsjYNwjDZUXoTWpP7VYm3mtsaQckQADN
      --known-validator eoKpUABi59aT4rR9HGS3LcMecfut9x7zJyodWWP43YQ
      --known-validator 9QxCLckBiJc783jnMvXZubK4wH86Eqqvashtrwvcsgkv
      --only-known-rpc
      --entrypoint entrypoint.testnet.solana.com:8001
      --entrypoint entrypoint2.testnet.solana.com:8001
      --entrypoint entrypoint3.testnet.solana.com:8001
      --expected-genesis-hash 4uhcVJyU9pJkvQyS88uRDiswHXSCkY3zQawwpjk2NsNY
    )
    ;;
  devnet)
    cluster_args+=(
      --known-validator dv1ZAGvdsz5hHLwWXsVnM94hWf1pjbKVau1QVkaMJ92
      --known-validator dv2eQHeP4RFrJZ6UeiZWoc3XTtmtZCUKxxCApCDcRNV
      --known-validator dv4ACNkpYPcE3aKmYDqZm9G5EB3J4MRoeE7WNDRBVJB
      --known-validator dv3qDFk1DTF36Z62bNvrCXe9sKATA6xvVy6A798xxAS
      --only-known-rpc
      --entrypoint entrypoint.devnet.solana.com:8001
      --entrypoint entrypoint2.devnet.solana.com:8001
      --entrypoint entrypoint3.devnet.solana.com:8001
      --entrypoint entrypoint4.devnet.solana.com:8001
      --entrypoint entrypoint5.devnet.solana.com:8001
      --expected-genesis-hash EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG
    )
    ;;
esac

validator_args=(
  --tx-sigverify-mode "$mode"
  --identity "$identity"
  --ledger "$ledger_dir"
  --accounts "$accounts_dir"
  --log "$log_file"
  --rpc-port "$rpc_port"
  --dynamic-port-range "$dynamic_port_range"
  --wal-recovery-mode skip_any_corrupted_record
)

if [[ -n "$vote_account" ]]; then
  validator_args+=(--vote-account "$vote_account")
fi

if [[ -n "$gossip_port" ]]; then
  validator_args+=(--gossip-port "$gossip_port")
fi

if $private_rpc; then
  validator_args+=(--private-rpc)
fi

if $full_rpc_api; then
  validator_args+=(--full-rpc-api)
fi

if $no_voting; then
  validator_args+=(--no-voting)
fi

validator_args+=("${cluster_args[@]}")

if (($#)); then
  validator_args+=("$@")
fi

cat <<EOF
Starting agave-validator
  binary:      $binary
  mode:        $mode
  cluster:     $cluster
  run dir:     $run_dir
  ledger dir:  $ledger_dir
  accounts dir:$accounts_dir
  log file:    $log_file
EOF

exec "$binary" "${validator_args[@]}"
