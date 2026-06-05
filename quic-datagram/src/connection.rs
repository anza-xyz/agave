//! Per-connection lifecycles: outbound (client) and inbound (server).
//!
//! These tasks only do I/O (dial / handshake) and report the result back to
//! the control loop via [`ConnEvent`]. All policy (lex direction, banlist,
//! allowlist, capacity, handover) and every table mutation happens on the
//! loop. See [`crate::connection_table`] and [`crate::endpoint`].

use {
    crate::{
        close_codes,
        connection_table::IdGeneration,
        error::Error,
        stats::{QuicDatagramStats, record_error},
    },
    bytes::Bytes,
    log::error,
    quinn::{Connection, Endpoint, Incoming},
    solana_pubkey::Pubkey,
    solana_tls_utils::{get_remote_pubkey, socket_addr_to_quic_server_name},
    std::{
        net::SocketAddr,
        sync::{Arc, atomic::Ordering},
    },
    tokio::sync::mpsc,
};

/// Result of an outbound dial, carried in [`ConnEvent::DialDone`].
pub(crate) enum DialOutcome {
    /// Dial + identity validation succeeded. `trigger` is the egress that
    /// started the dial; the loop sends it the moment the slot goes live.
    Established { conn: Connection, trigger: Bytes },
    /// Dial failed (or the attested identity mismatched). The loop clears the
    /// `Dialing` placeholder so a future egress can re-dial.
    Failed,
}

/// A connection-lifecycle result reported by a spawned task to the control
/// loop. Each variant carries the [`IdGeneration`] sampled when the task was
/// spawned; the loop drops events whose generation no longer matches.
pub(crate) enum ConnEvent {
    /// An inbound TLS handshake completed and yielded `peer`.
    Inbound {
        peer: Pubkey,
        conn: Connection,
        generation: IdGeneration,
    },
    /// An outbound dial to `peer` has resolved
    DialDone {
        peer: Pubkey,
        generation: IdGeneration,
        outcome: DialOutcome,
    },
    /// A read loop of existing connection exited.
    Closed {
        peer: Pubkey,
        generation: IdGeneration,
        stable_id: usize,
    },
}

impl ConnEvent {
    /// The generation this event was tagged with at spawn time.
    pub(crate) fn generation(&self) -> IdGeneration {
        match self {
            ConnEvent::Inbound { generation, .. }
            | ConnEvent::DialDone { generation, .. }
            | ConnEvent::Closed { generation, .. } => *generation,
        }
    }

    /// Close any live connection carried by a stale-generation event so a
    /// connection authenticated under a now-rotated identity is not leaked.
    pub(crate) fn close_stale(self, stats: &QuicDatagramStats) {
        let conn = match self {
            ConnEvent::Inbound { conn, .. } => conn,
            ConnEvent::DialDone {
                outcome: DialOutcome::Established { conn, .. },
                ..
            } => conn,
            ConnEvent::DialDone {
                outcome: DialOutcome::Failed,
                ..
            }
            | ConnEvent::Closed { .. } => return,
        };
        close_codes::IDENTITY_ROTATED.close(&conn);
        stats
            .connection_evicted_identity_rotated
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// An outbound dial: connect, validate the server identity, and report the
/// outcome to the control loop. Built by the loop and dispatched via
/// [`Self::spawn`].
pub(crate) struct ClientConnection {
    pub(crate) endpoint: Endpoint,
    pub(crate) peer: Pubkey,
    pub(crate) addr: SocketAddr,
    pub(crate) generation: IdGeneration,
    /// The egress that triggered this dial. Cold-start traffic (standstill)
    /// needs this to reach the peer reliably.
    pub(crate) trigger: Bytes,
    pub(crate) events: mpsc::Sender<ConnEvent>,
    pub(crate) stats: Arc<QuicDatagramStats>,
}

impl ClientConnection {
    /// Spawn a tokio task that drives this dial to completion.
    pub(crate) fn spawn(self) {
        tokio::spawn(self.run());
    }

    async fn run(self) {
        let outcome = match self.dial().await {
            Ok(conn) => DialOutcome::Established {
                conn,
                trigger: self.trigger,
            },
            Err(e) => {
                error!(
                    "Connection attempt to ({}, {}) failed: {e:?}",
                    self.peer, self.addr
                );
                record_error(&e, &self.stats);
                DialOutcome::Failed
            }
        };
        // Blocking send to make sure we clear the `Dialing` placeholder. If the
        // send fails there is nothing left to do.
        let _ = self
            .events
            .send(ConnEvent::DialDone {
                peer: self.peer,
                generation: self.generation,
                outcome,
            })
            .await;
    }

    async fn dial(&self) -> Result<Connection, Error> {
        let server_name = socket_addr_to_quic_server_name(self.addr);
        let connection = self.endpoint.connect(self.addr, &server_name)?.await?;
        // Server identity must match the pubkey the caller targeted.
        let attested = get_remote_pubkey(&connection).ok_or(Error::InvalidIdentity(self.addr))?;
        if attested != self.peer {
            close_codes::INVALID_IDENTITY.close(&connection);
            return Err(Error::InvalidIdentity(self.addr));
        }
        Ok(connection)
    }
}

/// An inbound accept: run the handshake and hand the connection (plus its
/// attested pubkey) to the control loop for admission.
pub(crate) struct ServerConnection {
    pub(crate) incoming: Incoming,
    pub(crate) generation: IdGeneration,
    pub(crate) events: mpsc::Sender<ConnEvent>,
    pub(crate) stats: Arc<QuicDatagramStats>,
}

impl ServerConnection {
    /// Spawn a tokio task that drives this handshake to completion.
    pub(crate) fn spawn(self) {
        tokio::spawn(self.run());
    }

    /// post-RETRY accept, extract the peer identity, hand off to the loop.
    /// All admission policy lives on the loop.
    async fn run(self) {
        let ServerConnection {
            incoming,
            generation,
            events,
            stats,
        } = self;
        let remote_addr = incoming.remote_address();
        let conn = match Self::handshake(incoming).await {
            Ok(conn) => conn,
            Err(e) => {
                record_error(&e, &stats);
                return;
            }
        };
        let Some(peer) = get_remote_pubkey(&conn) else {
            close_codes::INVALID_IDENTITY.close(&conn);
            record_error(&Error::InvalidIdentity(remote_addr), &stats);
            return;
        };
        // Hand the connection to the loop for admission. The loop is the sole
        // consumer of this channel and never sends into it, so awaiting here
        // cannot deadlock - a momentarily-full channel just parks this task
        // until the loop drains. If the send fails the loop is gone (shutdown)
        // and dropping `conn` closes it.
        let _ = events
            .send(ConnEvent::Inbound {
                peer,
                conn,
                generation,
            })
            .await;
    }

    async fn handshake(incoming: Incoming) -> Result<Connection, Error> {
        Ok(incoming.accept()?.await?)
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            allowlist::{AllowAll, StakedNodesAllowlist},
            endpoint::Datagram,
            testutils::{
                drain_matching, keypair_below, make_runtime, send_until_received, spawn_node,
            },
        },
        bytes::Bytes,
        solana_keypair::{Keypair, Signer},
        std::{
            collections::{HashMap, HashSet},
            sync::Arc,
            time::Duration,
        },
    };

    #[test]
    fn staked_peer_is_admitted_unstaked_is_rejected() {
        let rt = make_runtime();

        // Lex tiebreaker requires the dialing client's pubkey < server's. Pick
        // the server keypair first, then derive client A's keypair below it.
        let server_kp = Keypair::new();
        let server_pk = server_kp.pubkey();
        let a_kp = keypair_below(&server_pk);
        let a_pk = a_kp.pubkey();
        let admit_map: HashMap<_, _> = std::iter::once((a_pk, 100u64)).collect();
        let server = spawn_node(
            &rt,
            Arc::new(StakedNodesAllowlist::new(admit_map, HashSet::new())),
            server_kp,
        );

        // Client A - admitted (pubkey < server's, so handshake passes lex check).
        let client_a = spawn_node(&rt, Arc::new(AllowAll), a_kp);
        // Client B - not admitted. Pick below server so the rejection is by the
        // allowlist check, not the lex check.
        let client_b = spawn_node(&rt, Arc::new(AllowAll), keypair_below(&server_pk));

        let payload_a = Bytes::from_static(b"hello-from-A");
        send_until_received(
            &rt,
            &client_a.endpoint,
            server.pubkey(),
            server.addr,
            payload_a.clone(),
            &server.ingress_rx,
            Duration::from_secs(5),
            |d| (d.peer_pubkey == a_pk && d.message == payload_a).then_some(()),
            "server never received payload from admitted peer A",
        );
        // send_until_received may have left retry duplicates of payload_a
        // in the channel; drain them before asserting B's rejection.
        drain_matching(&server.ingress_rx, Duration::from_millis(200), |d| {
            d.message == payload_a
        });

        let payload_b = Bytes::from_static(b"hello-from-B");
        rt.block_on(async {
            client_b
                .endpoint
                .egress
                .send(Datagram {
                    peer_pubkey: server.pubkey(),
                    peer_address: server.addr,
                    message: payload_b.clone(),
                })
                .await
                .expect("egress send B");
        });

        // Server must close the handshake before any datagram from B is queued.
        let bad = server.ingress_rx.recv_timeout(Duration::from_millis(800));
        assert!(
            bad.is_err(),
            "unstaked peer B's datagram should not reach server ingress, got {bad:?}"
        );
    }
}
