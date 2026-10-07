#!/usr/bin/env bash
#
# Runs a command in a private network namespace with its own loopback device, so that
# local-cluster tests can shape loopback traffic (see local-cluster/src/network_delay.rs)
# without affecting the host. The command runs as the invoking user; the namespace is
# deleted on exit. Requires passwordless sudo.
#
# Example:
#   local-cluster/run-in-netns.sh cargo test -p solana-local-cluster --test local_cluster \
#     test_byz_fuzz -- --exact --nocapture

set -euo pipefail

if [[ $# -eq 0 ]]; then
  echo "usage: $0 <command> [args...]" >&2
  exit 1
fi

ns="local-cluster-$$"
sudo -n ip netns add "$ns"
trap 'sudo -n ip netns del "$ns"' EXIT
sudo -n ip netns exec "$ns" ip link set lo up

# sudo -E keeps the environment (cargo/rustup homes, RUST_LOG, BYZ_FUZZ_*) except PATH and
# HOME, which sudo resets, so they are restored explicitly. setpriv drops back to
# the invoking user inside the namespace.
LOCAL_CLUSTER_NETNS="$ns" sudo -n -E ip netns exec "$ns" \
  setpriv --reuid="$(id -u)" --regid="$(id -g)" --init-groups env PATH="$PATH" HOME="$HOME" "$@"
