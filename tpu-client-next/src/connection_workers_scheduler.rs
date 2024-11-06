//! This module defines [`ConnectionWorkersScheduler`] which sends transactions
//! to the upcoming leaders.

use {
    super::{leader_updater::LeaderUpdater, SendTransactionStatsPerAddr},
    crate::{
        connection_worker::ConnectionWorker,
        quic_networking::{
            create_client_config, create_client_endpoint, QuicClientCertificate, QuicError,
        },
        transaction_batch::TransactionBatch,
        workers_cache::{WorkerInfo, WorkersCache, WorkersCacheError},
        SendTransactionStats,
    },
    log::*,
    quinn::Endpoint,
    solana_sdk::signature::Keypair,
    std::{net::SocketAddr, sync::Arc},
    thiserror::Error,
    tokio::sync::mpsc,
    tokio_util::sync::CancellationToken,
};

/// The [`ConnectionWorkersScheduler`] sends transactions from the provided
/// receiver channel to upcoming leaders. It obtains information about future
/// leaders from the implementation of the [`LeaderUpdater`] trait.
///
/// Internally, it enables the management and coordination of multiple network
/// connections, schedules and oversees connection workers.
pub struct ConnectionWorkersScheduler;

/// Errors that arise from running [`ConnectionWorkersSchedulerError`].
#[derive(Debug, Error, PartialEq)]
pub enum ConnectionWorkersSchedulerError {
    #[error(transparent)]
    QuicError(#[from] QuicError),
    #[error(transparent)]
    WorkersCacheError(#[from] WorkersCacheError),
    #[error("Leader receiver unexpectedly dropped.")]
    LeaderReceiverDropped,
}

/// This enum defines to how many discovered leaders we will send transactions.
pub enum LeadersFanout {
    /// Send transactions to all the leaders discovered by the `next_leaders`
    /// call.
    All,
    /// Send transactions to the first selected number of leaders.
    Selected(usize),
}

/// Configuration for the [`ConnectionWorkersScheduler`].
///
/// This struct holds the necessary settings to initialize and manage connection
/// workers, including network binding, identity, connection limits, and
/// behavior related to transaction handling.
pub struct ConnectionWorkersSchedulerConfig {
    /// The local address to bind the scheduler to.
    pub bind: SocketAddr,

    /// Optional stake identity keypair used in the endpoint certificate for
    /// identifying the sender.
    pub stake_identity: Option<Keypair>,

    /// The number of connections to be maintained by the scheduler.
    pub num_connections: usize,

    /// Whether to skip checking the transaction blockhash expiration.
    pub skip_check_transaction_age: bool,

    /// The size of the channel used to transmit transaction batches to the
    /// worker tasks.
    pub worker_channel_size: usize,

    /// The maximum number of reconnection attempts allowed in case of
    /// connection failure.
    pub max_reconnect_attempts: usize,

    /// The number of slots to look ahead during the leader estimation
    /// procedure. Determines how far into the future leaders are estimated,
    /// allowing connections to be established with those leaders in advance.
    pub lookahead_slots: u64,

    /// The number of leaders to send transactions to.
    pub leaders_fanout: LeadersFanout,
}

impl ConnectionWorkersScheduler {
    /// Starts the scheduler, which manages the distribution of transactions to
    /// the network's upcoming leaders.
    ///
    /// Runs the main loop that handles worker scheduling and management for
    /// connections. Returns the error quic statistics per connection address or
    /// an error.
    ///
    /// Importantly, if some transactions were not delivered due to network
    /// problems, they will not be retried when the problem is resolved.
    pub async fn run(
        ConnectionWorkersSchedulerConfig {
            bind,
            stake_identity: validator_identity,
            num_connections,
            skip_check_transaction_age,
            worker_channel_size,
            max_reconnect_attempts,
            lookahead_slots,
            leaders_fanout,
        }: ConnectionWorkersSchedulerConfig,
        mut leader_updater: Box<dyn LeaderUpdater>,
        mut transaction_receiver: mpsc::Receiver<TransactionBatch>,
        cancel: CancellationToken,
    ) -> Result<SendTransactionStatsPerAddr, ConnectionWorkersSchedulerError> {
        let endpoint = Self::setup_endpoint(bind, validator_identity)?;
        debug!("Client endpoint bind address: {:?}", endpoint.local_addr());
        let mut workers = WorkersCache::new(num_connections, cancel.clone());
        let mut send_stats_per_addr = SendTransactionStatsPerAddr::new();

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
            let updated_leaders = leader_updater.next_leaders(lookahead_slots);

            let (new_leaders, future_leaders) = split_leaders(&updated_leaders, &leaders_fanout);
            for new_leader in new_leaders {
                if !workers.contains(new_leader) {
                    debug!("No existing workers for {new_leader:?}, starting a new one.");
                    let stats = send_stats_per_addr.entry(new_leader.ip()).or_default();
                    let worker = Self::spawn_worker(
                        &endpoint,
                        new_leader,
                        worker_channel_size,
                        skip_check_transaction_age,
                        max_reconnect_attempts,
                        stats.clone(),
                    );
                    let shutdown_worker = workers.push(*new_leader, worker).await;
                    if let Some(shutdown_worker) = shutdown_worker {
                        tokio::spawn(async move {
                            let leader = shutdown_worker.leader();
                            let res = shutdown_worker.shutdown().await;
                            if let Err(err) = res {
                                debug!("Error while shutting down worker for {leader}: {err}");
                            }
                        });
                    }
                }

                tokio::select! {
                    send_res = workers.send_transactions_to_address(new_leader, transaction_batch.clone()) => match send_res {
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
            }

            // add future leaders to the cache to hide the latency of opening the
            // connection.
            for peer in future_leaders {
                if !workers.contains(peer) {
                    let stats = send_stats_per_addr.entry(peer.ip()).or_default();
                    let worker = Self::spawn_worker(
                        &endpoint,
                        peer,
                        worker_channel_size,
                        skip_check_transaction_age,
                        max_reconnect_attempts,
                        stats.clone(),
                    );
                    workers.push(*peer, worker).await;
                }
            }
        }

        workers.shutdown().await;

        endpoint.close(0u32.into(), b"Closing connection");
        leader_updater.stop().await;
        Ok(send_stats_per_addr)
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
        max_reconnect_attempts: usize,
        stats: Arc<SendTransactionStats>,
    ) -> WorkerInfo {
        let (txs_sender, txs_receiver) = mpsc::channel(worker_channel_size);
        let endpoint = endpoint.clone();
        let peer = *peer;

        let (mut worker, cancel) = ConnectionWorker::new(
            endpoint,
            peer,
            txs_receiver,
            skip_check_transaction_age,
            max_reconnect_attempts,
            stats,
        );
        let handle = tokio::spawn(async move {
            worker.run().await;
        });

        WorkerInfo::new(txs_sender, handle, cancel)
    }
}

/// Splits the input vector of leaders into two parts based on the `fanout` configuration:
/// * the first vector contains the leaders to which transactions will be sent.
/// * the second vector contains the remaining leaders, used to warm up connections.
fn split_leaders<'a>(
    leaders: &'a [SocketAddr],
    fanout: &'a LeadersFanout,
) -> (Vec<&'a SocketAddr>, Vec<&'a SocketAddr>) {
    match fanout {
        LeadersFanout::All => (leaders.iter().collect(), Vec::new()), // All elements go to the first vector
        LeadersFanout::Selected(count) => {
            let (selected, remaining) = leaders.split_at((*count).min(leaders.len())); // Split at the specified count or max length
            (selected.iter().collect(), remaining.iter().collect())
        }
    }
}
