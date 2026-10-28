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
  # Crate:     quinn-proto
  # Version:   0.11.15
  # Title:     Unbounded growth of pending connection ID retirements in quinn-proto
  # ID:        RUSTSEC-2026-0339
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0339
  # Solution:  Upgrade to >=0.11.17
  # Agave Ok:  active QUIC uses anza-quinn-proto 0.11.15-rustsec20260339 with the fix.
  # Upstream quinn-proto remains only in Reqwest's disabled HTTP/3 dependencies.
  --ignore RUSTSEC-2026-0339

  # Crate:     quinn-proto
  # Version:   0.11.15
  # Title:     Remote peer can panic quinn-proto with zero-length STREAM frames during unordered reads
  # ID:        RUSTSEC-2026-0338
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0338
  # Solution:  Upgrade to >=0.11.18
  # Agave Ok:  Agave does not use unordered QUIC reads.
  --ignore RUSTSEC-2026-0338

  # Crate:     quinn-proto
  # Version:   0.11.15
  # Title:     Peer minimum ACK delay can panic quinn-proto when ACK_FREQUENCY transmission is enabled
  # ID:        RUSTSEC-2026-0340
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0340
  # Solution:  Upgrade to >=0.11.18
  # Agave Ok:  Agave does not enable ACK_FREQUENCY transmission.
  --ignore RUSTSEC-2026-0340

  # Crate:     quinn-proto
  # Version:   0.11.15
  # Title:     Stream reassembly memory-exhaustion guard in quinn-proto can be bypassed
  # ID:        RUSTSEC-2026-0341
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0341
  # Solution:  Upgrade to >=0.11.17
  # Agave Ok:  Accepted residual risk. Streamer's small per-stream receive window limits impact.
  # The tpu-client-next exposure is also accepted for v4.3.
  --ignore RUSTSEC-2026-0341

  # Crate:     quinn-proto
  # Version:   0.11.15
  # Title:     Zero-length DATAGRAM frames bypass the receive buffer limit in quinn-proto
  # ID:        RUSTSEC-2026-0342
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0342
  # Solution:  Upgrade to >=0.11.17
  # Agave Ok:  Agave has mitigations. See: https://github.com/anza-xyz/agave/pull/14407. We are not vulnerable.
  --ignore RUSTSEC-2026-0342

  # Crate:     ed25519-dalek
  # Version:   1.0.1
  # Title:     Double Public Key Signing Function Oracle Attack on `ed25519-dalek`
  # Date:      2022-06-11
  # ID:        RUSTSEC-2022-0093
  # URL:       https://rustsec.org/advisories/RUSTSEC-2022-0093
  # Solution:  Upgrade to >=2
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
  --ignore RUSTSEC-2024-0344

  # Crate:     rustls
  # Version:   <0.23.45
  # Title:     rustls accepts TLS 1.3 handshake messages at the wrong encryption level
  # Date:      2026-09-14
  # ID:        RUSTSEC-2026-0285
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0285
  # Solution:  Upgrade to >=0.23.45
  # Agave Ok:  handshake stays authenticated; no integrity or availability impact
  --ignore RUSTSEC-2026-0285

  # Crate:     rustls
  # Version:   0.23.43
  # Title:     `Debug` impls for client session values expose TLS secrets
  # Date:      2026-10-14
  # ID:        RUSTSEC-2026-0346
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0346
  # Solution:  Upgrade to >=0.23.46
  # Agave Ok:  local attacker/log-access risk accepted for v4.3.
  --ignore RUSTSEC-2026-0346

  # Crate:     rustls
  # Version:   0.23.43
  # Title:     Brotli certificate decompression can allocate up to 1 GiB before authentication
  # Date:      2026-10-14
  # ID:        RUSTSEC-2026-0348
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0348
  # Solution:  Upgrade to >=0.23.46
  # Agave Ok:  rustls's optional Brotli certificate decompression feature is not enabled.
  --ignore RUSTSEC-2026-0348

  # Crate:     rustls
  # Version:   0.23.43
  # Title:     Unauthenticated ServerHello can downgrade later connections to classical key exchange
  # Date:      2026-10-14
  # ID:        RUSTSEC-2026-0349
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0349
  # Solution:  Upgrade to >=0.23.46
  # Agave Ok:  rustls uses ring's classical key exchanges; post-quantum groups are not enabled.
  --ignore RUSTSEC-2026-0349

  # Crate:     webpki-roots
  # Version:   <1.0.10
  # Title:     Bundled root certificates allow trust in distrusted emSign intermediates
  # Date:      2026-10-19
  # ID:        RUSTSEC-2026-0381
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0381
  # Solution:  Upgrade to >=1.0.10
  # Agave Ok:  roots are only used by rustls, whose vendored rustls-webpki (anza-xyz fork, RUSTSEC-2026-0382 fix) rejects these paths
  --ignore RUSTSEC-2026-0381

  # Crate:     webpki-root-certs
  # Version:   <1.0.10
  # Title:     Bundled root certificates allow trust in distrusted emSign intermediates
  # Date:      2026-10-19
  # ID:        RUSTSEC-2026-0383
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0383
  # Solution:  Upgrade to >=1.0.10
  # Agave Ok:  only a wasm32 dependency of rustls-platform-verifier; not built for Agave
  --ignore RUSTSEC-2026-0383

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

  # Crate:     tonic
  # Version:   0.9.2
  # Title:     Remotely exploitable Denial of Service in Tonic
  # Date:      2024-10-01
  # ID:        RUSTSEC-2024-0376
  # URL:       https://rustsec.org/advisories/RUSTSEC-2024-0376
  # Solution:  Upgrade to >=0.12.3
  --ignore RUSTSEC-2024-0376

  # Crate:     ctutils
  # Version:   0.4.0
  # Title:     Non-constant-time mask generation and `u128` comparisons
  # Date:      2026-10-23
  # ID:        RUSTSEC-2026-0385
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0385
  # Solution:  Upgrade to >=0.4.4
  # Agave's transitive consumers only use byte/tag equality, which does not
  # call the affected mask generation or `u128` comparison APIs.
  --ignore RUSTSEC-2026-0385

  # Crate:     h2
  # Version:   0.4.13
  # Title:     h2 unbounded empty DATA frames
  # Date:      2026-08-17
  # ID:        RUSTSEC-2026-0258
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0258
  # Solution:  Upgrade to >=0.4.16
  # Agave Ok:  Bigtable is the only use and is being considered a trusted peer
  --ignore RUSTSEC-2026-0258

  # Crate:     imbl-sized-chunks
  # Version:   0.1.3
  # Title:     Double free / use-after-free in `Chunk` and `InlineArray` removal methods when an element's `Drop` panics
  # Date:      2026-09-04
  # ID:        RUSTSEC-2026-0292
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0292
  # Solution:  Upgrade to >=0.2.0 (requires imbl >=7.0.2)
  # Agave Ok:  only imbl::HashMap is used, which never calls the affected methods; stored types have no panicking Drop
  --ignore RUSTSEC-2026-0292

  # Crate:     zstd
  # Version:   0.13.3
  # Title:     Data from an abandoned stream leaks into the next stream on a reused context
  # Date:      2026-10-16
  # ID:        RUSTSEC-2026-0391
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0391
  # Solution:  Upgrade to >=0.14.3
  # Agave Ok:  only the stream `with_context` constructors are affected; Agave never uses them.
  --ignore RUSTSEC-2026-0391

  # Crate:     zstd-safe
  # Version:   7.2.1 (7.2.4 in dev-bins)
  # Title:     Uninitialized memory exposed when streaming decompression of a legacy frame fails
  # Date:      2026-10-16
  # ID:        RUSTSEC-2026-0403
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0403
  # Solution:  Upgrade to >=8.2.0
  # Agave Ok:  only reachable through zstd::stream::write::Decoder; Agave decompresses with
  # read::Decoder and the bulk API, which the advisory lists as unaffected.
  --ignore RUSTSEC-2026-0403

  # Crate:     zstd-safe
  # Version:   7.2.4 (dev-bins only; 7.2.1 elsewhere is unaffected)
  # Title:     `Seekable::decompress` sets a `Vec` length beyond its capacity for offsets
  #            past the end
  # Date:      2026-10-16
  # ID:        RUSTSEC-2026-0398
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0398
  # Solution:  Upgrade to >=8.2.0
  # Agave Ok:  the zstd-safe `seekable` feature is not enabled.
  --ignore RUSTSEC-2026-0398

  # Crate:     zstd-sys
  # Version:   2.0.13+zstd.1.5.6 (2.0.16+zstd.1.5.7 in dev-bins)
  # Title:     WebAssembly allocator shim corrupts memory on allocation failure
  # Date:      2026-10-16
  # ID:        RUSTSEC-2026-0410
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0410
  # Solution:  Upgrade to >=2.1.2
  # Agave Ok:  wasm32 only; Agave does not build for wasm32.
  --ignore RUSTSEC-2026-0410

  # Crate:     pulldown-cmark
  # Version:   0.13.1
  # Title:     Exponential event growth with `ENABLE_WIKILINKS`
  # Date:      2026-10-18
  # ID:        RUSTSEC-2026-0414
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0414
  # Solution:  Upgrade to >=0.13.5
  # Agave Ok:  build-time only; prost-build parses comments in Agave's own .proto files.
  --ignore RUSTSEC-2026-0414
)

xtask_cargo_audit_extra_args=(
  # Crate:     quinn-proto
  # Version:   0.11.15
  # Title:     Unbounded growth of pending connection ID retirements in quinn-proto
  # ID:        RUSTSEC-2026-0339
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0339
  # Solution:  Upgrade to >=0.11.17
  # Agave Ok:  xtask does not enable Reqwest's HTTP/3 feature; upstream Quinn is unused.
  --ignore RUSTSEC-2026-0339

  # Crate:     quinn-proto
  # Version:   0.11.15
  # Title:     Remote peer can panic quinn-proto with zero-length STREAM frames during unordered reads
  # ID:        RUSTSEC-2026-0338
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0338
  # Solution:  Upgrade to >=0.11.18
  # Agave Ok:  xtask does not enable Reqwest's HTTP/3 feature; upstream Quinn is unused.
  --ignore RUSTSEC-2026-0338

  # Crate:     quinn-proto
  # Version:   0.11.15
  # Title:     Peer minimum ACK delay can panic quinn-proto when ACK_FREQUENCY transmission is enabled
  # ID:        RUSTSEC-2026-0340
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0340
  # Solution:  Upgrade to >=0.11.18
  # Agave Ok:  xtask does not enable Reqwest's HTTP/3 feature; upstream Quinn is unused.
  --ignore RUSTSEC-2026-0340

  # Crate:     quinn-proto
  # Version:   0.11.15
  # Title:     Stream reassembly memory-exhaustion guard in quinn-proto can be bypassed
  # ID:        RUSTSEC-2026-0341
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0341
  # Solution:  Upgrade to >=0.11.17
  # Agave Ok:  xtask does not enable Reqwest's HTTP/3 feature; upstream Quinn is unused.
  --ignore RUSTSEC-2026-0341

  # Crate:     quinn-proto
  # Version:   0.11.15
  # Title:     Zero-length DATAGRAM frames bypass the receive buffer limit in quinn-proto
  # ID:        RUSTSEC-2026-0342
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0342
  # Solution:  Upgrade to >=0.11.17
  # Agave Ok:  xtask does not enable Reqwest's HTTP/3 feature; upstream Quinn is unused.
  --ignore RUSTSEC-2026-0342

  # Crate:     rsa
  # Version:   0.9.10
  # Title:     Marvin Attack: potential key recovery through timing sidechannels
  # Date:      2023-11-22
  # ID:        RUSTSEC-2023-0071
  # URL:       https://rustsec.org/advisories/RUSTSEC-2023-0071
  # Severity:  5.9 (medium)
  # Solution:  No fixed upgrade is available!
  --ignore RUSTSEC-2023-0071

  # Crate:     rustls
  # Version:   <0.23.45
  # Title:     rustls accepts TLS 1.3 handshake messages at the wrong encryption level
  # Date:      2026-09-14
  # ID:        RUSTSEC-2026-0285
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0285
  # Solution:  Upgrade to >=0.23.45
  # Agave Ok:  handshake stays authenticated; no integrity or availability impact
  --ignore RUSTSEC-2026-0285

  # Crate:     rustls
  # Version:   0.23.43
  # Title:     `Debug` impls for client session values expose TLS secrets
  # Date:      2026-10-14
  # ID:        RUSTSEC-2026-0346
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0346
  # Solution:  Upgrade to >=0.23.46
  # Agave Ok:  local attacker/log-access risk accepted for v4.3.
  --ignore RUSTSEC-2026-0346

  # Crate:     rustls
  # Version:   0.23.43
  # Title:     Brotli certificate decompression can allocate up to 1 GiB before authentication
  # Date:      2026-10-14
  # ID:        RUSTSEC-2026-0348
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0348
  # Solution:  Upgrade to >=0.23.46
  # Agave Ok:  rustls's optional Brotli certificate decompression feature is not enabled.
  --ignore RUSTSEC-2026-0348

  # Crate:     rustls
  # Version:   0.23.43
  # Title:     Unauthenticated ServerHello can downgrade later connections to classical key exchange
  # Date:      2026-10-14
  # ID:        RUSTSEC-2026-0349
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0349
  # Solution:  Upgrade to >=0.23.46
  # Agave Ok:  rustls uses ring's classical key exchanges; post-quantum groups are not enabled.
  --ignore RUSTSEC-2026-0349

  # Crate:     webpki-roots
  # Version:   <1.0.10
  # Title:     Bundled root certificates allow trust in distrusted emSign intermediates
  # Date:      2026-10-19
  # ID:        RUSTSEC-2026-0381
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0381
  # Solution:  Upgrade to >=1.0.10
  # Agave Ok:  roots are only used by rustls, whose vendored rustls-webpki (anza-xyz fork, RUSTSEC-2026-0382 fix) rejects these paths
  --ignore RUSTSEC-2026-0381

  # Crate:     crypto-bigint
  # Version:   0.5.5
  # Title:     Multiple timing variability issues in operations intended to be constant-time
  # Date:      2026-10-23
  # ID:        RUSTSEC-2026-0387
  # URL:       https://rustsec.org/advisories/RUSTSEC-2026-0387
  # Solution:  Upgrade to >=0.7.6
  # Agave Ok:  xtask only reaches it via octocrab's jsonwebtoken (p256/p384);
  # xtask authenticates with a personal token and never signs JWTs.
  --ignore RUSTSEC-2026-0387
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
