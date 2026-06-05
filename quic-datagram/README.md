# solana-quic-datagram

A QUIC transport for Solana's votor consensus traffic (votes and certificates).
Each node runs a single QUIC endpoint that acts as both client and server and
exchanges only QUIC datagrams — QUIC streams are disabled. Peers are identified
by the ed25519 pubkey in a self-signed TLS certificate, and only staked peers are
admitted.

The crate replaces the stream-based QUIC path previously used for votor messages.
Consensus traffic is small, high-cadence, and loss-tolerant, and a reliable,
stream-oriented transport is a poor fit for it.

## Why datagrams instead of streams

- **No head-of-line blocking.** A lost packet on a QUIC stream stalls every
  later message on that connection. A consensus vote that misses its slot is
  already worthless, so one lost message must never delay the next.
- **No per-message stream overhead.** Votor emits many small messages; opening
  and closing a stream per message is pure cost for no benefit.
- **Loss-tolerant, fire-and-forget.** Votor sends best-effort and rebroadcasts
  what matters, so it does not need reliable, ordered delivery. Datagrams give
  exactly those semantics, and the transport drops — rather than buffers — what
  it cannot send immediately, so it never backpressures vote production.
- **No flow control to fight.** Votor has fixed bandwidth demands and cannot
  slow down for a peer; a reliable transport's congestion and flow control would
  only get in the way.

## Model

- One endpoint per node, bound to a single UDP socket, playing both roles.
- Send and receive use separate one-way connections: a node sends over the
  connection it dialed and receives over the connection the peer dialed to it.
  This keeps the two directions independent, at the cost of more connections.
- Admission is gated by a staked-peer allowlist. Identity is the pubkey attested
  by the peer's TLS certificate, so a sender cannot impersonate another peer.
- Delivery is best-effort end to end: full queues drop rather than block.

## Security posture

- **Admission.** Only allowlisted (staked) peers are accepted, and admission is
  re-checked over a connection's lifetime so peers that leave the staked set are
  dropped.
- **Identity.** Every connection is bound to the peer's attested pubkey; a
  banlist rejects or tears down connections from banned peers.
- **Flood resistance.** Inbound handshakes are rate-limited before any expensive
  cryptography, and per-peer datagram rates are bounded — excess datagrams are
  dropped and abusive connections are closed. Total resource use is bounded by
  the (capped) staked set.

## Identity rotation

A node's identity (keypair and TLS certificate) can be rotated at runtime;
existing connections are re-established under the new identity.

## What this crate is not

- Not a general-purpose datagram transport — every decision assumes a votor
  workload (small peer set, predictable cadence, no retransmits).
- Not a buffered transport — there is no per-peer queue; bytes that have nowhere
  to go are dropped.
