//! Outbound (client) direction: we initiate, send-only.
use {
    crate::{
        METRICS_INTERVAL, PeerListReceiver, close_codes,
        endpoint::KeyUpdateListener,
        error::Error,
        stats::{self, ClientStats, record_client_error},
        transport::new_client_config,
    },
    bytes::Bytes,
    log::{debug, error, info, warn},
    quinn::{Connection, Endpoint, SendDatagramError},
    solana_keypair::{Keypair, Signer},
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    solana_tls_utils::{get_remote_pubkey, socket_addr_to_quic_server_name},
    std::{
        collections::HashMap,
        net::SocketAddr,
        sync::{Arc, atomic::Ordering},
        time::Duration,
    },
    tokio::{
        sync::mpsc,
        task::JoinSet,
        time::{MissedTickBehavior, interval, timeout},
    },
    tokio_util::sync::CancellationToken,
};

/// How often the outbound loop reconciles its connection table against the
/// peer_list. Doubles as the retry interval for failed connects.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(1);

/// Upper bound on a single handshake attempt, enforced inside the connect task.
/// Accommodates any plausible RTT with margin.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// State of a peer's entry in the outbound table.
#[derive(Debug)]
pub(crate) enum PeerState {
    /// Handshake in progress.
    Connecting,
    /// Live send-only connection.
    Established {
        connection: Connection,
        // This is cached at install so reconcile can
        // detect an address change without locking quinn state.
        target_address: SocketAddr,
    },
}

/// `Ok` carries the peer and its newly established connection; `Err` carries
/// the peer we tried (and failed) to connect to.
type HandshakeOutcome = Result<(Pubkey, Connection), Pubkey>;

/// Enforce the send-only contract of an outbound connection.
async fn reject_reverse_datagrams(peer: Pubkey, connection: Connection) {
    if connection.read_datagram().await.is_ok() {
        warn!("Peer {peer} sent a datagram on a send-only connection; closing.");
        close_codes::FLOODING.close(&connection);
    }
}

/// Open and authenticate a new connection to `peer` at `addr`.
async fn connect(
    endpoint: Endpoint,
    peer: Pubkey,
    addr: SocketAddr,
    stats: Arc<ClientStats>,
) -> HandshakeOutcome {
    let attempt = async {
        let server_name = socket_addr_to_quic_server_name(addr);
        let connection = endpoint.connect(addr, &server_name)?.await?;
        let attested = get_remote_pubkey(&connection).ok_or(Error::InvalidIdentity(addr))?;
        if attested != peer {
            close_codes::INVALID_IDENTITY.close(&connection);
            return Err(Error::InvalidIdentity(addr));
        }
        Ok(connection)
    };
    match timeout(HANDSHAKE_TIMEOUT, attempt).await {
        Ok(Ok(connection)) => Ok((peer, connection)),
        Ok(Err(e)) => {
            warn!("Connection attempt to ({peer}, {addr}) failed: {e:?}");
            record_client_error(&e, &stats);
            Err(peer)
        }
        Err(_elapsed) => {
            warn!("Connection attempt to ({peer}, {addr}) timed out after {HANDSHAKE_TIMEOUT:?}");
            stats.connect_failed.fetch_add(1, Ordering::Relaxed);
            Err(peer)
        }
    }
}

/// Outbound control loop for the egress direction (we-connect, send-only).
/// Strives to ensure open connections to everyone in the peer_list.
pub(crate) struct OutboundLoop {
    endpoint: Endpoint,
    local_pubkey: Pubkey,
    /// Channel for outbound messages to be broadcast
    egress_receiver: mpsc::Receiver<Bytes>,
    key_updates: KeyUpdateListener,
    peer_list_receiver: PeerListReceiver,
    /// Per-peer send-only connection state.
    /// Size is limited to the peer_list size by reconcile task.
    peer_state: HashMap<Pubkey, PeerState, PubkeyHasherBuilder>,
    in_flight_handshakes: JoinSet<HandshakeOutcome>,
    reverse_datagram_readers: JoinSet<()>,
    cancel: CancellationToken,
    stats: Arc<ClientStats>,
    max_datagrams_per_second_per_peer: usize,
}

impl OutboundLoop {
    pub(crate) fn new(
        endpoint: Endpoint,
        local_pubkey: Pubkey,
        egress_receiver: mpsc::Receiver<Bytes>,
        key_updates: KeyUpdateListener,
        peer_list_receiver: PeerListReceiver,
        cancel: CancellationToken,
        max_datagrams_per_second_per_peer: usize,
    ) -> Self {
        Self {
            endpoint,
            local_pubkey,
            egress_receiver,
            key_updates,
            peer_list_receiver,
            peer_state: HashMap::with_hasher(PubkeyHasherBuilder::default()),
            in_flight_handshakes: JoinSet::new(),
            reverse_datagram_readers: JoinSet::new(),
            cancel,
            stats: Arc::default(),
            max_datagrams_per_second_per_peer,
        }
    }

    pub(crate) async fn run(mut self) {
        let mut metrics_timer = interval(METRICS_INTERVAL);
        metrics_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut reconcile_timer = interval(RECONCILE_INTERVAL);
        reconcile_timer.set_missed_tick_behavior(MissedTickBehavior::Delay);

        // A receiver clone is needed to drive change notifications, as they call for &mut.
        let mut peer_list_receiver = self.peer_list_receiver.clone();

        info!("Votor QUIC transport client ready.");
        loop {
            tokio::select! {
                changed = self.key_updates.receiver.changed() => {
                    if changed.is_err() {
                        // Unreachable: endpoint holds the identity sender for this loop's lifetime.
                        error!("OutboundLoop: identity channel closed while running, exiting.");
                        debug_assert!(false, "identity channel closed while running");
                        break;
                    }
                    let new_keypair = self.key_updates.receiver.borrow_and_update().insecure_clone();
                    self.apply_identity_change(new_keypair);
                    self.reconcile();
                }
                // The peer set changed: reconcile the connection table right away.
                changed = peer_list_receiver.changed() => {
                    if changed.is_err() {
                        // Unreachable: PeerListService only drops sender when we drop the receiver.
                        error!("OutboundLoop: peer_list channel closed while running, exiting.");
                        debug_assert!(false, "peer_list channel closed while running");
                        break;
                    }
                    self.reconcile();
                }
                Some(joined) = self.in_flight_handshakes.join_next() => {
                    // we never abort these (without also wiping in_flight_handshakes)
                    // and panics are fatal in the validator.
                    let outcome = joined.expect("outbound handshake task panicked or was aborted");
                    self.handle_handshake_outcome(outcome);
                }
                Some(joined) = self.reverse_datagram_readers.join_next() => {
                    joined.expect("outbound reverse-datagram reader task panicked or was aborted");
                }
                maybe_message = self.egress_receiver.recv() => {
                    let Some(message) = maybe_message else {
                        debug_assert!(
                            self.cancel.is_cancelled(),
                            "egress channel closed before cancel signal is given"
                        );
                        error!("OutboundLoop: egress channel closed, exiting.");
                        break;
                    };
                    self.perform_broadcast(message);
                }
                // Periodic reconcile: connect to missing peers, drop departed peers.
                _ = reconcile_timer.tick() => self.reconcile(),
                _ = metrics_timer.tick() => {
                    debug!("OutboundLoop: running bookkeeping tasks");
                    self.stats.report(self.total_peers());
                }
                _ = self.cancel.cancelled() => break,
            }
        }
    }

    /// Counts the number of established connections
    fn total_peers(&self) -> u64 {
        // scan is not ideal but is simple and correct, and we have < 2000 entries
        self.peer_state
            .values()
            .filter(|entry| matches!(entry, PeerState::Established { .. }))
            .count() as u64
    }

    /// Rebuild the client TLS config against the new identity, swap it into the
    /// quinn endpoint, close every existing connection (since they use old ID),
    /// and adopt the new pubkey. The next reconcile will reconnect to everyone.
    fn apply_identity_change(&mut self, new_keypair: Keypair) {
        let client_config = new_client_config(&new_keypair, self.max_datagrams_per_second_per_peer);
        self.local_pubkey = new_keypair.pubkey();
        self.endpoint.set_default_client_config(client_config);
        // Dropping the JoinSet aborts every in-flight handshake. We do not
        // want them as they began under the old identity.
        self.in_flight_handshakes = JoinSet::new();
        self.reverse_datagram_readers = JoinSet::new();
        let closed = self
            .peer_state
            .drain()
            .filter(|(_peer, entry)| {
                if let PeerState::Established { connection, .. } = entry {
                    close_codes::IDENTITY_CHANGED.close(connection);
                    true
                } else {
                    false
                }
            })
            .count() as u64;
        self.stats
            .connection_closed_identity_changed
            .fetch_add(closed, Ordering::Relaxed);
        // Never blocks, and a dropped ack only matters at shutdown, when the
        // updater is gone anyway.
        let _ = self.key_updates.ack.try_send(());
        info!(
            "outbound identity changed to {} ({} connection(s) closed)",
            self.local_pubkey, closed
        );
    }

    /// Reconcile the connection table against the current peer_list.
    fn reconcile(&mut self) {
        debug!("OutboundLoop: running reconcile");
        let peer_list = self.peer_list_receiver.borrow().clone();
        // With pushing off every peer we hold counts as departed.
        let push_enabled = peer_list.push_enabled;

        // 1. Close connections for departed peers.
        let mut closed_not_in_peer_list = 0u64;
        self.peer_state.retain(|peer, state| {
            if push_enabled && peer_list.peers.contains_key(peer) {
                return true;
            }
            match state {
                PeerState::Established { connection, .. } => {
                    info!("OutboundLoop: closing connection to {peer}: no longer desired.");
                    close_codes::NOT_ADMITTED.close(connection);
                    closed_not_in_peer_list = closed_not_in_peer_list.saturating_add(1);
                    false
                }
                // A Connecting entry resolves within HANDSHAKE_TIMEOUT through
                // `handle_handshake_outcome` into an Established (at which point we can wipe it)
                // or just gets removed there.
                PeerState::Connecting => true,
            }
        });
        self.stats
            .connection_closed_not_in_peer_list
            .fetch_add(closed_not_in_peer_list, Ordering::Relaxed);

        if !push_enabled {
            return;
        }

        // 2. Ensure a connection for every addressable peer.
        for (peer, addr) in peer_list.peers.iter() {
            if *peer == self.local_pubkey {
                continue;
            }
            // No gossip address yet (or anymore)
            let Some(addr) = addr else {
                self.stats
                    .connect_failed_no_address
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            };
            let needs_connection = match self.peer_state.get(peer) {
                // A handshake is already in flight; do not spawn a second task for this peer.
                Some(PeerState::Connecting) => false,
                Some(PeerState::Established {
                    connection,
                    target_address: cur,
                }) => {
                    // desired peer address has changed
                    if cur != addr {
                        info!(
                            "OutboundLoop: closing connection to {peer}: desired address changed \
                             from {cur} to {addr}."
                        );
                        close_codes::PEER_MOVED.close(connection);
                        self.stats
                            .connection_closed_peer_moved
                            .fetch_add(1, Ordering::Relaxed);
                        true
                    } else {
                        // Reconnect if the connection has died.
                        connection
                            .close_reason()
                            .inspect(|reason| {
                                self.stats.connection_lost.fetch_add(1, Ordering::Relaxed);
                                info!("OutboundLoop: connection to {peer} was closed: {reason}");
                            })
                            .is_some()
                    }
                }
                None => true,
            };
            if needs_connection {
                info!("OutboundLoop: initiating connection to {peer} ({addr})");
                self.in_flight_handshakes.spawn(connect(
                    self.endpoint.clone(),
                    *peer,
                    *addr,
                    self.stats.clone(),
                ));
                self.peer_state.insert(*peer, PeerState::Connecting);
            }
        }
    }

    /// Broadcast one message to every live connection. Broken connections are
    /// dropped from the table so the next reconcile remakes them.
    fn perform_broadcast(&mut self, message: Bytes) {
        let mut sent = 0u64;
        let mut dead_peers = Vec::new();
        for (peer, state) in self.peer_state.iter() {
            let PeerState::Established { connection, .. } = state else {
                continue;
            };
            match connection.send_datagram(message.clone()) {
                Ok(()) => sent = sent.saturating_add(1),
                Err(SendDatagramError::ConnectionLost(reason)) => {
                    debug!("OutboundLoop: connection to {peer} lost while sending: {reason}.");
                    self.stats.connection_lost.fetch_add(1, Ordering::Relaxed);
                    dead_peers.push(*peer);
                }
                // The peer does not support datagrams or advertises a datagram size limit below
                // message size. Both are likely remote misconfigurations, reconnecting is not
                // likely to help, so we keep these connections to avoid churn.
                Err(e @ (SendDatagramError::UnsupportedByPeer | SendDatagramError::TooLarge)) => {
                    debug!(
                        "OutboundLoop: peer {peer} refused a {} byte datagram: {e}",
                        message.len()
                    );
                    self.stats
                        .datagram_send_failed
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(SendDatagramError::Disabled) => {
                    unreachable!("By construction (we enable datagrams)");
                }
            }
        }
        self.stats.datagrams_sent.fetch_add(sent, Ordering::Relaxed);
        for peer in dead_peers {
            self.peer_state.remove(&peer);
        }
    }

    /// Handle a handshake task that ran to completion.
    fn handle_handshake_outcome(&mut self, outcome: HandshakeOutcome) {
        // Peer state is always `Connecting` here: one handshake per peer, reconcile
        // leaves `Connecting` entries in place, and identity rotation resets the JoinSet.
        match outcome {
            Ok((peer, connection)) => {
                let target_address = connection.remote_address();
                let reverse_datagram_reader = connection.clone();
                let previous = self.peer_state.insert(
                    peer,
                    PeerState::Established {
                        connection,
                        target_address,
                    },
                );
                debug_assert!(
                    matches!(previous, Some(PeerState::Connecting)),
                    "handshake success for {peer} whose state was {previous:?}, not Connecting."
                );
                self.reverse_datagram_readers
                    .spawn(reject_reverse_datagrams(peer, reverse_datagram_reader));
                info!("OutboundLoop: established connection to {peer}.");
                stats::record_connection_count(&self.stats.peak_connections, self.total_peers());
            }
            Err(peer) => {
                let removed = self.peer_state.remove(&peer);
                debug_assert!(
                    matches!(removed, Some(PeerState::Connecting)),
                    "handshake failed for {peer} whose state was {removed:?}, not Connecting."
                );
                // an operator-visible message is printed in handshake task itself
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::PeerList,
        solana_net_utils::sockets::unique_port_range_for_tests,
        std::net::Ipv4Addr,
        tokio::{sync::watch, time::sleep},
    };

    /// Recreates an edge case when client identity changes while a peer is in `Connecting`
    /// state, we must correctly terminate the handshake and leave things in a clean state.
    #[tokio::test]
    async fn identity_change_drops_handshakes() {
        let old_kp = Keypair::new();
        let old_pubkey = old_kp.pubkey();
        let port = unique_port_range_for_tests(1).next().unwrap();
        let mut endpoint = Endpoint::client(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            .expect("bind client endpoint");
        endpoint.set_default_client_config(new_client_config(&old_kp, 50));

        let (_egress_tx, egress_rx) = mpsc::channel(1);
        let (_identity_tx, identity_rx) = watch::channel(Keypair::new());
        let (ack, _acks) = crossbeam_channel::unbounded();
        let (_peer_list_tx, peer_list_rx) = watch::channel(Arc::new(PeerList {
            peers: HashMap::default(),
            push_enabled: true,
        }));

        let mut outbound = OutboundLoop::new(
            endpoint,
            old_pubkey,
            egress_rx,
            KeyUpdateListener {
                receiver: identity_rx,
                ack,
            },
            peer_list_rx,
            CancellationToken::new(),
            50,
        );

        // Add a fake peer whose handshake is not finished at the moment of id change.
        let stuck_peer = Keypair::new().pubkey();
        outbound
            .peer_state
            .insert(stuck_peer, PeerState::Connecting);
        outbound.in_flight_handshakes.spawn(async {
            sleep(Duration::from_secs(10)).await;
            unreachable!("This future should never resolve");
        });
        outbound.reverse_datagram_readers.spawn(async {
            sleep(Duration::from_secs(10)).await;
            unreachable!("This future should never resolve");
        });

        let new_kp = Keypair::new();
        let new_pubkey = new_kp.pubkey();
        outbound.apply_identity_change(new_kp);
        assert!(
            outbound.in_flight_handshakes.is_empty(),
            "identity change must clear in_flight_handshakes"
        );
        assert!(
            outbound.reverse_datagram_readers.is_empty(),
            "identity change must clear reverse_datagram_readers"
        );
        assert!(
            outbound.peer_state.is_empty(),
            "identity change must clear state",
        );
        assert_eq!(
            outbound
                .stats
                .connection_closed_identity_changed
                .load(Ordering::Relaxed),
            0,
            "a peer in `Connecting` state must not count as a closed connection",
        );
        assert_eq!(
            outbound.local_pubkey, new_pubkey,
            "identity change must adopt the new pubkey",
        );
    }

    #[tokio::test]
    async fn reverse_datagram_closes_connection() {
        use crate::transport::new_server_config;

        const MAX_DATAGRAMS_PER_SECOND: usize = 50;
        let mut ports = unique_port_range_for_tests(2);

        let server_keypair = Keypair::new();
        let server = Endpoint::server(
            new_server_config(&server_keypair, MAX_DATAGRAMS_PER_SECOND, 1),
            SocketAddr::from((Ipv4Addr::LOCALHOST, ports.next().unwrap())),
        )
        .expect("bind server endpoint");
        let server_addr = server.local_addr().unwrap();

        let client_keypair = Keypair::new();
        let mut client = Endpoint::client(SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            ports.next().unwrap(),
        )))
        .expect("bind client endpoint");
        client.set_default_client_config(new_client_config(
            &client_keypair,
            MAX_DATAGRAMS_PER_SECOND,
        ));

        let connecting = client
            .connect(server_addr, &socket_addr_to_quic_server_name(server_addr))
            .unwrap();
        let incoming = server.accept().await.unwrap();
        let (client_connection, server_connection) = tokio::join!(connecting, incoming);
        let client_connection = client_connection.unwrap();
        let server_connection = server_connection.unwrap();

        let reader = tokio::spawn(reject_reverse_datagrams(
            server_keypair.pubkey(),
            client_connection,
        ));
        server_connection
            .send_datagram(Bytes::new())
            .expect("client advertised datagram support");
        reader.await.unwrap();
        let reason = timeout(Duration::from_secs(5), server_connection.closed())
            .await
            .expect("client did not close after the reverse datagram");
        let quinn::ConnectionError::ApplicationClosed(close) = reason else {
            panic!("unexpected client close reason: {reason:?}");
        };
        assert_eq!(close.error_code, close_codes::FLOODING.code);
    }
}
