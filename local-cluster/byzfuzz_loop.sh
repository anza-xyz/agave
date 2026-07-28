#!/usr/bin/env bash
# Runs test_alpenglow_byzfuzz repeatedly with fresh (time-based) seeds.
# Saves each run's log to byzfuzz_logs/run_<seed>.log and stops on the first
# invariant violation (panic containing "byzzfuzz invariant failed").
set -u
cd /home/aarnav/fuzz/agave/local-cluster
LOGDIR=/home/aarnav/fuzz/agave/byzfuzz_logs
mkdir -p "$LOGDIR"
STATUS=/home/aarnav/fuzz/agave/byzfuzz_logs/LOOP_STATUS.txt
MAX=${1:-100}
i=1
while [ "$i" -le "$MAX" ]; do
  OUT=/tmp/byzfuzz_run.out
  cargo test test_alpenglow_byzfuzz &> "$OUT"
  RC=$?
  SEED=$(grep -oE "using seed [0-9]+" "$OUT" | head -1 | grep -oE "[0-9]+")
  [ -z "$SEED" ] && SEED="unknown_$i"
  cp "$OUT" "$LOGDIR/run_${SEED}.log"
  if grep -q "byzzfuzz invariant failed" "$OUT"; then
    echo "VIOLATION run=$i seed=$SEED rc=$RC log=$LOGDIR/run_${SEED}.log" | tee "$STATUS"
    grep -n "byzzfuzz invariant failed" "$OUT" | head -5
    exit 42
  fi
  if [ "$RC" -ne 0 ]; then
    echo "NONZERO_EXIT run=$i seed=$SEED rc=$RC log=$LOGDIR/run_${SEED}.log" | tee "$STATUS"
    exit 43
  fi
  echo "run=$i seed=$SEED OK rc=$RC" | tee "$STATUS"
  i=$((i+1))
done
echo "COMPLETED $MAX runs, no violation" | tee "$STATUS"
