#!/usr/bin/env bash

set -eo pipefail

check_dcou1() {
  scripts/check-dev-context-only-utils.sh check-all-targets "$@"
}

check_dcou2() {
  scripts/check-dev-context-only-utils.sh check-bins-and-lib "$@"
}

shard=$1
shift
case "$shard" in
  1-of-4)
    check_dcou1 --partition 1/9
    check_dcou1 --partition 2/9
    check_dcou1 --partition 3/9
    check_dcou1 --partition 4/9
    check_dcou1 --partition 5/9
    check_dcou2 --partition 1/9
    check_dcou2 --partition 2/9
    check_dcou2 --partition 3/9
    check_dcou2 --partition 4/9
    check_dcou2 --partition 5/9
    ;;
  2-of-4)
    check_dcou1 --partition 6/9
    check_dcou1 --partition 7/9
    check_dcou2 --partition 6/9
    check_dcou2 --partition 7/9
    ;;
  3-of-4)
    check_dcou1 --partition 8/9
    check_dcou2 --partition 8/9
    ;;
  4-of-4)
    check_dcou1 --partition 9/9
    check_dcou2 --partition 9/9
    ;;
  *)
    echo "$0: unrecognized shard: $shard";
    exit 1
    ;;
esac
