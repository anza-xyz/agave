#!/bin/bash


set -ex


if [ "$LOG_PATH" == "" ]; then
  LOG_PATH="`mktemp -d`"
else
  rm    -rf $LOG_PATH
  mkdir -pv $LOG_PATH
fi




mkdir -p dump


if [ ! -d dump/test-vectors ]; then
  cd dump
  git clone --depth=1 -q https://github.com/firedancer-io/test-vectors.git
  cd ..
else
  cd dump/test-vectors
  git pull -q
  cd ../..
fi


# Skip only the programs that have been migrated to Core BPF
find dump/test-vectors/instr/fixtures -type f -name '*.fix' | \
  grep -v '/stake/' | \
  grep -v '/config/' | \
  grep -v '/address-lookup-table/' | \
  xargs -P 32 -I {} ../target/release/test_exec_instr {} > $LOG_PATH/test_exec_instr.log 2>&1
# Other tests will be included here...


failed=`grep -wR FAIL $LOG_PATH | wc -l`
passed=`grep -wR OK $LOG_PATH | wc -l`


echo "PASSED: $passed"
echo "FAILED: $failed"


echo Test vectors success