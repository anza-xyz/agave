#!/usr/bin/env bash

set -e

here="$(dirname "$0")"
src_root="$(readlink -f "${here}/..")"

cd "${src_root}"

# `cargo-audit` doesn't give us a way to do this nicely, so hammer it is...
dep_tree_filter="grep -Ev '│|└|├|─'"

while [[ -n $1 ]]; do
  if [[ $1 = "--display-dependency-trees" ]]; then
    dep_tree_filter="cat"
    shift
  fi
done

cargo_audit_ignores=(
  # === main repo ===
  #
  # Crate:     ed25519-dalek
  # Version:   1.0.1
  # Title:     Double Public Key Signing Function Oracle Attack on `ed25519-dalek`
  # Date:      2022-06-11
  # ID:        RUSTSEC-2022-0093
  # URL:       https://rustsec.org/advisories/RUSTSEC-2022-0093
  # Solution:  Upgrade to >=2
  --ignore RUSTSEC-2022-0093

  # === programs/sbf ===
  #
  # Crate:     curve25519-dalek
  # Version:   3.2.1
  # Title:     Timing variability in `curve25519-dalek`'s `Scalar29::sub`/`Scalar52::sub`
  # Date:      2024-06-18
  # ID:        RUSTSEC-2024-0344
  # URL:       https://rustsec.org/advisories/RUSTSEC-2024-0344
  # Solution:  Upgrade to >=4.1.3
  --ignore RUSTSEC-2024-0344

  # Crate:     tonic
  # Version:   0.9.2
  # Title:     Remotely exploitable Denial of Service in Tonic
  # Date:      2024-10-01
  # ID:        RUSTSEC-2024-0376
  # URL:       https://rustsec.org/advisories/RUSTSEC-2024-0376
  # Solution:  Upgrade to >=0.12.3
  --ignore RUSTSEC-2024-0376

  # Crate:     rustls
  # Version:   0.23.17
  # Title:     rustls network-reachable panic in `Acceptor::accept`
  # Date:      2024-11-22
  # ID:        RUSTSEC-2024-0399
  # URL:       https://rustsec.org/advisories/RUSTSEC-2024-0399
  # Solution:  Upgrade to >=0.23.18
  # Dependency tree:
  # rustls 0.23.17
  --ignore RUSTSEC-2024-0399
)
scripts/cargo-for-all-lock-files.sh audit "${cargo_audit_ignores[@]}" | $dep_tree_filter
# we want the `cargo audit` exit code, not `$dep_tree_filter`'s
exit "${PIPESTATUS[0]}"
