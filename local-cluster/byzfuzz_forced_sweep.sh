#!/usr/bin/env bash
# Sweeps safety-critical forced mutations via ALPENGLOW_FORCE_MUTATION, several
# fresh seeds each. Every corruption in a run applies the forced mutation, driving
# a specific adversarial behavior deep into votor. Stops on first invariant violation.
set -u
cd /home/aarnav/fuzz/agave/local-cluster
LOGDIR=/home/aarnav/fuzz/agave/byzfuzz_logs/forced
mkdir -p "$LOGDIR"
STATUS=/home/aarnav/fuzz/agave/byzfuzz_logs/FORCED_STATUS.txt
SEEDS_PER=${1:-4}
# Highest-value safety mutations first: cert forgery + equivocation.
MUTS=(CertBitmapSet CertTypeSwap CertSlotPlus1 CertSlotMinus1 FastFallbackSwap \
      NotarizeEquivocation FallbackEquivocation EquivocateVote CertBitmapClear \
      CertSignatureBitflip ParentSlotPlus1 BlockIdBitflip)
: > "$STATUS"
for M in "${MUTS[@]}"; do
  for s in $(seq 1 "$SEEDS_PER"); do
    OUT=/tmp/byzfuzz_forced.out
    ALPENGLOW_FORCE_MUTATION="$M" cargo test test_alpenglow_byzfuzz &> "$OUT"
    RC=$?
    SEED=$(grep -oE "using seed [0-9]+" "$OUT" | head -1 | grep -oE "[0-9]+")
    [ -z "$SEED" ] && SEED="unk"
    cp "$OUT" "$LOGDIR/${M}_${SEED}.log"
    if grep -q "byzzfuzz invariant failed" "$OUT"; then
      echo "VIOLATION mutation=$M seed=$SEED rc=$RC log=$LOGDIR/${M}_${SEED}.log" | tee -a "$STATUS"
      grep -n "byzzfuzz invariant failed" "$OUT" | head -5
      exit 42
    fi
    if [ "$RC" -ne 0 ]; then
      echo "NONZERO mutation=$M seed=$SEED rc=$RC (not an invariant panic) log=$LOGDIR/${M}_${SEED}.log" | tee -a "$STATUS"
    else
      echo "OK mutation=$M seed=$SEED" | tee -a "$STATUS"
    fi
  done
done
echo "COMPLETED forced sweep, no invariant violation" | tee -a "$STATUS"
