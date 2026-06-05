use {
    crate::{
        MAX_PEERS, close_codes,
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

/// Per-peer connection table. Holds up to [`crate::MAX_PEERS`] entries.
///
/// Each entry is a [`ConnectionTableEntry`] - either `Dialing` (placeholder
/// while a single in-flight dial task brings the connection up) or
/// `Established` (a live [`quinn::Connection`] for one peer). A new
/// handshake from a pubkey already in `Established` closes the old
/// connection with `HANDOVER` and installs the new one - we assume that
/// the remote validator is a hot-spare that was just brought up.
/// The displaced side observes `HANDOVER` in its
/// read loop, soft-bans the peer, and reports the event to the driving
/// service (e.g. votor).
pub(crate) struct ConnectionTable {
    inner: HashMap<Pubkey, ConnectionTableEntry>,
}

/// State of a peer's entry in the connection table.
pub(crate) enum ConnectionTableEntry {
    /// A placeholder installed by the dialer side before spawning a dial
    /// task. Exactly one dial is ever in flight per peer (later egress sees
    /// `Dialing` and drops), so the placeholder needs no further tagging.
    Dialing,
    /// Holds the live connection for fan-out.
    Established(Connection),
}

/// Outcome of attempting to insert a fresh connection.
pub(crate) enum InsertOutcome {
    /// Table had no prior entry (or held our own `Dialing` placeholder);
    /// the new connection takes the slot.
    Inserted,
    /// Table already held an `Established` connection for this peer. The
    /// old one was closed with `HANDOVER`; the new one took its place.
    Replaced,
    /// Table was at [`MAX_PEERS`] and this is a fresh pubkey. Caller must
    /// close the connection with `TABLE_FULL`.
    Rejected,
}

/// Outcome of [`ConnectionTable::send_over_outbound_connection`].
pub(crate) enum EgressDispatch {
    /// A cached `Established` for the requested addr existed; the datagram
    /// was queued on it inside the method (the caller does nothing more).
    Sent,
    /// Another dial task is already in flight for this peer; the datagram
    /// was dropped (counted in `egress_dropped_dial_in_progress`).
    Dialing,
    /// No usable connection. The slot now holds `Dialing` (placeholder
    /// installed by this call, possibly after closing a stale `Established`
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

impl ConnectionTable {
    /// Create a new empty connection table.
    pub(crate) fn new() -> Self {
        Self {
            inner: HashMap::new(),
        }
    }

    /// Current number of active entries.
    pub(crate) fn len(&self) -> u64 {
        self.inner.len() as u64
    }

    /// Register an `Established` connection for `peer`. Returns:
    /// - `Inserted` on a fresh slot (or one previously holding our own
    ///   `Dialing` placeholder).
    /// - `Replaced` if a prior `Established` was displaced (HANDOVER); the
    ///   old connection is closed inside this method.
    /// - `Rejected` if the table is at [`MAX_PEERS`] and this is a fresh
    ///   pubkey; caller must close `conn` with `TABLE_FULL`. This should
    ///   never happen during normal operation.
    pub(crate) fn insert_connection(
        &mut self,
        peer: Pubkey,
        conn: Connection,
        stats: &QuicDatagramStats,
    ) -> InsertOutcome {
        let at_cap = self.inner.len() as u64 >= MAX_PEERS;
        match self.inner.entry(peer) {
            Entry::Vacant(slot) => {
                if at_cap {
                    return InsertOutcome::Rejected;
                }
                slot.insert(ConnectionTableEntry::Established(conn));
                InsertOutcome::Inserted
            }
            Entry::Occupied(mut slot) => {
                let old =
                    std::mem::replace(slot.get_mut(), ConnectionTableEntry::Established(conn));
                match old {
                    // Our own placeholder; just transition state - not a handover.
                    ConnectionTableEntry::Dialing => InsertOutcome::Inserted,
                    // A prior live connection for this pubkey. Normal during
                    // handover.
                    ConnectionTableEntry::Established(old_conn) => {
                        // close the displaced connection with appropriate code.
                        close_codes::HANDOVER.close(&old_conn);
                        stats
                            .connection_replaced_handover
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        InsertOutcome::Replaced
                    }
                }
            }
        }
    }

    /// Transmit over the cached inbound connection for `peer`.
    /// Returns `true` iff a cached `Established` was found and the send was
    /// attempted; `false` if the slot is empty.
    pub(crate) fn send_over_inbound_connection(
        &self,
        peer: &Pubkey,
        bytes: &Bytes,
        stats: &QuicDatagramStats,
    ) -> bool {
        let Some(entry) = self.inner.get(peer) else {
            return false;
        };
        match entry {
            ConnectionTableEntry::Established(conn) => {
                match conn.send_datagram(bytes.clone()) {
                    Ok(()) => add(&stats.datagrams_sent),
                    Err(e) => record_error(&Error::from(e), stats),
                }
                true
            }
            ConnectionTableEntry::Dialing => {
                debug_assert!(
                    false,
                    "lex-higher side should never hold Dialing for lower pubkey"
                );
                false
            }
        }
    }

    /// Outbound connection (dialer) packet dispatch.
    /// Encapsulates the 4-case decision over the table entry for `peer`:
    /// * vacant -> initiate new connection,
    /// * dial in progress -> drop,
    /// * established -> send,
    /// * established to wrong address: evict + ask for redial.
    pub(crate) fn send_over_outbound_connection(
        &mut self,
        peer: Pubkey,
        addr: SocketAddr,
        bytes: &Bytes,
        stats: &QuicDatagramStats,
    ) -> EgressDispatch {
        match self.inner.entry(peer) {
            Entry::Vacant(slot) => {
                slot.insert(ConnectionTableEntry::Dialing);
                // Trigger packet is carried into the dial task and sent
                // the moment the connection lands - the cold-start /
                // standstill case needs it. Subsequent followers during
                // `Dialing` are dropped (see the `Dialing` arm below).
                EgressDispatch::SpawnDialTask
            }
            Entry::Occupied(mut slot) => match slot.get() {
                ConnectionTableEntry::Dialing => {
                    stats
                        .egress_dropped_dial_in_progress
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    EgressDispatch::Dialing
                }
                ConnectionTableEntry::Established(conn) if conn.remote_address() == addr => {
                    match conn.send_datagram(bytes.clone()) {
                        Ok(()) => add(&stats.datagrams_sent),
                        Err(e) => record_error(&Error::from(e), stats),
                    }
                    EgressDispatch::Sent
                }
                ConnectionTableEntry::Established(_) => {
                    // Peer moved - swap the slot to `Dialing`...
                    let old = std::mem::replace(slot.get_mut(), ConnectionTableEntry::Dialing);
                    // ... and close the displaced connection with PEER_MOVED.
                    if let ConnectionTableEntry::Established(old_conn) = old {
                        close_codes::PEER_MOVED.close(&old_conn);
                        stats
                            .connection_evicted_peer_moved
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        info!("peer {peer} moved; re-dialing at {addr}");
                    }
                    // Trigger packet is carried into the new dial task and
                    // sent the moment the new connection lands at `addr`.
                    EgressDispatch::SpawnDialTask
                }
            },
        }
    }

    /// Remove the slot for `peer` if and only if it currently holds an
    /// `Established` with the given `stable_id`. No-op if the slot holds
    /// a different connection (HANDOVER replacement landed) or a
    /// `Dialing` placeholder.
    pub(crate) fn maybe_reap_connection(&mut self, peer: &Pubkey, stable_id: usize) {
        if let Entry::Occupied(entry) = self.inner.entry(*peer)
            && matches!(entry.get(), ConnectionTableEntry::Established(c) if c.stable_id() == stable_id)
        {
            entry.remove();
        }
    }

    /// Remove the slot for `peer` iff it still holds a `Dialing`
    /// placeholder. Called when a dial gives up. No-op if some other path
    /// already replaced the slot with an `Established`.
    pub(crate) fn clear_dialing_placeholder(&mut self, peer: &Pubkey) {
        if let Entry::Occupied(slot) = self.inner.entry(*peer)
            && matches!(slot.get(), ConnectionTableEntry::Dialing)
        {
            slot.remove();
        }
    }

    /// Wipe the table on identity rotation. Every `Established` is closed
    /// with `IDENTITY_ROTATED` on the way out; `Dialing` placeholders are
    /// dropped. The control loop bumps its own [`IdGeneration`] alongside
    /// this call, so any in-flight dial / accept whose handshake completes
    /// afterwards is dropped at the event boundary and never reinstalled.
    /// Returns the count of entries dropped.
    pub(crate) fn clear_for_id_change(&mut self) -> u64 {
        let evicted = self.inner.len() as u64;
        for entry in self.inner.values() {
            if let ConnectionTableEntry::Established(conn) = entry {
                close_codes::IDENTITY_ROTATED.close(conn);
            }
        }
        self.inner.clear();
        evicted
    }
}
