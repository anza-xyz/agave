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

default_cargo_audit_extra_args=(
  # Crate:     ed25519-dalek
  # Version:   1.0.1
  # Title:     Double Public Key Signing Function Oracle Attack on `ed25519-dalek`
  # Date:      2022-06-11
  # ID:        RUSTSEC-2022-0093
  # URL:       https://rustsec.org/advisories/RUSTSEC-2022-0093
  # Solution:  Upgrade to >=2
  # Note:      dev-dependency of agave-precompiles only; the ed25519 precompile
  #            test compares against the exact dalek version it used to call.
  --ignore RUSTSEC-2022-0093

  # Crate:     idna
  # Version:   0.1.5
  # Title:     `idna` accepts Punycode labels that do not produce any non-ASCII when decoded
  # Date:      2024-12-09
  # ID:        RUSTSEC-2024-0421
  # URL:       https://rustsec.org/advisories/RUSTSEC-2024-0421
  # Solution:  Upgrade to >=1.0.0
  # need to solve this dependant tree:
  # jsonrpc-core-client v18.0.0 -> jsonrpc-client-transports v18.0.0 -> url v1.7.2 -> idna v0.1.5
  --ignore RUSTSEC-2024-0421

  # Crate:     curve25519-dalek
  # Version:   3.2.1
  # Title:     Timing variability in `curve25519-dalek`'s `Scalar29::sub`/`Scalar52::sub`
  # Date:      2024-06-18
  # ID:        RUSTSEC-2024-0344
  # URL:       https://rustsec.org/advisories/RUSTSEC-2024-0344
  # Solution:  Upgrade to >=4.1.3
  # Note:      pulled in by the ed25519-dalek 1.0.1 dev-dependency above.
  --ignore RUSTSEC-2024-0344

  # Crate:     crypto-bigint
  # Version:   0.5.5
  # Title:     Multiple timing variability issues in operations intended to be constant-time
  # Date:      2026-10-23
  # ID:        RUSTSEC-2026-0387
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0387
  # Solution:  Upgrade to >=0.7.6
  # Agave Ok:  only reached through k256 in solana-secp256k1-program and
  # solana-secp256k1-recover, which verify and recover over public data.
  # The fix needs those SDK crates to move to elliptic-curve 0.14.
  --ignore RUSTSEC-2026-0387

  # Crate:     quinn-udp
  # Version:   0.5.14 (dev-bins), 0.5.4 (programs/sbf)
  # Title:     Receive loop can spin after a truncated datagram
  # ID:        RUSTSEC-2026-0538
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0538
  # Solution:  Upgrade to >=0.5.16
  # Agave Ok:  only the single-recvmsg fallback (macOS, OpenBSD, Solaris) is affected; Linux
  # uses recvmmsg. On macOS, agave-ledger-tool (dev-bins) links it via solana-core but
  # never opens a QUIC socket.
  --ignore RUSTSEC-2026-0538

  # Crate:     libz-sys
  # Version:   1.1.31
  # Title:     libs-sys: heap buffer overflow
  # Date:      2026-11-11
  # ID:        RUSTSEC-2026-0544
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0544
  # Solution:  Upgrade to >=1.1.33
  # Agave Ok:  optional rocksdb zlib dep, not built; the bug is in the gzFile API rocksdb never calls.
  --ignore RUSTSEC-2026-0544

  # Crate:     url
  # Version:   2.5.8 (1.7.2 via jsonrpc-client-transports)
  # Title:     Stack overflow in `Url::origin()` on nested `blob:` URLs
  # Date:      2026-11-22
  # ID:        RUSTSEC-2026-0592
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0592
  # Solution:  Upgrade to >=2.5.9
  # Agave Ok:  Agave never calls `Url::origin` or `url::quirks::origin` (clippy disallows them).
  --ignore RUSTSEC-2026-0592
)

xtask_cargo_audit_extra_args=(
  # Crate:     rsa
  # Version:   0.9.10
  # Title:     Marvin Attack: potential key recovery through timing sidechannels
  # Date:      2023-11-22
  # ID:        RUSTSEC-2023-0071
  # URL:       https://rustsec.org/advisories/RUSTSEC-2023-0071
  # Severity:  5.9 (medium)
  # Solution:  No fixed upgrade is available!
  --ignore RUSTSEC-2023-0071

  # Crate:     quinn-udp
  # Version:   0.5.14
  # Title:     Receive loop can spin after a truncated datagram
  # ID:        RUSTSEC-2026-0538
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0538
  # Solution:  Upgrade to >=0.5.16
  # Agave Ok:  only in xtask's lockfile as an optional dep of Reqwest's HTTP/3 feature, which
  # xtask does not enable, so upstream Quinn and quinn-udp are never built.
  --ignore RUSTSEC-2026-0538

  # Crate:     url
  # Version:   2.5.8
  # Title:     Stack overflow in `Url::origin()` on nested `blob:` URLs
  # Date:      2026-11-22
  # ID:        RUSTSEC-2026-0592
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0592
  # Solution:  Upgrade to >=2.5.9
  # Agave Ok:  only octocrab and reqwest use url in xtask, and neither calls `Url::origin`
  # or `url::quirks::origin`. xtask doesn't call them either.
  --ignore RUSTSEC-2026-0592
)

lock_files="$(git ls-files ':**Cargo.lock')"

for lock_file in $lock_files; do
  audit_extra_args=()
  case "$lock_file" in
  ci/xtask/Cargo.lock)
    audit_extra_args=("${xtask_cargo_audit_extra_args[@]}")
    ;;
  *)
    audit_extra_args=("${default_cargo_audit_extra_args[@]}")
    ;;
  esac

  echo "--- [$lock_file]: cargo audit ${audit_extra_args[*]}"

  set +e
  (
    cd "$(dirname "$lock_file")"
    cargo audit --color always "${audit_extra_args[@]}" </dev/null
  ) | $dep_tree_filter
  pipeline_status=("${PIPESTATUS[@]}")
  set -e

  exit_code="${pipeline_status[0]}"
  if [[ $exit_code -ne 0 ]]; then
    exit "$exit_code"
  fi
done
