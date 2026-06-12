# solana-quic-datagram

A QUIC-datagram transport designed for Solana's votor consensus traffic.
Single [`quinn::Endpoint`] per node bound to one UDP socket, playing both
client and server roles. Peer identity is the ed25519 pubkey embedded in
a self-signed TLS cert; connections are gated to staked only by an `Allowlist` trait.
Streams are disabled at the transport config level - only QUIC datagrams flow.

# Why datagrams and not streamer?

- **No HoL blocking.** Stream-based transports retransmit every message
  on the same connection when a packet is lost. A vote that didn't arrive
  within ~one slot is worthless. So for consensus messages
  losing one shouldn't delay the transmission of the next.
- **No per-message stream open/close.** Votor sends small (≤1200 byte)
  payloads at ~4/slot/peer. A stream per message is pure overhead.
- **Fire-and-forget semantics** `VotingService`'s
  `broadcast_consensus_message` already uses `try_send` with
  drop-on-full; datagrams reflect that on the wire. Votor does not
  care about packet losses as much as tower did, and has builtin
  retransmits.
- **No congestion control.** Votor traffic is not flexible, it has
  fixed bandwidth demands and can not slow down in response to
  congestion.
- **Split send/receive directions.** Each peer pair runs two
  independent unidirectional connections: one we dialed (send-only) and one we
  accepted (receive-only). We always send over our own outbound and only ever
  read from the inbound a peer dialed to us.
- **ACKs throttled.** `AckFrequencyConfig` is tuned to minimize the
  amount of ACK packets that we have to carry on the wire.

## Architecture
Two control loops, one per direction, each its own tokio task over a single
`tokio::select!`; both share one cloned quinn `Endpoint`.

`OutboundLoop::run` (we-dial, send-only) selects over:

- shutdown (`shutdown.cancelled()`)
- identity rotation (`identity_rx.changed()`)
- caller egress (`egress_rx.recv()`)
- dial outcomes from spawned dial tasks (`events_rx.recv()` -> `DialEvent`)
- metrics-report timer (every 2 s)

`InboundLoop::run` (we-accept, receive-only) selects over:

- shutdown (`shutdown.cancelled()`)
- identity rotation (`identity_rx.changed()`)
- connection-lifecycle events (`events_rx.recv()` -> `InboundEvent`)
- server-accept (`endpoint.accept()`)
- banlist-prune timer (hourly)
- metrics-report timer (every 2 s)

Statful per-connection work (TLS handshakes, dials) is spawned onto its own tasks
that report back via channels when done. The inbound (server) loop reaps a table
slot on the `Closed` event its read task emits when a connection ends. The
outbound loop has no close event: it owns each `Connection` in an LRU and reclaims
dead ones lazily - a send that returns `SendDatagramError::ConnectionLost` swaps
the slot back to `Dialing` and re-dials, and a connection idle long enough to fall
out of the LRU is evicted (and closed) when a newer peer takes its slot.

The task -> loop channels are bounded, and every task sends with `send().await`,
so all events are eventually delivered.

## Connection table

Each peer pair runs two independent unidirectional connections (one in
each direction). The connection tables serve two purposes:
* Admission control - one outbound connection per peer ID, and up to
  `MAX_INBOUND_CONNECTIONS_PER_PEER` (= 2) inbound connections per peer ID. Two
  inbounds let a same-identity hot-spare coexist with the original without a
  handover; a third is refused with `TABLE_FULL`.
* Dispatch of egress packets - egress always picks the peer's `outgoing`
  connection (dialing one if absent).

Two tables keyed by peer pubkey, each **owned exclusively by its control loop**:

```text
outgoing: LruCache<Pubkey, OutgoingEntry>  // capacity 2 × MAX_PEERS
incoming: HashMap<Pubkey, ArrayVec<Connection, MAX_INBOUND_CONNECTIONS_PER_PEER>>

enum OutgoingEntry {
    Dialing,                           // placeholder; one dial task in flight
    Established { conn: Connection },  // live send-only connection
}
```

The outbound table is an LRU rather than a HashMap and the sole owner of
each outbound `Connection`. It is critical that no client task hold any
references to `Connection` objects, else such tasks may result in quinn
resource leak (assuming server never closes our connection). As peers leave
the staked set, we can end up with some stale idle connections (but they
are of little consequence CPU cost wise), which will be reclaimed if needed
by new peers. Any idle connection that ages out of the LRU should already be
gone server-side if the server was not stuck, no explicit close code is provided
for this path.

## Split send/receive directions

There is no reuse of connections in both directions. To talk to a peer we
always dial our own `outgoing` (send-only) connection; we only ever read from
the `incoming` (receive-only) connection a peer dialed to us. Egress never
sends back over an accepted inbound.

This is somewhat simpler than a single bidirectional connection (no direction
arbitration, no "higher side waits to be dialed") at the cost of roughly
doubling the connection count.

### Minimal buffering during `Dialing`

When no connection exists to a peer, the first packet to be sent is
buffered while dialing is going on. This is critical for standstill to
deliver the cert if connection got lost while we were waiting for a
rebroadcast. Further traffic that arrives while a dial is in flight
is **dropped on the floor** (counted in `egress_dropped_dial_in_progress`).
Rationale:

- Votor sends ≤4 pkt/slot/peer (~50 ms apart). A dial-in-flight window
  loses ~a few packets; the upper layer keeps producing fresh ones.
- Buffering would require a queue - how big would it need to be?
- Old votes probably not worth delivering by the time a dial completes.
- Standstill is covered by buffering of the first packet.

### Why no `Backoff` state

A peer that's permanently down will hammer with one dial per egress to
that pubkey (votor-paced, ~5 attempts/s per peer worst case). No
exponential backoff. Trade-off: simpler state machine, marginally more
wasted dials against dead peers. Acceptable because the failure cost
(one quinn `connect` + handshake-timeout) is bounded and fairly low.

## Multiple connections per identity (no handover)

We keep up to `MAX_INBOUND_CONNECTIONS_PER_PEER` (= 2) inbound connections per
pubkey. A second handshake from an identity already present simply *coexists*
with the first - typically a hot-spare of the same identity brought up
alongside the original. We never close the prior connection or signal a
handover, so there is no displacement, no thrash, and no soft-ban; the
consensus layer already dedups any datagram delivered over both. A third
concurrent inbound from the same identity is refused with `TABLE_FULL`.

This is a deliberate trade: we carry (briefly) two connections for one identity
rather than coordinating an in-band handover and displacement. Double-vote
safety lives at the consensus/tower layer, not here.

## PEER_MOVED (address-aware eviction)

On the dialer side, if a later egress arrives with a different addr for
the same pubkey (e.g., gossip published a new addr for the peer), the
slot is atomically swapped to `Dialing`, the displaced connection is
closed with `PEER_MOVED`, and a fresh dial spawns at the new addr.

The server side **does not** do this per-pubkey addr check (the peer's
NAT-mapped source addr can legitimately differ from its gossip-published one);
inbound connections are receive-only and keyed by pubkey, not addr.

## Security posture

Loopback is exempt from the pre-handshake rate limit (for local-cluster
and tests).

**Handshake-level DoS (junk packets, spoofed-IP flood, real-IP flood).**
Before spending any TLS-handshake CPU the inbound loop charges one token to a
global `TokenBucket` (shared across all source IPs). On exhaustion the inbound is
silently `ignore()`d (no reply, no close). IPv6 and multicast sources are dropped
outright. Spoofed-IP packets cannot complete the QUIC crypto handshake so their
further impact is limited. There is no QUIC RETRY or per-IP throttling yet. These
hardening features should be considered.

**Unauthorized connections / identity spoofing.**
The peer's cert must yield a valid ed25519 pubkey. The client checks that
the attested pubkey matches the one votor targeted. The server checks the allowlist;
the pubkey must be in the current allowlist snapshot. Server-admitted connections are
re-checked every `ALLOWLIST_CHECK_INTERVAL` so peers that are no longer welcome are
evicted. This also caps the total number of incoming connections we may have to hold.

**Banlist bypass.**
A banned peer is rejected at handshake (inbound closed with `BANNED`). On an
already-established inbound the ban is enforced lazily: the next datagram the
peer sends is dropped and the connection is closed with `BANNED` (an idle
established connection is not torn down until its next packet). On the egress
side a datagram addressed to a banned peer is dropped before send; an existing
send-only outbound to that peer is left to expire on idle-timeout rather than
being actively closed.

**Connection flood from allowed peers.**
At most `MAX_INBOUND_CONNECTIONS_PER_PEER` inbound connections per pubkey;
further ones are refused with `TABLE_FULL`. The total number of distinct inbound peers is
bounded by the allowlist (only staked pubkeys are admitted), so no absolute
connection-count ceiling is needed. `MAX_PEERS` is just a sizing constant for
buffers and channels.

**Datagram flood over an open connection.**
Each accepted connection runs a per-connection RX token bucket. Bursts within
a votor-appropriate rate are tolerated, anything above that is silently dropped
(counted in `datagram_rate_limited`) while the connection stays alive - consensus
traffic legitimately bursts above the refill rate.
A peer that drains the bucket entirely (`PEER_RATE_LIMIT_BURST_DOS`) is treated
as an attacker, banned for `BAN_DURATION_DOS`, and closed with `BANNED`.

**Stale connections after epoch transitions.**
Allowlist membership is re-checked for every connection every
`ALLOWLIST_CHECK_INTERVAL`.

**Resource exhaustion / OOM.**
Inbound peers are bounded by the staked allowlist, with a hard per-peer cap of
`MAX_INBOUND_CONNECTIONS_PER_PEER` (= 2) connections. The pre-TLS handshake rate
limit prevents CPU exhaustion from connection attempts.

**Upstream (votor) stalls from a misbehaving peer.**
All external connections are via channels and/or atomics.

### Known-untested mitigations

One mitigation has no automated coverage because the loopback test harness
cannot reach it without a code seam:

- **Global pre-handshake rate limit.** Loopback is exempt from this limiter and
  every test runs on loopback, so `handshake_rejected_global_limit` is never
  exercised.

## Identity rotation

`KeyUpdater::update_key(new_keypair)` publishes a new `IdentitySnapshot`
through a `tokio::sync::watch`. Both loops observe the change independently and
each:

1. Rebuilds its TLS config (outbound: client config; inbound: server config).
2. Swaps it into the shared quinn endpoint.
3. Bumps its own `generation` counter to invalidate in-flight handshakes.
4. Evicts every connection it owns with an `IDENTITY_ROTATED` close.
5. (Outbound) adopts the new pubkey.

Every spawned task is stamped with the `generation` current when it was spawned
and echoes it back in its event. Each loop's `handle_event` drops any event
whose generation no longer matches, closing any connection it carries with
`IDENTITY_ROTATED`.

## Egress / ingress channels

- **Egress** (caller → endpoint, `EGRESS_CHANNEL_CAP = 4 × MAX_PEERS = 8000`).
  Bounded mpsc. Callers must `try_send` and accept drop-on-full -
  matches `VotingService::broadcast_consensus_message`. Sized to absorb
  a full slot's burst with headroom.
- **Ingress** (endpoint → caller, caller-supplied
  `crossbeam_channel::Sender`). Drop-on-full counted in
  `datagram_ingress_dropped_channel_full`.

### Epoch boundary handling

At the end of an epoch, we may experience churn in the allowlist.
Some peers will no longer be allowed, while some new ones will be
admitted. We will eventually break connections to peers which are
no longer in the allowlist. This keeps the logic simpler than the
explicit notify during epoch change.

## What this crate is not

- Not a general-purpose datagram transport. All decisions assume a
  votor workload (small peer set, predictable cadence, no retransmits).
- Not a buffered transport. There is no per-peer queue. Bytes that have
  nowhere to go are gone.
