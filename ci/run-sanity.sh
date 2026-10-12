#!/usr/bin/env bash
set -e

cd "$(dirname "$0")/.."
# shellcheck source=multinode-demo/common.sh
source multinode-demo/common.sh

if [[ -z $CI ]]; then
  # Build eagerly if needed for local development. Otherwise, odd timing error occurs...
  $solana_keygen --version
  $solana_genesis --version
  $solana_faucet --version
  $solana_cli --version
  $agave_validator --version
  $solana_ledger_tool --version
fi

rm -rf config/run/init-completed config/ledger

# Sanity-check that agave-validator can successfully terminate itself without relying on
# process::exit() by extending the timeout...
validator_timeout="${SOLANA_VALIDATOR_EXIT_TIMEOUT:-120}"
SOLANA_RUN_SH_VALIDATOR_ARGS="${SOLANA_RUN_SH_VALIDATOR_ARGS} --full-snapshot-interval-slots 200 --incremental-snapshot-interval-slots 100" \
  SOLANA_VALIDATOR_EXIT_TIMEOUT="$validator_timeout" \
  timeout "$validator_timeout" ./scripts/run.sh &

pid=$!

attempts=20
while [[ ! -f config/run/init-completed ]]; do
  sleep 1
  if ((--attempts == 0)); then
     echo "Error: validator failed to boot"
     exit 1
  else
    echo "Checking init"
  fi
done

# Wait for enough rooted slots to exercise snapshot creation and ledger verification.
snapshot_slot=50
latest_slot=0

# wait a bit longer than snapshot_slot
while [[ $latest_slot -le $((snapshot_slot + 1)) ]]; do
  sleep 1
  echo "Checking slot"
  latest_slot=$($solana_cli --url http://localhost:8899 slot --commitment processed)
done

# Ensure the copied range contains a transaction. Tower votes used to put a
# transaction in every slot, but Alpenglow votes are block components.
$solana_cli --url http://localhost:8899 airdrop 1
latest_slot=$($solana_cli --url http://localhost:8899 slot --commitment processed)

$agave_validator --ledger config/ledger exit --force || true

wait $pid

rm -rf config/snapshot-ledger
$solana_ledger_tool create-snapshot --ledger config/ledger "$snapshot_slot" config/snapshot-ledger
cp config/ledger/genesis.tar.bz2 config/snapshot-ledger
$solana_ledger_tool copy --ledger config/ledger \
  --target-ledger config/snapshot-ledger --starting-slot "$snapshot_slot" --ending-slot "$latest_slot"

set -x
$solana_ledger_tool --ledger config/snapshot-ledger --verbose --verbose print \
  --starting-slot "$snapshot_slot" --ending-slot "$latest_slot" \
  |& grep -q "Log Messages:$" && exit 1

$solana_ledger_tool verify --abort-on-invalid-block \
  --ledger config/snapshot-ledger --enable-rpc-transaction-history \
  --enable-extended-tx-metadata-storage

$solana_ledger_tool --ledger config/snapshot-ledger --verbose --verbose print \
  --starting-slot "$snapshot_slot" --ending-slot "$latest_slot" \
  |& grep -q "Log Messages:$"
set +x
