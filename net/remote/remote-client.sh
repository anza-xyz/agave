#!/usr/bin/env bash
set -e

cd "$(dirname "$0")"/../..

deployMethod="$1"
entrypointIp="$2"
clientToRun="$3"
if [[ -n $4 ]]; then
  export RUST_LOG="$4"
fi
benchTpsExtraArgs="$5"
clientIndex="$6"
clientType="${7:-tpu-client}"
maybeUseUnstakedConnection="$8"

missing() {
  echo "Error: $1 not specified"
  exit 1
}

[[ -n $deployMethod ]] || missing deployMethod
[[ -n $entrypointIp ]] || missing entrypointIp

source net/common.sh
loadConfigFile

threadCount=$(nproc)
if [[ $threadCount -gt 4 ]]; then
  threadCount=4
fi

case $deployMethod in
local|tar)
  PATH="$HOME"/.cargo/bin:"$PATH"
  export USE_INSTALL=1
  net/scripts/rsync-retry.sh -vPrc "$entrypointIp:~/.cargo/bin/*" ~/.cargo/bin/
  ;;
skip)
  ;;
*)
  echo "Unknown deployment method: $deployMethod"
  exit 1
esac

RPC_CLIENT=false
case "$clientType" in
  tpu-client)
    RPC_CLIENT=false
    ;;
  rpc-client)
    RPC_CLIENT=true
    ;;
  *)
    echo "Unexpected clientType: \"$clientType\""
    exit 1
    ;;
esac

# Does "$@" (beyond the first arg, which is the name to look for) contain $name,
# either as a bare flag or as --flag=value?
contains_arg() {
  declare name=$1
  shift
  declare arg
  for arg in "$@"; do
    if [[ $arg = "$name" || $arg = "$name="* ]]; then
      return 0
    fi
  done
  return 1
}

contains_url_arg() {
  declare arg
  for arg in "$@"; do
    case "$arg" in
    --url | --url=* | -u | -u?*)
      return 0
      ;;
    esac
  done
  return 1
}

# Ensure the external solana-transaction-bench binary is available. Prefer a
# path provided via SOLANA_TRANSACTION_BENCH, then one already on PATH (e.g.
# rsynced from the entrypoint's ~/.cargo/bin), and finally fall back to
# installing it from crates.io.
ensureTransactionBench() {
  transactionBench="${SOLANA_TRANSACTION_BENCH:-solana-transaction-bench}"
  if command -v "$transactionBench" > /dev/null 2>&1; then
    return 0
  fi

  echo "$transactionBench not found, installing solana-transaction-bench from crates.io"
  declare -a cargoArgs=(install --locked)
  [[ -z ${SOLANA_TRANSACTION_BENCH_VERSION:-} ]] || cargoArgs+=(--version "$SOLANA_TRANSACTION_BENCH_VERSION")
  cargoArgs+=(solana-transaction-bench)
  if ! cargo "${cargoArgs[@]}"; then
    echo "Error: unable to obtain solana-transaction-bench."
    echo "Stage it into the entrypoint's ~/.cargo/bin or set SOLANA_TRANSACTION_BENCH to a prebuilt binary path."
    exit 1
  fi
  transactionBench=solana-transaction-bench
}

case $clientToRun in
solana-transaction-bench)
  ensureTransactionBench

  # transaction-bench creates and funds ephemeral payer accounts from the
  # faucet (the genesis mint) at runtime, so unlike bench-tps it needs no
  # pre-generated client account keys. We only need the faucet keypair as the
  # funding authority and, for a staked QUIC connection, a staked validator
  # identity.
  net/scripts/rsync-retry.sh -vPrc \
    "$entrypointIp":~/solana/config/faucet.json ./faucet.json

  net/scripts/rsync-retry.sh -vPrc \
    "$entrypointIp":~/solana/config/validator-identity-1.json ./validator-identity.json

  if [[ $clientType = rpc-client ]]; then
    echo "Note: clientType=rpc-client is ignored by solana-transaction-bench; it always sends over QUIC to the TPU."
  fi

  # solana-transaction-bench expects its arguments in positional buckets:
  #   <global args> run <run args> <leader-tracker subcommand>
  # Split the operator-supplied extra args into those buckets so that, e.g.,
  # a --url override lands before "run" while --duration lands after it.
  globalArgs=()
  runArgs=()
  leaderTrackerArgs=()

  # shellcheck disable=SC2206 # Intentional word-splitting of the extra args string
  extraArgs=($benchTpsExtraArgs)
  argIndex=0
  while [[ $argIndex -lt ${#extraArgs[@]} ]]; do
    arg="${extraArgs[$argIndex]}"
    case "$arg" in
    -u | --url | --authority | --commitment-config)
      if [[ $((argIndex + 1)) -ge ${#extraArgs[@]} ]]; then
        echo "Error: missing value for $arg in extra args"
        exit 1
      fi
      globalArgs+=("$arg" "${extraArgs[$((argIndex + 1))]}")
      argIndex=$((argIndex + 2))
      ;;
    --url=* | --authority=* | --commitment-config=* | -u?*)
      globalArgs+=("$arg")
      argIndex=$((argIndex + 1))
      ;;
    --validate-accounts | --mock-rpc)
      globalArgs+=("$arg")
      argIndex=$((argIndex + 1))
      ;;
    run | read-accounts-run | write-accounts)
      echo "Error: do not pass a solana-transaction-bench subcommand in extra args; this wrapper always uses 'run'."
      exit 1
      ;;
    pinned-leader-tracker | legacy-leader-tracker | ws-leader-tracker | \
      yellowstone-leader-tracker | custom-leader-tracker)
      leaderTrackerArgs=("${extraArgs[@]:$argIndex}")
      break
      ;;
    *)
      runArgs+=("$arg")
      argIndex=$((argIndex + 1))
      ;;
    esac
  done

  contains_url_arg "${globalArgs[@]}" || globalArgs+=(--url "http://$entrypointIp:8899")
  contains_arg --authority "${globalArgs[@]}" || globalArgs+=(--authority ./faucet.json)

  contains_arg --duration "${runArgs[@]}" || runArgs+=(--duration 7500)
  contains_arg --num-payers "${runArgs[@]}" || runArgs+=(--num-payers 256)
  contains_arg --payer-account-balance "${runArgs[@]}" || runArgs+=(--payer-account-balance 10SOL)
  contains_arg --transfer-tx-cu-budget "${runArgs[@]}" || runArgs+=(--transfer-tx-cu-budget 600)

  if [[ -z "$maybeUseUnstakedConnection" ]]; then
    contains_arg --staked-identity-file "${runArgs[@]}" || runArgs+=(--staked-identity-file ./validator-identity.json)
  fi

  # In a multinode cluster the leader rotates, so follow the leader schedule
  # over the RPC websocket by default. Operators can override by passing a
  # leader-tracker subcommand in the extra args.
  if [[ ${#leaderTrackerArgs[@]} -eq 0 ]]; then
    leaderTrackerArgs=(ws-leader-tracker)
  fi

  clientCommand="\
    $transactionBench \
      ${globalArgs[*]} \
      run \
      ${runArgs[*]} \
      ${leaderTrackerArgs[*]} \
  "
  ;;
solana-bench-tps)
  net/scripts/rsync-retry.sh -vPrc \
    "$entrypointIp":~/solana/config/bench-tps"$clientIndex".yml ./client-accounts.yml

  net/scripts/rsync-retry.sh -vPrc \
    "$entrypointIp":~/solana/config/validator-identity-1.json ./validator-identity.json

  args=()

  if ${RPC_CLIENT}; then
    args+=(--use-rpc-client)
  fi

  if [[ -z "$maybeUseUnstakedConnection" ]]; then
    args+=(--bind-address "$entrypointIp")
    args+=(--client-node-id ./validator-identity.json)
  fi

  clientCommand="\
    solana-bench-tps \
      --duration 7500 \
      --sustained \
      --threads $threadCount \
      $benchTpsExtraArgs \
      --read-client-keys ./client-accounts.yml \
      --url "http://$entrypointIp:8899" \
      ${args[*]} \
  "
  ;;
idle)
  # Add the faucet keypair to idle clients for convenience
  net/scripts/rsync-retry.sh -vPrc \
    "$entrypointIp":~/solana/config/faucet.json ~/solana/
  exit 0
  ;;
*)
  echo "Unknown client name: $clientToRun"
  exit 1
esac


cat > ~/solana/on-reboot <<EOF
#!/usr/bin/env bash
cd ~/solana

PATH="$HOME"/.cargo/bin:"$PATH"
export USE_INSTALL=1

echo "$(date) | $0 $*" >> client.log

(
  sudo SOLANA_METRICS_CONFIG="$SOLANA_METRICS_CONFIG" scripts/oom-monitor.sh
) > oom-monitor.log 2>&1 &
echo \$! > oom-monitor.pid
scripts/fd-monitor.sh > fd-monitor.log 2>&1 &
echo \$! > fd-monitor.pid
scripts/net-stats.sh  > net-stats.log 2>&1 &
echo \$! > net-stats.pid
! tmux list-sessions || tmux kill-session

tmux new -s "$clientToRun" -d "
  while true; do
    echo === Client start: \$(date) | tee -a client.log
    $metricsWriteDatapoint 'testnet-deploy client-begin=1'
    echo '$ $clientCommand' | tee -a client.log
    $clientCommand >> client.log 2>&1
    $metricsWriteDatapoint 'testnet-deploy client-complete=1'
  done
"
EOF
chmod +x ~/solana/on-reboot
echo "@reboot ~/solana/on-reboot" | crontab -

~/solana/on-reboot

sleep 1
tmux capture-pane -t "$clientToRun" -p -S -100
