use {
    crate::{
        close_codes,
        error::Error,
        stats::{QuicDatagramStats, add, record_error},
    },
    bytes::Bytes,
    log::info,
    quinn::Connection,
    solana_pubkey::Pubkey,
    std::{
        collections::{HashMap, hash_map::Entry},
        net::SocketAddr,
    },
};

/// States of all existing peer connections. This can grow up to the
/// total amount of peers in the Allowlist, + a few that were allowed
/// but are not yet evicted.
pub(crate) struct PeerStates {
    inner: HashMap<Pubkey, PeerConnectionState>,
}

/// Per-peer connection state machine.
pub(crate) enum PeerConnectionState {
    /// A placeholder installed by the dialer side before spawning a dial
    /// task. Exactly one dial is ever in flight per peer (later egress sees
    /// `Dialing` and drops), so the placeholder needs no further tagging.
    Dialing,
    /// The peer dialed us; we accepted the handshake.
    Inbound(Connection),
    /// We dialed the peer.
    Outbound(Connection),
}

/// Outcome of attempting to insert a fresh connection.
pub(crate) enum InsertOutcome {
    /// Table had no prior entry (or held our own `Dialing` placeholder);
    /// the new connection takes the slot.
    Inserted,
    /// Table already held an established connection for this peer. The
    /// old one was closed; the new one took its place.
    Replaced,
    /// The table already holds the canonical connection for this peer;
    /// the new connection is not canonical. Caller must close it.
    LexLoser,
}

/// Outcome of [`PeerStates::try_send`].
pub(crate) enum EgressDispatch {
    /// A cached connection for the requested peer existed; the datagram
    /// was queued on it inside the method (the caller does nothing more).
    Sent,
    /// Another dial task is already in flight for this peer; the datagram
    /// was dropped (counted in `egress_dropped_dial_in_progress`).
    Dialing,
    /// No usable connection. The slot now holds `Dialing` (placeholder
    /// installed by this call, possibly after closing a stale `Outbound`
    /// with `PEER_MOVED`). Caller is expected to spawn a dial task.
    SpawnDialTask,
}

/// Monotonic counter incremented on every identity rotation. Owned by the
/// control loop. Spawned dial / accept / read tasks capture the generation
/// live when they were spawned and echo it back in their
/// [`crate::connection::ConnEvent`]; the loop drops any event whose
/// generation no longer matches, so a connection authenticated under a
/// previous cert never reaches this table.
pub(crate) type IdGeneration = u64;

impl PeerStates {
    pub(crate) fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    /// Current number of active entries.
    pub(crate) fn len(&self) -> u64 {
        self.inner.len() as u64
    }

    /// Register a fresh `Inbound` or `Outbound` connection for `peer`.
    ///
    /// Returns:
    /// - `Inserted` on a fresh slot (or one previously holding our own
    ///   `Dialing` placeholder).
    /// - `Replaced` if a prior established connection was displaced; it is
    ///   closed inside this method (`HANDOVER` for same-direction replacements,
    ///   `WRONG_DIRECTION` when the canonical connection evicts a non-canonical one).
    /// - `LexLoser` if the table already holds the canonical connection for
    ///   `peer`; `state` is NOT inserted. Caller must close it.
    pub(crate) fn insert_connection(
        &mut self,
        peer: Pubkey,
        state: PeerConnectionState,
        my_pubkey: &Pubkey,
        stats: &QuicDatagramStats,
    ) -> InsertOutcome {
        match self.inner.entry(peer) {
            Entry::Vacant(slot) => {
                slot.insert(state);
                InsertOutcome::Inserted
            }
            Entry::Occupied(mut slot) => {
                // Sample old direction without keeping a borrow on the slot.
                let old_is_outbound = match slot.get() {
                    PeerConnectionState::Dialing => {
                        *slot.get_mut() = state;
                        return InsertOutcome::Inserted;
                    }
                    PeerConnectionState::Outbound(_) => true,
                    PeerConnectionState::Inbound(_) => false,
                };

                let new_is_outbound = matches!(state, PeerConnectionState::Outbound(_));
                let canonical_is_outbound = my_pubkey < &peer;

                if old_is_outbound == new_is_outbound {
                    // Same direction: peer reconnected (HANDOVER — normal for hot-spare).
                    let old = std::mem::replace(slot.get_mut(), state);
                    if let PeerConnectionState::Inbound(c) | PeerConnectionState::Outbound(c) = old
                    {
                        close_codes::HANDOVER.close(&c);
                        stats
                            .connection_replaced_handover
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    InsertOutcome::Replaced
                } else if new_is_outbound == canonical_is_outbound {
                    // New connection is canonical; replace the non-canonical incumbent.
                    let old = std::mem::replace(slot.get_mut(), state);
                    if let PeerConnectionState::Inbound(c) | PeerConnectionState::Outbound(c) = old
                    {
                        close_codes::WRONG_DIRECTION.close(&c);
                        stats
                            .connections_lex_loser_closed
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    InsertOutcome::Replaced
                } else {
                    // Existing connection is canonical; new connection lost the lex race.
                    InsertOutcome::LexLoser
                }
            }
        }
    }

    /// Packet dispatch for egress to `peer`. Encodes state transitions:
    /// * vacant → install `Dialing` placeholder, ask caller to spawn a dial task,
    /// * dial in progress → drop (counted in `egress_dropped_dial_in_progress`),
    /// * inbound → send (gossip addr ignored; peer's NAT source addr is authoritative),
    /// * outbound to matching `addr` → send,
    /// * outbound to wrong `addr` → evict with `PEER_MOVED`, ask caller to re-dial.
    pub(crate) fn try_send(
        &mut self,
        peer: Pubkey,
        addr: SocketAddr,
        bytes: &Bytes,
        stats: &QuicDatagramStats,
    ) -> EgressDispatch {
        match self.inner.entry(peer) {
            Entry::Vacant(slot) => {
                slot.insert(PeerConnectionState::Dialing);
                EgressDispatch::SpawnDialTask
            }
            Entry::Occupied(mut slot) => match slot.get() {
                PeerConnectionState::Dialing => {
                    stats
                        .egress_dropped_dial_in_progress
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    EgressDispatch::Dialing
                }
                PeerConnectionState::Inbound(conn) => {
                    match conn.send_datagram(bytes.clone()) {
                        Ok(()) => add(&stats.datagrams_sent),
                        Err(e) => record_error(&Error::from(e), stats),
                    }
                    EgressDispatch::Sent
                }
                PeerConnectionState::Outbound(conn) if conn.remote_address() == addr => {
                    match conn.send_datagram(bytes.clone()) {
                        Ok(()) => add(&stats.datagrams_sent),
                        Err(e) => record_error(&Error::from(e), stats),
                    }
                    EgressDispatch::Sent
                }
                PeerConnectionState::Outbound(_) => {
                    let old = std::mem::replace(slot.get_mut(), PeerConnectionState::Dialing);
                    if let PeerConnectionState::Outbound(old_conn) = old {
                        close_codes::PEER_MOVED.close(&old_conn);
                        stats
                            .connection_evicted_peer_moved
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        info!("peer {peer} moved; re-dialing at {addr}");
                    }
                    EgressDispatch::SpawnDialTask
                }
            },
        }
    }

    /// Remove the slot for `peer` if it currently holds an established
    /// connection with the given `stable_id`.
    pub(crate) fn maybe_reap_connection(&mut self, peer: &Pubkey, stable_id: usize) {
        if let Entry::Occupied(entry) = self.inner.entry(*peer)
            && matches!(
                entry.get(),
                PeerConnectionState::Inbound(c) | PeerConnectionState::Outbound(c)
                    if c.stable_id() == stable_id
            )
        {
            entry.remove();
        }
    }

    /// Remove the slot for `peer` iff it still holds a `Dialing`
    /// placeholder. Called when a dial gives up.
    pub(crate) fn clear_dialing_placeholder(&mut self, peer: &Pubkey) {
        if let Entry::Occupied(slot) = self.inner.entry(*peer)
            && matches!(slot.get(), PeerConnectionState::Dialing)
        {
            slot.remove();
        }
    }

    /// Wipe the table on identity rotation. Every established connection is
    /// closed with `IDENTITY_ROTATED`; `Dialing` placeholders are dropped.
    /// The control loop bumps its own [`IdGeneration`] alongside this call,
    /// so any in-flight dial / accept whose handshake completes afterwards is
    /// dropped at the event boundary and never reinstalled. Returns the count
    /// of entries dropped.
    pub(crate) fn clear_for_id_change(&mut self) -> u64 {
        let evicted = self.inner.len() as u64;
        for entry in self.inner.values() {
            if let PeerConnectionState::Inbound(conn) | PeerConnectionState::Outbound(conn) = entry
            {
                close_codes::IDENTITY_ROTATED.close(conn);
            }
        }
        self.inner.clear();
        evicted
    }
}
