//! This module defines [`WorkersCache`] along with aux struct [`WorkerInfo`]. These
//! structures provide mechanisms for caching workers, sending transaction
//! batches, and gathering send transaction statistics.

use {
    crate::{
        SendTransactionStats,
        connection_worker::ConnectionWorker,
        logging::{debug, trace},
        transaction_batch::TransactionBatch,
    },
    lru::LruCache,
    quinn::Endpoint,
    std::{net::SocketAddr, sync::Arc, time::Duration},
    thiserror::Error,
    tokio::{
        sync::mpsc::{self, error::TrySendError},
        task::{JoinHandle, JoinSet},
    },
    tokio_util::sync::CancellationToken,
};

/// [`WorkerInfo`] holds information about a worker responsible for sending
/// transaction batches.
pub struct WorkerInfo {
    sender: mpsc::Sender<TransactionBatch>,
    handle: JoinHandle<()>,
    cancel: CancellationToken,
}

impl WorkerInfo {
    pub fn new(
        sender: mpsc::Sender<TransactionBatch>,
        handle: JoinHandle<()>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            sender,
            handle,
            cancel,
        }
    }

    fn try_send_transactions(&self, txs_batch: TransactionBatch) -> Result<(), WorkersCacheError> {
        self.sender.try_send(txs_batch).map_err(|err| match err {
            TrySendError::Full(_) => WorkersCacheError::FullChannel,
            TrySendError::Closed(_) => WorkersCacheError::ReceiverDropped,
        })?;
        Ok(())
    }

    async fn send_transactions(
        &self,
        txs_batch: TransactionBatch,
    ) -> Result<(), WorkersCacheError> {
        self.sender
            .send(txs_batch)
            .await
            .map_err(|_| WorkersCacheError::ReceiverDropped)?;
        Ok(())
    }

    /// Closes the worker by dropping the sender and awaiting the worker's
    /// statistics.
    async fn shutdown(self) -> Result<(), WorkersCacheError> {
        self.cancel.cancel();
        drop(self.sender);
        self.handle
            .await
            .map_err(|_| WorkersCacheError::TaskJoinFailure)?;
        Ok(())
    }

    /// Returns `true` if the worker is still active and able to send
    /// transactions.
    fn is_active(&self) -> bool {
        !(self.cancel.is_cancelled() || self.sender.is_closed())
    }
}

/// Spawns a worker to handle communication with a given peer.
pub fn spawn_worker(
    endpoint: &Endpoint,
    peer: &SocketAddr,
    worker_channel_size: usize,
    skip_check_transaction_age: bool,
    max_reconnect_attempts: usize,
    handshake_timeout: Duration,
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
        handshake_timeout,
    );
    let handle = tokio::spawn(async move {
        worker.run().await;
    });

    WorkerInfo::new(txs_sender, handle, cancel)
}

/// [`WorkersCache`] manages and caches workers. It uses an LRU cache to store and
/// manage workers. It also tracks transaction statistics for each peer.
pub struct WorkersCache {
    workers: LruCache<SocketAddr, WorkerInfo>,

    /// Indicates that the `WorkersCache` is been `shutdown()`, interrupting any outstanding
    /// `send_transactions_to_address()` invocations.
    cancel: CancellationToken,
}

#[derive(Debug, Error, PartialEq)]
pub enum WorkersCacheError {
    /// typically happens when the client could not establish the connection.
    #[error("Work receiver has been dropped unexpectedly.")]
    ReceiverDropped,

    #[error("Worker's channel is full.")]
    FullChannel,

    #[error("Task failed to join.")]
    TaskJoinFailure,

    #[error("The WorkersCache is being shutdown.")]
    ShutdownError,

    #[error("No worker exists for the specified peer.")]
    WorkerNotFound,
}

impl WorkersCache {
    pub fn new(capacity: usize, cancel: CancellationToken) -> Self {
        Self {
            workers: LruCache::new(capacity),
            cancel,
        }
    }

    /// Checks if the worker for a given peer exists and it hasn't been
    /// cancelled.
    pub fn contains(&self, peer: &SocketAddr) -> bool {
        self.workers.contains(peer)
    }

    pub fn push(&mut self, leader: SocketAddr, peer_worker: WorkerInfo) -> Option<ShutdownWorker> {
        if let Some((leader, popped_worker)) = self.workers.push(leader, peer_worker) {
            return Some(ShutdownWorker {
                leader,
                worker: popped_worker,
            });
        }
        None
    }

    pub fn pop(&mut self, leader: SocketAddr) -> Option<ShutdownWorker> {
        if let Some(popped_worker) = self.workers.pop(&leader) {
            return Some(ShutdownWorker {
                leader,
                worker: popped_worker,
            });
        }
        None
    }

    /// Ensures a worker exists for the given peer, creating one if necessary.
    ///
    /// Returns any evicted worker that needs shutdown.
    pub fn ensure_worker(
        &mut self,
        peer: SocketAddr,
        endpoint: &Endpoint,
        worker_channel_size: usize,
        skip_check_transaction_age: bool,
        max_reconnect_attempts: usize,
        handshake_timeout: Duration,
        stats: Arc<SendTransactionStats>,
    ) -> Option<ShutdownWorker> {
        if let Some(worker) = self.workers.get(&peer) {
            // if worker is active, we will reuse it. Otherwise, we will spawn
            // the new one and the existing will be popped out.
            if worker.is_active() {
                return None;
            }
        }
        trace!("No active worker for peer {peer}, respawning.");

        let worker = spawn_worker(
            endpoint,
            &peer,
            worker_channel_size,
            skip_check_transaction_age,
            max_reconnect_attempts,
            handshake_timeout,
            stats,
        );

        self.push(peer, worker)
    }

    /// Attempts to send immediately a batch of transactions to the worker for a
    /// given peer.
    ///
    /// This method returns immediately if the channel of worker corresponding
    /// to this peer is full returning error [`WorkersCacheError::FullChannel`].
    /// If no worker exists for the peer, it returns
    /// [`WorkersCacheError::WorkerNotFound`]. If it happens that the peer's
    /// worker is stopped, it returns [`WorkersCacheError::ShutdownError`].
    /// In case if the worker is not stopped but it's channel is unexpectedly
    /// dropped, it returns [`WorkersCacheError::ReceiverDropped`].
    ///
    /// Note: The worker existence check is necessary because workers can fail
    /// asynchronously between creation and sending. Worker tasks may exit
    /// due to connection failures, network issues, or cache evictions,
    /// making a previously created worker unavailable.
    pub fn try_send_transactions_to_address(
        &mut self,
        peer: &SocketAddr,
        txs_batch: TransactionBatch,
    ) -> Result<(), WorkersCacheError> {
        let Self {
            workers, cancel, ..
        } = self;
        if cancel.is_cancelled() {
            return Err(WorkersCacheError::ShutdownError);
        }

        let current_worker = workers.get(peer).ok_or(WorkersCacheError::WorkerNotFound)?;

        let send_res = current_worker.try_send_transactions(txs_batch);

        if let Err(WorkersCacheError::ReceiverDropped) = send_res {
            debug!(
                "Failed to deliver transaction batch for leader {}, drop batch.",
                peer.ip()
            );
            if let Some(current_worker) = workers.pop(peer) {
                shutdown_worker(ShutdownWorker {
                    leader: *peer,
                    worker: current_worker,
                })
            }
        }

        send_res
    }

    /// Sends a batch of transactions to the worker for a given peer.
    ///
    /// If the worker for the peer is disconnected or fails, it
    /// is removed from the cache. If no worker exists for the peer,
    /// it returns [`WorkersCacheError::WorkerNotFound`].
    #[allow(
        dead_code,
        reason = "This method will be used in the upcoming changes to implement optional \
                  backpressure on the sender."
    )]
    pub async fn send_transactions_to_address(
        &mut self,
        peer: &SocketAddr,
        txs_batch: TransactionBatch,
    ) -> Result<(), WorkersCacheError> {
        let Self {
            workers, cancel, ..
        } = self;

        let body = async move {
            let current_worker = workers.get(peer).ok_or(WorkersCacheError::WorkerNotFound)?;

            let send_res = current_worker.send_transactions(txs_batch).await;
            if let Err(WorkersCacheError::ReceiverDropped) = send_res {
                // Remove the worker from the cache, if the peer has disconnected.
                if let Some(current_worker) = workers.pop(peer) {
                    shutdown_worker(ShutdownWorker {
                        leader: *peer,
                        worker: current_worker,
                    })
                }
            }

            send_res
        };

        cancel
            .run_until_cancelled(body)
            .await
            .unwrap_or(Err(WorkersCacheError::ShutdownError))
    }

    /// Flushes the cache and asynchronously shuts down all workers. This method
    /// doesn't wait for the completion of all the shutdown tasks.
    pub(crate) fn flush(&mut self) {
        while let Some((peer, current_worker)) = self.workers.pop_lru() {
            shutdown_worker(ShutdownWorker {
                leader: peer,
                worker: current_worker,
            });
        }
    }

    /// Closes and removes all workers in the cache. This is typically done when
    /// shutting down the system.
    ///
    /// The method awaits the completion of all shutdown tasks, ensuring that
    /// each worker is properly terminated.
    pub async fn shutdown(&mut self) {
        // Interrupt any outstanding `send_transactions()` calls.
        self.cancel.cancel();

        let mut tasks = JoinSet::new();
        while let Some((peer, current_worker)) = self.workers.pop_lru() {
            let shutdown_worker = ShutdownWorker {
                leader: peer,
                worker: current_worker,
            };
            tasks.spawn(shutdown_worker.shutdown());
        }
        while let Some(res) = tasks.join_next().await {
            if let Err(err) = res {
                debug!("A shutdown task failed: {err}");
            }
        }
    }
}

/// [`ShutdownWorker`] takes care of stopping the worker. It's method
/// `shutdown()` should be executed in a separate task to hide the latency of
/// finishing worker gracefully.
pub struct ShutdownWorker {
    leader: SocketAddr,
    worker: WorkerInfo,
}

impl ShutdownWorker {
    pub(crate) fn leader(&self) -> SocketAddr {
        self.leader
    }

    pub(crate) async fn shutdown(self) -> Result<(), WorkersCacheError> {
        self.worker.shutdown().await
    }
}

pub fn shutdown_worker(worker: ShutdownWorker) {
    tokio::spawn(async move {
        let leader = worker.leader();
        let res = worker.shutdown().await;
        if let Err(err) = res {
            debug!("Error while shutting down worker for {leader}: {err}");
        }
    });
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            SendTransactionStats,
            connection_worker::DEFAULT_MAX_CONNECTION_HANDSHAKE_TIMEOUT,
            connection_workers_scheduler::BindTarget,
            quic_networking::{create_client_config, create_client_endpoint},
            send_transaction_stats::SendTransactionStatsNonAtomic,
            transaction_batch::TransactionBatch,
            workers_cache::{WorkersCache, WorkersCacheError, spawn_worker},
        },
        quinn::Endpoint,
        solana_net_utils::sockets::{bind_to_localhost_unique, unique_port_range_for_tests},
        solana_tls_utils::QuicClientCertificate,
        std::{
            net::{Ipv4Addr, SocketAddr},
            sync::Arc,
            time::Duration,
        },
        tokio::time::{Instant, sleep, timeout},
        tokio_util::sync::CancellationToken,
    };

    // Specify the pessimistic time to finish generation and result checks.
    const TEST_MAX_TIME: Duration = Duration::from_secs(5);

    fn create_test_endpoint() -> Endpoint {
        let socket = bind_to_localhost_unique().unwrap();
        let client_config = create_client_config(&QuicClientCertificate::new(None), None);
        create_client_endpoint(BindTarget::Socket(socket), client_config).unwrap()
    }

    #[tokio::test]
    async fn test_worker_stopped_after_failed_connect() {
        let endpoint = create_test_endpoint();

        let port_range = unique_port_range_for_tests(2);
        let peer: SocketAddr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port_range.start);

        let worker_channel_size = 1;
        let skip_check_transaction_age = true;
        let max_reconnect_attempts = 0;
        let stats = Arc::new(SendTransactionStats::default());
        let worker_info = spawn_worker(
            &endpoint,
            &peer,
            worker_channel_size,
            skip_check_transaction_age,
            max_reconnect_attempts,
            DEFAULT_MAX_CONNECTION_HANDSHAKE_TIMEOUT,
            stats.clone(),
        );

        timeout(TEST_MAX_TIME, worker_info.handle)
            .await
            .unwrap_or_else(|_| panic!("Should stop in less than {TEST_MAX_TIME:?}."))
            .expect("Worker task should finish successfully.");
        assert_eq!(
            stats.read_and_reset(),
            SendTransactionStatsNonAtomic {
                connection_error_timed_out: 1,
                ..Default::default()
            }
        );
    }

    #[tokio::test]
    async fn test_worker_shutdown() {
        let endpoint = create_test_endpoint();

        let port_range = unique_port_range_for_tests(2);
        let peer: SocketAddr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port_range.start);

        let worker_channel_size = 1;
        let skip_check_transaction_age = true;
        let max_reconnect_attempts = 0;
        let stats = Arc::new(SendTransactionStats::default());
        let worker_info = spawn_worker(
            &endpoint,
            &peer,
            worker_channel_size,
            skip_check_transaction_age,
            max_reconnect_attempts,
            DEFAULT_MAX_CONNECTION_HANDSHAKE_TIMEOUT,
            stats.clone(),
        );

        timeout(TEST_MAX_TIME, worker_info.shutdown())
            .await
            .unwrap_or_else(|_| panic!("Should stop in less than {TEST_MAX_TIME:?}."))
            .expect("Worker task should finish successfully.");
    }

    // Verifies that a worker which terminates (e.g. due to connection failure)
    // is properly detected, its sender is closed, and it is removed from the
    // `WorkersCache`.
    #[tokio::test]
    async fn test_worker_removed_after_exit() {
        let endpoint = create_test_endpoint();

        let cancel = CancellationToken::new();
        let mut cache = WorkersCache::new(10, cancel.clone());

        let port_range = unique_port_range_for_tests(2);
        let peer: SocketAddr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port_range.start);
        let worker_channel_size = 1;
        let skip_check_transaction_age = true;
        let max_reconnect_attempts = 0;
        let stats = Arc::new(SendTransactionStats::default());
        let worker = spawn_worker(
            &endpoint,
            &peer,
            worker_channel_size,
            skip_check_transaction_age,
            max_reconnect_attempts,
            DEFAULT_MAX_CONNECTION_HANDSHAKE_TIMEOUT,
            stats.clone(),
        );
        assert!(cache.push(peer, worker).is_none());

        let worker_info = cache.workers.peek(&peer).unwrap();
        // wait until sender is closed which happens when task has finished.
        let start = Instant::now();
        while !worker_info.sender.is_closed() {
            if start.elapsed() > TEST_MAX_TIME {
                panic!("Sender did not close in {TEST_MAX_TIME:?}");
            }
            sleep(Duration::from_millis(500)).await;
        }

        assert!(!worker_info.is_active(), "Worker should be inactive");

        // try to send to this worker — should fail and remove the worker
        let result = cache
            .try_send_transactions_to_address(&peer, TransactionBatch::new(vec![vec![0u8; 1]]));

        assert_eq!(result, Err(WorkersCacheError::ReceiverDropped));
        assert!(
            !cache.contains(&peer),
            "worker should be removed after failure"
        );

        // Cleanup
        cancel.cancel();
        cache.shutdown().await;

        assert_eq!(
            stats.read_and_reset(),
            SendTransactionStatsNonAtomic {
                connection_error_timed_out: 1,
                ..Default::default()
            }
        );
    }
}
