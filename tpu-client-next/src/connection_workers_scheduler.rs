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
        workers_cache::{maybe_shutdown_worker, WorkerInfo, WorkersCache, WorkersCacheError},
        SendTransactionStats,
    },
    async_trait::async_trait,
    log::*,
    quinn::Endpoint,
    solana_keypair::Keypair,
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

/// [`Fanout`] is a configuration struct that specifies how many leaders should
/// be targeted when sending transactions and connecting.
///
/// Note, that the unit is number of leaders per
/// [`NUM_CONSECUTIVE_LEADER_SLOTS`]. It means that if the leader schedule is
/// [L1, L1, L1, L1, L1, L1, L1, L1, L2, L2, L2, L2], the leaders per
/// consecutive leader slots are [L1, L1, L2], so there are 3 of them.
///
/// The idea of having a separate `connect` parameter is to create a set of
/// nodes to connect to in advance in order to hide the latency of opening new
/// connection. Hence, `connect` must be greater or equal to `send`
pub struct Fanout {
    /// The number of leaders to target for sending transactions.
    pub send: usize,

    /// The number of leaders to target for establishing connections.
    pub connect: usize,
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

    /// Configures the number of leaders to connect to and send transactions to.
    pub leaders_fanout: Fanout,
}

/// The [`WorkersBroadcaster`] trait defines a customizable mechanism for
/// sending transaction batches to workers corresponding to the provided list of
/// addresses. Implementations of this trait are used by the
/// [`ConnectionWorkersScheduler`] to distribute transactions to workers
/// accordingly.
#[async_trait]
pub trait WorkersBroadcaster {
    /// Sends a `transaction_batch` to workers associated with the given
    /// `leaders` addresses.
    ///
    /// Returns error if a critical issue occurs, e.g. the implementation
    /// encounters an unrecoverable error. In this case, it will trigger
    /// stopping the scheduler and cleaning all the data.
    async fn send_to_workers(
        workers: &mut WorkersCache,
        leaders: &[SocketAddr],
        transaction_batch: TransactionBatch,
    ) -> Result<(), ConnectionWorkersSchedulerError>;
}

pub type TransactionStatsAndReceiver = (
    SendTransactionStatsPerAddr,
    mpsc::Receiver<TransactionBatch>,
);

impl ConnectionWorkersScheduler {
    /// Starts the scheduler, which manages the distribution of transactions to
    /// the network's upcoming leaders.
    ///
    /// This method is a shorthand for
    /// [`ConnectionWorkersScheduler::run_with_broadcaster`] using
    /// [`NonblockingBroadcaster`] strategy.
    ///
    /// Transactions that fail to be delivered to workers due to full channels
    /// will be dropped. The same for transactions that failed to be delivered
    /// over the network.
    pub async fn run(
        config: ConnectionWorkersSchedulerConfig,
        leader_updater: Box<dyn LeaderUpdater>,
        transaction_receiver: mpsc::Receiver<TransactionBatch>,
        cancel: CancellationToken,
    ) -> Result<TransactionStatsAndReceiver, ConnectionWorkersSchedulerError> {
        Self::run_with_broadcaster::<NonblockingBroadcaster>(
            config,
            leader_updater,
            transaction_receiver,
            cancel,
        )
        .await
    }

    /// Starts the scheduler, which manages the distribution of transactions to
    /// the network's upcoming leaders. `Broadcaster` allows to customize the
    /// way transactions are send to the leaders, see [`WorkersBroadcaster`].
    ///
    /// Runs the main loop that handles worker scheduling and management for
    /// connections. Returns the error quic statistics per connection address or
    /// an error along with receiver for transactions. The receiver returned
    /// back to the user because in some cases we need to re-utilize the same
    /// receiver for the new scheduler. For example, this happens when the
    /// identity for the validator is updated.
    ///
    /// Importantly, if some transactions were not delivered due to network
    /// problems, they will not be retried when the problem is resolved.
    pub async fn run_with_broadcaster<Broadcaster: WorkersBroadcaster>(
        ConnectionWorkersSchedulerConfig {
            bind,
            stake_identity,
            num_connections,
            skip_check_transaction_age,
            worker_channel_size,
            max_reconnect_attempts,
            leaders_fanout,
        }: ConnectionWorkersSchedulerConfig,
        mut leader_updater: Box<dyn LeaderUpdater>,
        mut transaction_receiver: mpsc::Receiver<TransactionBatch>,
        cancel: CancellationToken,
    ) -> Result<TransactionStatsAndReceiver, ConnectionWorkersSchedulerError> {
        let endpoint = Self::setup_endpoint(bind, stake_identity.as_ref())?;
        debug!("Client endpoint bind address: {:?}", endpoint.local_addr());
        let mut workers = WorkersCache::new(num_connections, cancel.clone());
        let mut send_stats_per_addr = SendTransactionStatsPerAddr::new();

        let mut last_error = None;

        loop {
            let transaction_batch: TransactionBatch = tokio::select! {
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

            let updated_leaders = leader_updater.next_leaders(leaders_fanout.connect);

            let (fanout_leaders, connect_leaders) =
                split_leaders(&updated_leaders, &leaders_fanout);
            // add future leaders to the cache to hide the latency of opening
            // the connection.
            for peer in connect_leaders {
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
                    maybe_shutdown_worker(workers.push(*peer, worker));
                }
            }

            if let Err(error) =
                Broadcaster::send_to_workers(&mut workers, fanout_leaders, transaction_batch).await
            {
                last_error = Some(error);
                break;
            }
        }

        workers.shutdown().await;

        endpoint.close(0u32.into(), b"Closing connection");
        leader_updater.stop().await;
        if let Some(error) = last_error {
            return Err(error);
        }
        Ok((send_stats_per_addr, transaction_receiver))
    }

    /// Sets up the QUIC endpoint for the scheduler to handle connections.
    fn setup_endpoint(
        bind: SocketAddr,
        stake_identity: Option<&Keypair>,
    ) -> Result<Endpoint, ConnectionWorkersSchedulerError> {
        let client_certificate = QuicClientCertificate::new(stake_identity);
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

/// [`NonblockingBroadcaster`] attempts to immediately send transactions to all
/// the workers. If worker cannot accept transactions because it's channel is
/// full, the transactions will not be sent to this worker.
struct NonblockingBroadcaster;

#[async_trait]
impl WorkersBroadcaster for NonblockingBroadcaster {
    async fn send_to_workers(
        workers: &mut WorkersCache,
        leaders: &[SocketAddr],
        transaction_batch: TransactionBatch,
    ) -> Result<(), ConnectionWorkersSchedulerError> {
        for new_leader in leaders {
            if !workers.contains(new_leader) {
                warn!("No existing worker for {new_leader:?}, skip sending to this leader.");
                continue;
            }

            let send_res =
                workers.try_send_transactions_to_address(new_leader, transaction_batch.clone());
            match send_res {
                Ok(()) => (),
                Err(WorkersCacheError::ShutdownError) => {
                    debug!("Connection to {new_leader} was closed, worker cache shutdown");
                }
                Err(WorkersCacheError::ReceiverDropped) => {
                    // Remove the worker from the cache, if the peer has disconnected.
                    maybe_shutdown_worker(workers.pop(*new_leader));
                }
                Err(err) => {
                    warn!("Connection to {new_leader} was closed, worker error: {err}");
                    // If we have failed to send batch, it will be dropped.
                }
            }
        }
        Ok(())
    }
}

/// Splits `leaders` into two slices based on the `fanout` configuration:
/// * the first slice contains the leaders to which transactions will be sent,
/// * the second vector contains the leaders, used to warm up connections. This
///   slice includes the first set.
fn split_leaders<'leaders>(
    leaders: &'leaders [SocketAddr],
    fanout: &Fanout,
) -> (&'leaders [SocketAddr], &'leaders [SocketAddr]) {
    let Fanout { send, connect } = fanout;
    assert!(send <= connect);
    let send_count = (*send).min(leaders.len());
    let connect_count = (*connect).min(leaders.len());

    let send_slice = &leaders[..send_count];
    let connect_slice = &leaders[..connect_count];

    (send_slice, connect_slice)
}
