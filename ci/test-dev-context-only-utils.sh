#!/usr/bin/env bash

set -eo pipefail

check_dcou() {
  scripts/check-dev-context-only-utils.sh check-all-targets "$@"
  scripts/check-dev-context-only-utils.sh check-bins-and-lib "$@"
}

shard=$1
shift
case "$shard" in
  1-of-4)
    check_dcou --partition 1/9
    check_dcou --partition 2/9
    check_dcou --partition 3/9
    check_dcou --partition 4/9
    check_dcou --partition 5/9
    ;;
  2-of-4)
    check_dcou --partition 6/9
    check_dcou --partition 7/9
    ;;
  2-of-4)
    check_dcou --partition 8/9
    ;;
  4-of-4)
    check_dcou --partition 9/9
    ;;
  *)
    echo "$0: unrecognized shard: $shard";
    exit 1
    ;;
esac
