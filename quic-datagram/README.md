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
- **One connection per peer** unlike existing solutions used for TPU, this
  allows each connection to be used in both directions, reducing overheads.
- **ACKs throttled.** `AckFrequencyConfig` is tuned to minimize the
  amount of ACK packets that we have to carry on the wire.

## Architecture
This is almost single threaded. One tokio task - `endpoint::EndpointLoop::run`
multiplexes every event source via a single `tokio::select!`:

- identity rotation (`identity_rx.changed()`)
- client egress (`egress_rx.recv()`)
- connection-lifecycle events from spawned tasks (`conn_events_rx.recv()`)
- server-accept (`endpoint.accept()`)
- banlist-prune timer (hourly)
- metrics-report timer (every 2 s)

Heavy per-event work (TLS handshakes, dials) is spawned onto its own tasks.
Those tasks do the necessary TLS and IO work; then report back to the event loop
as a `ConnEvent`, event loop then evaluates all state transitions.  On a
successful handshake / dial the event loop spawns the per-connection
read task, which in turn reports `ConnEvent::Closed` when it exits.

The task -> event loop channel is bounded, and every task sends with `send().await`.
This can never deadlock: the event loop is the only consumer and never sends
into it.

## Peer state map

A plain `HashMap<Pubkey, PeerConnectionState>` **owned exclusively by the
control loop**, where:

```text
enum PeerConnectionState {
    Dialing,
    Inbound(Connection),
    Outbound(Connection),
}
```


Table serves two objectives:
* Admission control - we do not allow > 1 connection per peer ID, votor has no need
  to allow more.
* Dispatch of egress packets - we need to pick a connection to send packets over.

We could split the single table into 2 (one for egress, one for ingress) if we
use each connection in one direction. This does not notably simplify the implementation.

### Pubkey based connection direction tiebreaker

For every peer pair, the node with the **lower** pubkey is the canonical
dialer; in case connection attempts race, the one in which higher pubkey listens wins.
The rule is enforced after handshakes complete only, so both handshakes
race and if one completes before other is initiated, it is kept.

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

## Handover

A new successful handshake from a pubkey already in `Established`
closes the prior connection with `HANDOVER` and installs the new one.
The displaced side observes `HANDOVER` in its read loop and soft-bans
the evicting peer so it does not evict the new node while waiting for
the gossip notification to kill itself.

## PEER_MOVED (address-aware eviction)

On the dialer side, if a later egress arrives with a different addr for
the same pubkey (e.g., gossip published a new addr for the peer), the
slot is atomically swapped to `Dialing`, the displaced connection is
closed with `PEER_MOVED`, and a fresh dial spawns at the new addr.

The server side **does not** do this per-pubkey addr check. It does,
however, gate inbound on the source *IP* being a
gossip-advertised address of a staked peer - see the admission gate in the
Security posture below.

## Security posture

1. **QUIC RETRY** (mandatory). Every inbound is bounced through a RETRY.
   Blocks address-spoofed handshake floods - an attacker without a working
   return path completes nothing and does not burn our CPU.
2. **Gossip-IP admission gate.** Post-RETRY (source addr is now validated),
   the *only* sources we spend TLS-handshake CPU on are those whose IP is a
   gossip-advertised address of a staked peer (`Allowlist::allow_ip`). Every
   other validated source is refused before the handshake. This is airtight
   against handshake-CPU DoS: an unstaked attacker cannot place its IP in our
   gossip-derived set, and cannot borrow a staked validator's IP because RETRY
   proves return-routability. NAT'd peers must advertise their external IP in
   gossip to be admitted. Loopback is exempt (local-cluster / tests). No IPv6
   to keep this easy. (This replaces an earlier per-/24 rate limiter, now
   unnecessary since unrecognized sources never reach the handshake.)
3. **Per-IP handshake throttle.** Even an admitted source gets at most one
   in-flight handshake: a fresh start is refused while a prior one from the
   same IP could still be running (window = `MAX_IDLE_TIMEOUT`, the ceiling on
   a pending handshake). Bounds handshake-CPU abuse from a buggy or compromised
   staked peer. Loopback is exempt (local-cluster / tests).
4. **Bidirectional TLS ID validation.** Peer's cert must yield a
   recoverable Solana ed25519 pubkey; client checks the attested pubkey
   matches the targeted one, server checks that client owns a staked
   identity.
5. **Stake check**  Peer must be in the current staked-set
   snapshot.
6. **External banlist** (`DatagramBanlist`). Application-level bans
   (e.g., BLS sigverify failures) are funneled through here; the
   evictor task reaps every cached connection for the banned pubkey.
7. **Per-connection RX token bucket.** Bursts above
   `BURST_DATAGRAMS_PER_SECOND_PER_PEER` are silently dropped (the
   bucket itself is the throttle). The connection stays alive; the
   peer is **not** banned (consensus traffic legitimately bursts).
   If the peer keeps flooding we will drop connection and ban it.
8. **MAX_PEERS = 4000.** Defensive cap on the connection table size.


## Identity rotation

`KeyUpdater::update_key(new_keypair)` publishes a new
`IdentitySnapshot` through a `tokio::sync::watch`. The control loop
observes the change and:

1. Rebuilds server + client TLS configs.
2. Swaps them into the quinn endpoint.
3. Bumps its `IdGeneration` counter to invalidate ongoing handshakes.
4. Evicts every existing connection with `IDENTITY_ROTATED` close.
5. Adopts the new pubkey.

Every spawned task is stamped with the IdFeneration when it was spawned and
echoes it back in its `ConnEvent`. The event loop's `handle_conn_event` drops any
event whose generation no longer matches, closing any connection it carries with
`IDENTITY_ROTATED`.

## Egress / ingress channels

- **Egress** (caller → endpoint, `EGRESS_CHANNEL_CAP = 16384`).
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
explicit notify during epoch change. We easily accommodate extra connections
in the allowlist since MAX_PEERS = 2x of what votor needs.

## What this crate is not

- Not a general-purpose datagram transport. All decisions assume a
  votor workload (small peer set, predictable cadence, no retransmits).
- Not a buffered transport. There is no per-peer queue. Bytes that have
  nowhere to go are gone.
