//! This module defines [`ConnectionWorkersScheduler`] which creates and
//! orchestrates `ConnectionWorker` instances.
use {
    super::{leader_updater::LeaderUpdater, SendTransactionStatsPerAddr},
    crate::{
        connection_worker::ConnectionWorker,
        quic_networking::{
            create_client_config, create_client_endpoint, QuicClientCertificate, QuicError,
        },
        transaction_batch::TransactionBatch,
        workers_cache::{WorkerInfo, WorkersCache, WorkersCacheError},
    },
    log::*,
    quinn::Endpoint,
    solana_sdk::signature::Keypair,
    std::{net::SocketAddr, sync::Arc},
    thiserror::Error,
    tokio::sync::mpsc,
    tokio_util::sync::CancellationToken,
};

/// [`ConnectionWorkersScheduler`] is responsible for managing and scheduling
/// connection workers that handle transactions over the network.
pub struct ConnectionWorkersScheduler;

#[derive(Debug, Error, PartialEq)]
pub enum ConnectionWorkersSchedulerError {
    #[error(transparent)]
    QuicError(#[from] QuicError),
    #[error(transparent)]
    WorkersCacheError(#[from] WorkersCacheError),
    #[error("Leader receiver unexpectedly dropped.")]
    LeaderReceiverDropped,
}

pub struct ConnectionWorkersSchedulerConfig {
    pub bind: SocketAddr,
    pub stake_identity: Option<Keypair>,
    pub num_connections: usize,
    pub skip_check_transaction_age: bool,
    /// Size of the channel to transmit transaction batches to the target workers.
    pub worker_channel_size: usize, // = 2;
}

impl ConnectionWorkersScheduler {
    /// Runs the main loop that handles worker scheduling and management for
    /// connections. Returns the error quic statistics per connection address or
    /// an error if something goes wrong. Importantly, if some transactions were
    /// not delivered due to network problems, they will not be retried when the
    /// problem is resolved.
    pub async fn run(
        ConnectionWorkersSchedulerConfig {
            bind,
            stake_identity: validator_identity,
            num_connections,
            skip_check_transaction_age,
            worker_channel_size,
        }: ConnectionWorkersSchedulerConfig,
        mut leader_updater: Box<dyn LeaderUpdater>,
        mut transaction_receiver: mpsc::Receiver<TransactionBatch>,
        cancel: CancellationToken,
    ) -> Result<SendTransactionStatsPerAddr, ConnectionWorkersSchedulerError> {
        let endpoint = Self::setup_endpoint(bind, validator_identity)?;
        debug!("Client endpoint bind address: {:?}", endpoint.local_addr());
        let mut workers = WorkersCache::new(num_connections, cancel.clone());

        loop {
            let transaction_batch = tokio::select! {
                recv_res = transaction_receiver.recv() => match recv_res {
                    Some(txs) => txs,
                    None => {
                        debug!("End of `transaction_receiver`: shutting down.");
                        break;
                    }
                },
                () = cancel.cancelled() => {
                    debug!("Cancelled: Shutting down");
                    break;
                }
            };
            let updated_leaders = leader_updater.next_num_lookahead_slots_leaders();
            let new_leader = &updated_leaders[0];
            let future_leaders = &updated_leaders[1..];
            if !workers.contains(new_leader) {
                debug!("No existing workers for {new_leader:?}, starting a new one.");
                let worker = Self::spawn_worker(
                    &endpoint,
                    new_leader,
                    worker_channel_size,
                    skip_check_transaction_age,
                );
                workers.push(*new_leader, worker).await;
            }

            tokio::select! {
                send_res = workers.send_transactions_to_address(new_leader, transaction_batch) => match send_res {
                    Ok(()) => (),
                    Err(WorkersCacheError::ShutdownError) => {
                        debug!("Connection to {new_leader} was closed, worker cache shutdown");
                    }
                    Err(err) => {
                        warn!("Connection to {new_leader} was closed, worker error: {err}");
                       // If we has failed to send batch, it will be dropped.
                    }
                },
                () = cancel.cancelled() => {
                    debug!("Cancelled: Shutting down");
                    break;
                }
            };

            // Regardless of who is leader, add future leaders to the cache to
            // hide the latency of opening the connection.
            for peer in future_leaders {
                if !workers.contains(peer) {
                    let worker = Self::spawn_worker(
                        &endpoint,
                        peer,
                        worker_channel_size,
                        skip_check_transaction_age,
                    );
                    workers.push(*peer, worker).await;
                }
            }
        }

        workers.shutdown().await;

        endpoint.close(0u32.into(), b"Closing connection");
        leader_updater.stop().await;
        Ok(workers.transaction_stats().clone())
    }

    /// Sets up the QUIC endpoint for the scheduler to handle connections.
    fn setup_endpoint(
        bind: SocketAddr,
        validator_identity: Option<Keypair>,
    ) -> Result<Endpoint, ConnectionWorkersSchedulerError> {
        let client_certificate = if let Some(validator_identity) = validator_identity {
            Arc::new(QuicClientCertificate::new(&validator_identity))
        } else {
            Arc::new(QuicClientCertificate::new(&Keypair::new()))
        };
        let client_config = create_client_config(client_certificate);
        let endpoint = create_client_endpoint(bind, client_config)?;
        Ok(endpoint)
    }

    /// Spawns a worker to handle communication with a given peer.
    fn spawn_worker(
        endpoint: &Endpoint,
        peer: &SocketAddr,
        worker_channel_size: usize,
        skip_check_transaction_age: bool,
    ) -> WorkerInfo {
        let (txs_sender, txs_receiver) = mpsc::channel(worker_channel_size);
        let endpoint = endpoint.clone();
        let peer = *peer;

        let (mut worker, cancel) =
            ConnectionWorker::new(endpoint, peer, txs_receiver, skip_check_transaction_age);
        let handle = tokio::spawn(async move {
            worker.run().await;
            worker.transaction_stats().clone()
        });

        WorkerInfo::new(txs_sender, handle, cancel)
    }
}
