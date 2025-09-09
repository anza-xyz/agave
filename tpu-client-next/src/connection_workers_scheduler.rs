//! This module defines [`ConnectionWorkersScheduler`] which sends transactions
//! to the upcoming leaders.

use {
    super::leader_updater::LeaderUpdater,
    crate::{
        connection_worker::DEFAULT_MAX_CONNECTION_HANDSHAKE_TIMEOUT,
        logging::{debug, warn},
        quic_networking::{
            create_client_config, create_client_endpoint, QuicClientCertificate, QuicError,
        },
        transaction_batch::TransactionBatch,
        workers_cache::{shutdown_worker, spawn_worker, WorkersCache, WorkersCacheError},
        SendTransactionStats,
    },
    async_trait::async_trait,
    quinn::{ClientConfig, Endpoint},
    solana_keypair::Keypair,
    std::{
        net::{SocketAddr, UdpSocket},
        sync::Arc,
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
    update_identity_receiver: watch::Receiver<Option<StakeIdentity>>,
    cancel: CancellationToken,
    stats: Arc<SendTransactionStats>,
}

/// Errors that arise from running [`ConnectionWorkersSchedulerError`].
#[derive(Debug, Error)]
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

impl StakeIdentity {
    pub fn new(keypair: &Keypair) -> Self {
        Self(QuicClientCertificate::new(Some(keypair)))
    }

    pub fn as_certificate(&self) -> &QuicClientCertificate {
        &self.0
    }
}

impl From<StakeIdentity> for QuicClientCertificate {
    fn from(identity: StakeIdentity) -> Self {
        identity.0
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
    ) -> Result<(), ConnectionWorkersSchedulerError>;
}

impl ConnectionWorkersScheduler {
    /// Creates the scheduler, which manages the distribution of transactions to
    /// the network's upcoming leaders.
    pub fn new(
        leader_updater: Box<dyn LeaderUpdater>,
        transaction_receiver: mpsc::Receiver<TransactionBatch>,
        update_identity_receiver: watch::Receiver<Option<StakeIdentity>>,
        cancel: CancellationToken,
    ) -> Self {
        let stats = Arc::new(SendTransactionStats::default());
        Self {
            leader_updater,
            transaction_receiver,
            update_identity_receiver,
            cancel,
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
        config: ConnectionWorkersSchedulerConfig,
    ) -> Result<Arc<SendTransactionStats>, ConnectionWorkersSchedulerError> {
        self.run_with_broadcaster::<NonblockingBroadcaster>(config)
            .await
    }

    /// Starts the scheduler, which manages the distribution of transactions to
    /// the network's upcoming leaders. `Broadcaster` allows to customize the
    /// way transactions are send to the leaders, see [`WorkersBroadcaster`].
    ///
    /// Runs the main loop that handles worker scheduling and management for
    /// connections. Returns [`SendTransactionStats`] or an error.
    ///
    /// Importantly, if some transactions were not delivered due to network
    /// problems, they will not be retried when the problem is resolved.
    pub async fn run_with_broadcaster<Broadcaster: WorkersBroadcaster>(
        self,
        ConnectionWorkersSchedulerConfig {
            bind,
            stake_identity,
            num_connections,
            skip_check_transaction_age,
            worker_channel_size,
            max_reconnect_attempts,
            leaders_fanout,
        }: ConnectionWorkersSchedulerConfig,
    ) -> Result<Arc<SendTransactionStats>, ConnectionWorkersSchedulerError> {
        let ConnectionWorkersScheduler {
            mut leader_updater,
            mut transaction_receiver,
            mut update_identity_receiver,
            cancel,
            stats,
        } = self;
        let mut endpoint = setup_endpoint(bind, stake_identity)?;

        debug!("Client endpoint bind address: {:?}", endpoint.local_addr());
        let mut workers = WorkersCache::new(num_connections, cancel.clone());

        let mut last_error = None;
        // flag to ensure that the section handling
        // `update_identity_receiver.changed()` is entered only once when the
        // channel is dropped.
        let mut identity_updater_is_active = true;

        loop {
            let transaction_batch: TransactionBatch = tokio::select! {
                recv_res = transaction_receiver.recv() => match recv_res {
                    Some(txs) => txs,
                    None => {
                        debug!("End of `transaction_receiver`: shutting down.");
                        break;
                    }
                },
                res = update_identity_receiver.changed(), if identity_updater_is_active => {
                    let Ok(()) = res else {
                        // Sender has been dropped; log and continue
                        debug!("Certificate update channel closed; continuing without further updates.");
                        identity_updater_is_active = false;
                        continue;
                    };

                    let client_config = build_client_config(update_identity_receiver.borrow_and_update().as_ref());
                    endpoint.set_default_client_config(client_config);
                    // Flush workers since they are handling connections created
                    // with outdated certificate.
                    workers.flush();
                    debug!("Updated certificate.");
                    continue;
                },
                () = cancel.cancelled() => {
                    debug!("Cancelled: Shutting down");
                    break;
                }
            };

            let connect_leaders = leader_updater.next_leaders(leaders_fanout.connect);
            let send_leaders = extract_send_leaders(&connect_leaders, leaders_fanout.send);

            // add future leaders to the cache to hide the latency of opening
            // the connection.
            for peer in connect_leaders {
                if !workers.contains(&peer) {
                    let worker = spawn_worker(
                        &endpoint,
                        &peer,
                        worker_channel_size,
                        skip_check_transaction_age,
                        max_reconnect_attempts,
                        DEFAULT_MAX_CONNECTION_HANDSHAKE_TIMEOUT,
                        stats.clone(),
                    );
                    if let Some(pop_worker) = workers.push(peer, worker) {
                        shutdown_worker(pop_worker)
                    }
                }
            }

            if let Err(error) =
                Broadcaster::send_to_workers(&mut workers, &send_leaders, transaction_batch).await
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
        Ok(stats)
    }
}

/// Sets up the QUIC endpoint for the scheduler to handle connections.
fn setup_endpoint(
    bind: BindTarget,
    stake_identity: Option<StakeIdentity>,
) -> Result<Endpoint, ConnectionWorkersSchedulerError> {
    let client_config = build_client_config(stake_identity.as_ref());
    let endpoint = create_client_endpoint(bind, client_config)?;
    Ok(endpoint)
}

fn build_client_config(stake_identity: Option<&StakeIdentity>) -> ClientConfig {
    let client_certificate = match stake_identity {
        Some(identity) => identity.as_certificate(),
        None => &QuicClientCertificate::new(None),
    };
    create_client_config(client_certificate)
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
                    if let Some(pop_worker) = workers.pop(*new_leader) {
                        shutdown_worker(pop_worker)
                    }
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

/// Extracts leaders to send transactions to, up to `send_fanout` count.
/// 
/// In Solana's leader schedule, each leader produces blocks for [`solana_clock::NUM_CONSECUTIVE_LEADER_SLOTS`]
/// consecutive slots. The schedule might look like: [L1, L1, L2, L3], where each entry
/// represents a leader producing [`solana_clock::NUM_CONSECUTIVE_LEADER_SLOTS`] slots. 
/// So L1, L1 means L1 produces 2 * [`solana_clock::NUM_CONSECUTIVE_LEADER_SLOTS`] consecutive slots.
/// 
/// This function skips consecutive duplicates because sending the same transaction multiple
/// times to the same leader during their consecutive slot range is redundant. 
/// 
/// However, non-consecutive duplicates (e.g., L1 appears again after L2) are preserved because
/// they represent legitimate retry opportunities at different time windows.
/// 
/// # Examples
/// 
/// [L1, L1, L2] with fanout=3 -> [L1, L2]
/// L1 has 2 * NUM_CONSECUTIVE_LEADER_SLOTS consecutive slots, no need to send twice
///
/// [L1, L2, L1] with fanout=3 -> [L1, L2, L1]
/// L1 appears at different times, worth retrying
pub fn extract_send_leaders(leaders: &[SocketAddr], send_fanout: usize) -> Vec<SocketAddr> {
    let mut result = Vec::with_capacity(send_fanout);
    let mut prev_leader: Option<SocketAddr> = None;
    
    for &leader in leaders {
        // Skip only consecutive duplicates
        if prev_leader != Some(leader) {
            result.push(leader);
            if result.len() >= send_fanout {
                break;
            }
        }
        prev_leader = Some(leader);
    }
    
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn make_addr(port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
    }

    #[test]
    fn test_consecutive_duplicates_skipped() {
        let l1 = make_addr(1001);
        let l2 = make_addr(1002);
        let l3 = make_addr(1003);
        
        // Consecutive duplicates should be skipped
        let leaders = vec![l1, l1, l1, l2, l2, l3];
        assert_eq!(extract_send_leaders(&leaders, 3), vec![l1, l2, l3]);
    }

    #[test]
    fn test_non_consecutive_duplicates_kept() {
        let l1 = make_addr(1001);
        let l2 = make_addr(1002);
        
        // Non-consecutive duplicates are legitimate retries
        let leaders = vec![l1, l2, l1];
        assert_eq!(extract_send_leaders(&leaders, 3), vec![l1, l2, l1]);
    }
}