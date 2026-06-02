use {
    crate::{
        MAX_PEERS, close_codes,
        error::Error,
        stats::{QuicDatagramStats, record_error},
    },
    bytes::Bytes,
    dashmap::{DashMap, mapref::entry::Entry},
    log::info,
    quinn::Connection,
    solana_pubkey::Pubkey,
    std::{
        net::SocketAddr,
        sync::atomic::{AtomicU64, Ordering},
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
///
/// Backed by [`DashMap`] - multiple tasks (server-accept, client-egress,
/// per-connection read loops, banlist evictor) hammer the table concurrently.
///
/// INVARIANT: no code path can hold a `Ref`/`RefMut` from the DashMap across
/// an `.await` - DashMap entry locks held across yield points will deadlock
/// the tokio runtime under contention. All DashMap access is encapsulated
/// inside this module. Every public method is a plain `fn` (never `async fn`)
/// and never returns a [`dashmap::mapref`] guard, so the "no `Ref`/`RefMut`
/// held across `.await`" invariant is enforced.
pub(crate) struct ConnectionTable {
    inner: DashMap<Pubkey, ConnectionTableEntry>,
    /// See [`IdGeneration`]. Bumped by [`Self::clear`].
    generation: AtomicU64,
}

/// State of a peer's entry in the connection table.
pub(crate) enum ConnectionTableEntry {
    /// A placeholder inserted by the dialer side before spawning a dial
    /// task. Tagged with the [`IdGeneration`] live at insert time so a
    /// stale (post-rotation) cleanup from an old dial task can't clobber
    /// a fresh placeholder installed by a new dial task at a later
    /// generation.
    Dialing(IdGeneration),
    /// Holds the live connection for fan-out.
    Established(Connection),
}

/// Outcome of attempting to insert a fresh incoming connection.
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
    /// Identity rotated between the start of the dial / accept and this
    /// insert; the connection is authenticated under our previous cert
    /// and must not be installed. Caller closes with `IDENTITY_ROTATED`.
    Stale,
}

/// Outcome of [`ConnectionTable::dispatch_outbound`].
pub(crate) enum EgressDispatch {
    /// A cached `Established` for the requested addr existed; the datagram
    /// was queued on it inside the method (the caller does nothing more).
    Sent,
    /// Another dial task is already in flight for this peer; the datagram
    /// was dropped (counted in `egress_dropped_dial_in_progress`).
    Dialing,
    /// No usable connection. The slot now holds `Dialing` (placeholder
    /// installed by this call, possibly after closing a stale
    /// `Established` with `PEER_MOVED`). Caller is expected to spawn a
    /// dial task and pass the returned [`IdGeneration`] to
    /// [`ConnectionTable::insert_connection`] and
    /// [`ConnectionTable::clear_dialing_placeholder`].
    SpawnDialTask { generation: IdGeneration },
}

/// Monotonic counter incremented on every identity rotation. In-flight
/// dial / accept tasks capture the generation at start and pass it into
/// [`ConnectionTable::insert_connection`]; a mismatch at insert time
/// means our identity changed mid-handshake and the resulting connection
/// is authenticated under our previous cert. We should close such
/// connections with `IDENTITY_ROTATED`.
pub(crate) type IdGeneration = u64;

impl ConnectionTable {
    /// Create a new empty connection table.
    pub(crate) fn new() -> Self {
        Self {
            inner: DashMap::new(),
            generation: AtomicU64::new(0),
        }
    }

    /// Snapshot the current [`IdGeneration`]. Server-accept and dial
    /// paths read this at the start of their handshake and pass it back
    /// into [`Self::insert_connection`] and (on the dial side)
    /// [`Self::clear_dialing_placeholder`] so a rotation that happens
    /// mid-operation can be detected.
    pub(crate) fn current_generation(&self) -> IdGeneration {
        // this happens rarely, SeqCst is ok
        self.generation.load(Ordering::SeqCst)
    }

    /// Register an `Established` connection for `peer`. Server-accept and
    /// dial-completion paths both call this with the [`IdGeneration`]
    /// they snapshotted at handshake start. Returns:
    /// - `Inserted` on a fresh slot (or one previously holding our own
    ///   `Dialing` placeholder at the same generation).
    /// - `Replaced` if a prior `Established` was displaced (HANDOVER); the
    ///   old connection is closed inside this method.
    /// - `Rejected` if the table is at [`MAX_PEERS`] and this is a fresh
    ///   pubkey; caller must close `conn` with `TABLE_FULL`. This should
    ///   never happen during normal operation.
    /// - `Stale` if our identity rotated between the handshake start and
    ///   this call; the connection is authenticated under our old cert
    ///   and the caller must close it with `IDENTITY_ROTATED`.
    pub(crate) fn insert_connection(
        &self,
        peer: Pubkey,
        conn: Connection,
        gen_at_start: IdGeneration,
        stats: &QuicDatagramStats,
    ) -> InsertOutcome {
        if self.current_generation() != gen_at_start {
            return InsertOutcome::Stale;
        }
        // CRITICAL: check `len()` BEFORE acquiring an `Entry`. Holding an
        // entry pins one of DashMap's internal shard locks; `len()` then
        // iterates over every shard and would deadlock on its own shard
        // (std::sync::RwLock is not reentrant).
        //
        // The MAX_PEERS check is racy under concurrent inserts to different
        // vacant keys - going slightly over is acceptable.
        let at_cap = self.inner.len() >= MAX_PEERS;
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
                    ConnectionTableEntry::Dialing(_) => InsertOutcome::Inserted,
                    // A prior live connection for this pubkey. Normal during
                    // handover.
                    ConnectionTableEntry::Established(old_conn) => {
                        // close the displaced connection with appropriate code.
                        close_codes::HANDOVER.close(&old_conn);
                        stats
                            .connection_replaced_handover
                            .fetch_add(1, Ordering::Relaxed);
                        InsertOutcome::Replaced
                    }
                }
            }
        }
    }

    /// Like [`Self::insert_connection`], but on cap-induced `Rejected` first
    /// tries to free a slot by evicting an entry whose peer is no longer
    /// admitted by `allow`, then retries the insert. Returns the final
    /// outcome; `Rejected` only if the table is at cap AND no unadmitted
    /// entry could be found to displace.
    ///
    /// This is the canonical entry point for handshake-complete inserts on
    /// both the dial and accept paths. Direct `insert_connection` callers
    /// are tests / contexts where eviction policy isn't applicable.
    pub(crate) fn insert_connection_or_evict<F: Fn(&Pubkey) -> bool>(
        &self,
        peer: Pubkey,
        conn: Connection,
        gen_at_start: IdGeneration,
        allow: F,
        stats: &QuicDatagramStats,
    ) -> InsertOutcome {
        // `Connection` is Arc-backed; the clone here is just an Arc bump
        // so we can retry on the eviction-success path.
        let outcome = self.insert_connection(peer, conn.clone(), gen_at_start, stats);
        if matches!(outcome, InsertOutcome::Rejected) && self.evict_one_unadmitted(allow, stats) {
            self.insert_connection(peer, conn, gen_at_start, stats)
        } else {
            outcome
        }
    }

    /// Transmit over the cached inbound connection for `peer` (lex-higher side).
    /// Returns `true` iff a cached `Established` was found and the send was
    /// attempted; `false` if the slot is empty. The lex-higher side never
    /// installs `Dialing` placeholders, so that arm is impossible by
    /// construction and trips a `debug_assert` if hit. Per-error counters
    /// (`send_datagram_error_*`) are bumped via `record_error` on quinn-level
    /// send failures.
    pub(crate) fn send_over_inbound_connection(
        &self,
        peer: &Pubkey,
        bytes: &Bytes,
        stats: &QuicDatagramStats,
    ) -> bool {
        let Some(entry) = self.inner.get(peer) else {
            return false;
        };
        match entry.value() {
            ConnectionTableEntry::Established(conn) => {
                if let Err(e) = conn.send_datagram(bytes.clone()) {
                    record_error(&Error::from(e), stats);
                }
                true
            }
            ConnectionTableEntry::Dialing(_) => {
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
        &self,
        peer: Pubkey,
        addr: SocketAddr,
        bytes: &Bytes,
        stats: &QuicDatagramStats,
    ) -> EgressDispatch {
        let generation = self.current_generation();
        match self.inner.entry(peer) {
            Entry::Vacant(slot) => {
                slot.insert(ConnectionTableEntry::Dialing(generation));
                // Trigger packet is carried into the dial task and sent
                // the moment the connection lands - the cold-start /
                // standstill case needs it. Subsequent followers during
                // `Dialing` are dropped (see the `Dialing` arm below).
                EgressDispatch::SpawnDialTask { generation }
            }
            Entry::Occupied(mut slot) => match slot.get() {
                ConnectionTableEntry::Dialing(_) => {
                    stats
                        .egress_dropped_dial_in_progress
                        .fetch_add(1, Ordering::Relaxed);
                    EgressDispatch::Dialing
                }
                ConnectionTableEntry::Established(conn) if conn.remote_address() == addr => {
                    if let Err(e) = conn.send_datagram(bytes.clone()) {
                        record_error(&Error::from(e), stats);
                    }
                    EgressDispatch::Sent
                }
                ConnectionTableEntry::Established(_) => {
                    // Peer moved - swap the slot to `Dialing(generation)`...
                    let old = std::mem::replace(
                        slot.get_mut(),
                        ConnectionTableEntry::Dialing(generation),
                    );
                    // ... and close the displaced connection with PEER_MOVED.
                    if let ConnectionTableEntry::Established(old_conn) = old {
                        close_codes::PEER_MOVED.close(&old_conn);
                        stats
                            .connection_evicted_peer_moved
                            .fetch_add(1, Ordering::Relaxed);
                        info!("peer {peer} moved; re-dialing at {addr}");
                    }
                    // Trigger packet is carried into the new dial task and
                    // sent the moment the new connection lands at `addr`.
                    EgressDispatch::SpawnDialTask { generation }
                }
            },
        }
    }

    /// Remove the slot for `peer` if and only if it currently holds an
    /// `Established` with the given `stable_id`. No-op if the slot holds
    /// a different connection (HANDOVER replacement landed) or a
    /// `Dialing` placeholder.
    pub(crate) fn maybe_reap_connection(&self, peer: &Pubkey, stable_id: usize) {
        if let Entry::Occupied(entry) = self.inner.entry(*peer)
            && matches!(entry.get(), ConnectionTableEntry::Established(c) if c.stable_id() == stable_id)
        {
            entry.remove();
        }
    }

    /// Scan the table for an entry whose peer is no longer admitted by
    /// `allow` and evict it. Used at insert-cap time to make room for a
    /// freshly-admitted peer (typically across an epoch boundary where the
    /// staked-set churn temporarily pushes the union past `MAX_PEERS`).
    /// Closes any displaced `Established` with `NOT_ADMITTED` and bumps
    /// `connection_evicted_admission`. Returns `true` on a hit. Stops at
    /// the first eviction - one slot is enough for the caller's retry.
    pub(crate) fn evict_one_unadmitted<F: Fn(&Pubkey) -> bool>(
        &self,
        allow: F,
        stats: &QuicDatagramStats,
    ) -> bool {
        // Collect a candidate key first (release the iterator's shard
        // guards before mutating). `iter` holds shard read-locks; any
        // call that wants a write lock on the same shard would deadlock
        // if we mutated inside the loop.
        let victim = self
            .inner
            .iter()
            .find(|entry| !allow(entry.key()))
            .map(|entry| *entry.key());
        let Some(peer) = victim else {
            return false;
        };
        if let Some((_, entry)) = self.inner.remove(&peer) {
            if let ConnectionTableEntry::Established(conn) = entry {
                close_codes::NOT_ADMITTED.close(&conn);
            }
            stats
                .connection_evicted_admission
                .fetch_add(1, Ordering::Relaxed);
            true
        } else {
            // Slot vanished between the scan and the remove. Caller will
            // re-attempt insert; if the cap is still hit they'll retry
            // eviction.
            false
        }
    }

    /// Remove the slot for `peer` iff it still holds a `Dialing`
    /// placeholder tagged with the same generation. Called by the
    /// dialer task on failure so the next egress can spawn a fresh
    /// dial. Race-safe: silently does nothing if some other path
    /// replaced the slot with an `Established`, or if a later
    /// generation's dialer already swapped its own `Dialing` in (we
    /// must not clobber a fresher placeholder).
    pub(crate) fn clear_dialing_placeholder(&self, peer: &Pubkey, generation: IdGeneration) {
        if let Entry::Occupied(slot) = self.inner.entry(*peer)
            && matches!(slot.get(), ConnectionTableEntry::Dialing(g) if *g == generation)
        {
            slot.remove();
        }
    }

    /// Bump the identity generation and wipe the table. Every
    /// `Established` is closed with `IDENTITY_ROTATED` on the way out;
    /// `Dialing` placeholders are dropped. Any in-flight dial / accept
    /// whose handshake completes after the gen bump will hit `Stale`
    /// in [`Self::insert_connection`] and bail without installing a
    /// wrong-identity connection. Returns the count of entries
    /// dropped (caller bumps the `connection_evicted_identity_rotated`
    /// stat).
    pub(crate) fn clear_for_id_change(&self) -> u64 {
        // Bump generation FIRST so any concurrent dial that observes
        // the new generation via `current_generation()` after this
        // point bails on insert.
        // Overkill on SeqCst since this is super rare
        self.generation.fetch_add(1, Ordering::SeqCst);
        let mut evicted: u64 = 0;
        self.inner.retain(|_, entry| {
            if let ConnectionTableEntry::Established(conn) = entry {
                close_codes::IDENTITY_ROTATED.close(conn);
            }
            evicted = evicted.saturating_add(1);
            false
        });
        evicted
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::collections::HashSet};

    /// Build a `ConnectionTable` seeded with `n_admitted` plus `n_unadmitted`
    /// `Dialing` placeholders. Returns (table, admitted_set, unadmitted_set)
    /// so the test can pass the admitted set as the "allow" predicate.
    ///
    /// Using `Dialing` placeholders (not `Established`) lets us exercise the
    /// scan / remove / stat-bump path at scale without standing up real QUIC
    /// connections. The `Established` close path in `evict_one_unadmitted` is
    /// a single `close_codes::NOT_ADMITTED.close(&conn)` call; not exercised
    /// here, but it's the same one-liner used in every other eviction site.
    fn seed_table(
        n_admitted: usize,
        n_unadmitted: usize,
    ) -> (ConnectionTable, HashSet<Pubkey>, HashSet<Pubkey>) {
        let table = ConnectionTable::new();
        let mut admitted = HashSet::with_capacity(n_admitted);
        let mut unadmitted = HashSet::with_capacity(n_unadmitted);
        for _ in 0..n_admitted {
            let pk = Pubkey::new_unique();
            table.inner.insert(pk, ConnectionTableEntry::Dialing(0));
            admitted.insert(pk);
        }
        for _ in 0..n_unadmitted {
            let pk = Pubkey::new_unique();
            table.inner.insert(pk, ConnectionTableEntry::Dialing(0));
            unadmitted.insert(pk);
        }
        (table, admitted, unadmitted)
    }

    #[test]
    fn evict_one_unadmitted_returns_false_on_empty() {
        let table = ConnectionTable::new();
        let stats = QuicDatagramStats::default();
        assert!(!table.evict_one_unadmitted(|_| true, &stats));
        assert_eq!(
            stats.connection_evicted_admission.load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn evict_one_unadmitted_returns_false_when_all_admitted() {
        let (table, admitted, _) = seed_table(50, 0);
        let stats = QuicDatagramStats::default();
        assert!(!table.evict_one_unadmitted(|pk| admitted.contains(pk), &stats));
        assert_eq!(table.inner.len(), 50);
        assert_eq!(
            stats.connection_evicted_admission.load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn evict_one_unadmitted_drains_only_unadmitted_at_scale() {
        // 1000 admitted + 1000 unadmitted. Loop evict until it returns
        // false; verify every unadmitted entry is gone, every admitted
        // entry remains, and the stat counter matches the eviction count.
        const N: usize = 1000;
        let (table, admitted, unadmitted) = seed_table(N, N);
        assert_eq!(table.inner.len(), 2 * N);

        let stats = QuicDatagramStats::default();
        let mut evicted = 0usize;
        while table.evict_one_unadmitted(|pk| admitted.contains(pk), &stats) {
            evicted += 1;
            assert!(
                evicted <= N,
                "evict_one_unadmitted returned true past the unadmitted set; must not touch \
                 admitted entries"
            );
        }

        assert_eq!(evicted, N, "must evict exactly the {N} unadmitted entries");
        assert_eq!(
            table.inner.len(),
            N,
            "exactly the {N} admitted entries must remain"
        );
        for pk in &admitted {
            assert!(
                table.inner.contains_key(pk),
                "admitted peer {pk} was evicted",
            );
        }
        for pk in &unadmitted {
            assert!(
                !table.inner.contains_key(pk),
                "unadmitted peer {pk} survived eviction",
            );
        }
        assert_eq!(
            stats.connection_evicted_admission.load(Ordering::Relaxed),
            N as u64,
            "stat counter must match eviction count"
        );
    }
}
