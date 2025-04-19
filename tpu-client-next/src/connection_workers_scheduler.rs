//! This module defines [`ConnectionWorkersScheduler`] which sends transactions
//! to the upcoming leaders.

use {
    super::leader_updater::LeaderUpdater,
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
    std::{
        net::{SocketAddr, UdpSocket},
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
    },
    thiserror::Error,
    tokio::sync::{mpsc, watch},
    tokio_util::sync::CancellationToken,
};
pub type TransactionReceiver = mpsc::Receiver<TransactionBatch>;

/// The [`ConnectionWorkersScheduler`] sends transactions from the provided
/// receiver channel to upcoming leaders. It obtains information about future
/// leaders from the implementation of the [`LeaderUpdater`] trait.
///
/// Internally, it enables the management and coordination of multiple network
/// connections, schedules and oversees connection workers.
pub struct ConnectionWorkersScheduler {
    leader_updater: Box<dyn LeaderUpdater>,
    transaction_receiver: TransactionReceiver,
    stats: Arc<SendTransactionStats>,
}

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
/// [`solana_clock::NUM_CONSECUTIVE_LEADER_SLOTS`]. It means that if the leader schedule is
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
    pub bind: BindTarget,

    /// Optional stake identity keypair used in the endpoint certificate for
    /// identifying the sender.
    pub stake_identity: Option<StakeIdentity>,

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

/// The [`BindTarget`] enum defines how the UDP socket should be bound:
/// either by providing a [`SocketAddr`] or an existing [`UdpSocket`].
pub enum BindTarget {
    Address(SocketAddr),
    Socket(UdpSocket),
}

/// The [`StakeIdentity`] structure provides a convenient abstraction for handling
/// [`Keypair`] when creating a QUIC certificate. Since `Keypair` does not implement
/// [`Clone`], it cannot be moved in situations where [`ConnectionWorkersSchedulerConfig`]
/// needs to be transferred. This wrapper structure allows the use of either a `Keypair`
/// or a `&Keypair` to create a certificate, which is stored internally and later
/// consumed by [`ConnectionWorkersScheduler`] to create an endpoint.
pub struct StakeIdentity(QuicClientCertificate);

impl From<Keypair> for StakeIdentity {
    fn from(keypair: Keypair) -> Self {
        Self(QuicClientCertificate::new(Some(&keypair)))
    }
}

impl From<&Keypair> for StakeIdentity {
    fn from(keypair: &Keypair) -> Self {
        Self(QuicClientCertificate::new(Some(keypair)))
    }
}

impl From<StakeIdentity> for QuicClientCertificate {
    fn from(identity: StakeIdentity) -> Self {
        identity.0
    }
}

impl StakeIdentity {
    pub fn as_certificate(&self) -> &QuicClientCertificate {
        &self.0
    }
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
        generation: u64,
    ) -> Result<(), ConnectionWorkersSchedulerError>;
}

impl ConnectionWorkersScheduler {
    /// Creates the scheduler, which manages the distribution of transactions to
    /// the network's upcoming leaders.
    pub fn new(
        leader_updater: Box<dyn LeaderUpdater>,
        transaction_receiver: mpsc::Receiver<TransactionBatch>,
    ) -> Self {
        let stats = Arc::new(SendTransactionStats::default());
        Self {
            leader_updater,
            transaction_receiver,
            stats,
        }
    }

    /// Retrieves a reference to the statistics of the scheduler
    pub fn get_stats(&self) -> Arc<SendTransactionStats> {
        self.stats.clone()
    }

    /// Starts the scheduler.
    ///
    /// This method is a shorthand for
    /// [`ConnectionWorkersScheduler::run_with_broadcaster`] using
    /// `NonblockingBroadcaster` strategy.
    ///
    /// Transactions that fail to be delivered to workers due to full channels
    /// will be dropped. The same for transactions that failed to be delivered
    /// over the network.
    pub async fn run(
        self,
        update_certificate_receiver: watch::Receiver<Option<StakeIdentity>>,
        config: ConnectionWorkersSchedulerConfig,
        cancel: CancellationToken,
    ) -> Result<Self, ConnectionWorkersSchedulerError> {
        self.run_with_broadcaster::<NonblockingBroadcaster>(
            update_certificate_receiver,
            config,
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
    /// an error along with receiver for transactions.
    ///
    /// Importantly, if some transactions were not delivered due to network
    /// problems, they will not be retried when the problem is resolved.
    pub async fn run_with_broadcaster<Broadcaster: WorkersBroadcaster>(
        mut self,
        mut update_certificate_receiver: watch::Receiver<Option<StakeIdentity>>,
        ConnectionWorkersSchedulerConfig {
            bind,
            stake_identity,
            num_connections,
            skip_check_transaction_age,
            worker_channel_size,
            max_reconnect_attempts,
            leaders_fanout,
        }: ConnectionWorkersSchedulerConfig,
        cancel: CancellationToken,
    ) -> Result<Self, ConnectionWorkersSchedulerError> {
        let endpoint = Self::setup_endpoint(bind, stake_identity)?;

        let connection_gen = Arc::new(AtomicU64::new(0));
        tokio::spawn({
            let cancel = cancel.clone();
            let connection_gen = connection_gen.clone();
            let mut endpoint = endpoint.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = update_certificate_receiver.changed() => {
                            let stake_identity = update_certificate_receiver.borrow_and_update();
                            let client_certificate = match stake_identity.as_ref() {
                                Some(identity) => &identity.as_certificate(),
                                None => &QuicClientCertificate::new(None),
                            };

                            let client_config = create_client_config(client_certificate);
                            endpoint.set_default_client_config(client_config);
                            connection_gen.fetch_add(1, Ordering::Relaxed);
                        }
                        _ = cancel.cancelled() => {
                            break;
                        }
                    }
                }
            }
        });
        debug!("Client endpoint bind address: {:?}", endpoint.local_addr());
        let mut workers = WorkersCache::new(num_connections, cancel.clone());

        let mut last_error = None;

        loop {
            let transaction_batch: TransactionBatch = tokio::select! {
                recv_res = self.transaction_receiver.recv() => match recv_res {
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

            let connect_leaders = self.leader_updater.next_leaders(leaders_fanout.connect);
            let send_leaders = extract_send_leaders(&connect_leaders, leaders_fanout.send);

            // add future leaders to the cache to hide the latency of opening
            // the connection.
            for peer in connect_leaders {
                let current_connection_gen = connection_gen.load(Ordering::Relaxed);
                if !workers.contains_and_valid(&peer, current_connection_gen) {
                    let current_connection_gen = connection_gen.load(Ordering::Relaxed);
                    let worker = Self::spawn_worker(
                        &endpoint,
                        &peer,
                        worker_channel_size,
                        skip_check_transaction_age,
                        max_reconnect_attempts,
                        current_connection_gen,
                        self.stats.clone(),
                    );
                    maybe_shutdown_worker(workers.push(peer, worker));
                }
            }

            let current_connection_gen = connection_gen.load(Ordering::Relaxed);
            if let Err(error) = Broadcaster::send_to_workers(
                &mut workers,
                &send_leaders,
                transaction_batch,
                current_connection_gen,
            )
            .await
            {
                last_error = Some(error);
                break;
            }
        }

        workers.shutdown().await;

        endpoint.close(0u32.into(), b"Closing connection");
        self.leader_updater.stop().await;
        if let Some(error) = last_error {
            return Err(error);
        }
        Ok(self)
    }

    /// Sets up the QUIC endpoint for the scheduler to handle connections.
    fn setup_endpoint(
        bind: BindTarget,
        stake_identity: Option<StakeIdentity>,
    ) -> Result<Endpoint, ConnectionWorkersSchedulerError> {
        let client_certificate = match stake_identity {
            Some(identity) => identity.into(),
            None => QuicClientCertificate::new(None),
        };
        let client_config = create_client_config(&client_certificate);
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
        generation: u64,
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

        WorkerInfo::new(txs_sender, handle, generation, cancel)
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
        generation: u64,
    ) -> Result<(), ConnectionWorkersSchedulerError> {
        for new_leader in leaders {
            if !workers.contains_and_valid(new_leader, generation) {
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

/// Extracts a list of unique leader addresses to which transactions will be sent.
///
/// This function selects up to `send_fanout` addresses from the `leaders` list, ensuring that
/// only unique addresses are included while maintaining their original order.
fn extract_send_leaders(leaders: &[SocketAddr], send_fanout: usize) -> Vec<SocketAddr> {
    let send_count = send_fanout.min(leaders.len());
    remove_duplicates(&leaders[..send_count])
}

/// Removes duplicate `SocketAddr` elements from the given slice while
/// preserving their original order.
fn remove_duplicates(input: &[SocketAddr]) -> Vec<SocketAddr> {
    let mut res = Vec::with_capacity(input.len());
    for address in input {
        if !res.contains(address) {
            res.push(*address);
        }
    }
    res
}
