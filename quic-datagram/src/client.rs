//! Client-side connection lifecycle.

use {
    crate::{
        Banlist,
        allowlist::Allowlist,
        close_codes,
        connection_table::{ConnectionTable, IdGeneration, InsertOutcome},
        endpoint::Datagram,
        error::Error,
        read_loop::read_datagram_loop,
        stats::{QuicDatagramStats, add, record_error},
    },
    bytes::Bytes,
    crossbeam_channel::Sender,
    log::error,
    quinn::Endpoint,
    solana_pubkey::Pubkey,
    solana_tls_utils::{get_remote_pubkey, socket_addr_to_quic_server_name},
    std::{net::SocketAddr, sync::Arc},
};

/// A client-side connection's full lifecycle: dial, validate identity,
/// install in the table, run the read loop, reap on exit. Built by the
/// control loop and dispatched via [`Self::spawn`].
pub(crate) struct ClientConnection<A: Allowlist> {
    pub(crate) endpoint: Endpoint,
    pub(crate) peer: Pubkey,
    pub(crate) addr: SocketAddr,
    pub(crate) id_generation: IdGeneration,
    /// The egress that triggered this dial. Carried into the dial task
    /// and sent the moment `insert_connection` succeeds, before the
    /// read loop starts. Cold-start traffic (standstill) needs this to
    /// reach the peer at all.
    pub(crate) trigger: Bytes,
    pub(crate) ingress: Sender<Datagram>,
    pub(crate) allowlist: Arc<A>,
    pub(crate) banlist: Arc<Banlist<Pubkey>>,
    pub(crate) table: Arc<ConnectionTable>,
    pub(crate) stats: Arc<QuicDatagramStats>,
}

impl<A: Allowlist> ClientConnection<A> {
    /// Spawn a tokio task that drives this connection to completion.
    /// Returns immediately; the task logs and records any error before
    /// exiting.
    pub(crate) fn spawn(self) {
        let peer = self.peer;
        let addr = self.addr;
        let stats = self.stats.clone();
        tokio::spawn(async move {
            if let Err(e) = self.run().await {
                error!("Connection attempt to ({peer}, {addr}) failed: {e:?}");
                record_error(&e, &stats);
            }
        });
    }

    async fn run(self) -> Result<(), Error> {
        // Clone the needed data for cleanup
        let table_ref_for_cleanup = self.table.clone();
        let peer = self.peer;
        let id_generation = self.id_generation;

        let result: Result<(), Error> = async move {
            let server_name = socket_addr_to_quic_server_name(self.addr);
            let connection = self.endpoint.connect(self.addr, &server_name)?.await?;

            // Server identity must match the pubkey the caller targeted.
            let attested =
                get_remote_pubkey(&connection).ok_or(Error::InvalidIdentity(self.addr))?;
            if attested != self.peer {
                close_codes::INVALID_IDENTITY.close(&connection);
                return Err(Error::InvalidIdentity(self.addr));
            }

            // On cap-induced Rejected, the table will try to evict a
            // now-unadmitted peer before giving up - epoch-boundary churn
            // can momentarily push the staked-set union past `MAX_PEERS`.
            match self.table.insert_connection_or_evict(
                self.peer,
                connection.clone(),
                id_generation,
                |pk| self.allowlist.allow(pk),
                &self.stats,
            ) {
                InsertOutcome::Rejected => {
                    close_codes::TABLE_FULL.close(&connection);
                    return Err(Error::TableFull);
                }
                InsertOutcome::Stale => {
                    close_codes::IDENTITY_ROTATED.close(&connection);
                    return Err(Error::IdentityRotated(self.peer));
                }
                InsertOutcome::Inserted | InsertOutcome::Replaced => {
                    self.stats.record_connection_count(self.table.len());
                }
            }

            // Send the trigger packet that started this dial. Quinn-level
            // failures are recorded but not propagated - the connection
            // itself is healthy and the read loop should still run.
            match connection.send_datagram(self.trigger) {
                Ok(()) => add(&self.stats.datagrams_sent),
                Err(e) => record_error(&Error::from(e), &self.stats),
            }

            // Drive the read loop inline.
            let stable_id = connection.stable_id();
            read_datagram_loop(
                connection,
                self.peer,
                self.addr,
                self.ingress,
                self.banlist,
                self.stats,
            )
            .await;
            // Reap our slot only if it still points at *this* connection
            // (HANDOVER could have replaced it).
            self.table.maybe_reap_connection(&self.peer, stable_id);
            Ok(())
        }
        .await;

        if result.is_err() {
            table_ref_for_cleanup.clear_dialing_placeholder(&peer, id_generation);
        }
        result
    }
}
