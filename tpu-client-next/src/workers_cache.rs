//! This module defines `WorkersCache` along with aux struct `WorkerInfo`. These
//! structures provide mechanisms for caching workers, sending transaction
//! batches, and gathering send transaction statistics.

use {
    crate::transaction_batch::TransactionBatch,
    log::*,
    lru::LruCache,
    std::net::SocketAddr,
    thiserror::Error,
    tokio::{sync::mpsc, task::JoinHandle},
    tokio_util::sync::CancellationToken,
};

/// [`WorkerInfo`] holds information about a worker responsible for sending
/// transaction batches.
pub(crate) struct WorkerInfo {
    pub sender: mpsc::Sender<TransactionBatch>,
    pub handle: JoinHandle<()>,
    pub cancel: CancellationToken,
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
}

/// [`WorkersCache`] manages and caches workers. It uses an LRU cache to store and
/// manage workers. It also tracks transaction statistics for each peer.
pub(crate) struct WorkersCache {
    workers: LruCache<SocketAddr, WorkerInfo>,

    /// Indicates that the `WorkersCache` is been `shutdown()`, interrupting any outstanding
    /// `send_txs()` invocations.
    cancel: CancellationToken,
}

#[derive(Debug, Error, PartialEq)]
pub enum WorkersCacheError {
    /// typically happens when the client could not establish the connection.
    #[error("Work receiver has been dropped unexpectedly.")]
    ReceiverDropped,

    #[error("Task failed to join.")]
    TaskJoinFailure,

    #[error("The WorkersCache is being shutdown.")]
    ShutdownError,
}

impl WorkersCache {
    pub(crate) fn new(capacity: usize, cancel: CancellationToken) -> Self {
        Self {
            workers: LruCache::new(capacity),
            cancel,
        }
    }

    pub(crate) fn contains(&self, peer: &SocketAddr) -> bool {
        self.workers.contains(peer)
    }

    pub(crate) async fn push(
        &mut self,
        peer: SocketAddr,
        peer_worker: WorkerInfo,
    ) -> Option<ShutdownWorker> {
        if let Some((leader, popped_worker)) = self.workers.push(peer, peer_worker) {
            return Some(ShutdownWorker {
                leader,
                worker: popped_worker,
            });
        }
        None
    }

    /// Sends a batch of transactions to the worker for a given peer. If the
    /// worker for the peer is disconnected or fails, it is removed from the
    /// cache.
    pub(crate) async fn send_transactions_to_address(
        &mut self,
        peer: &SocketAddr,
        txs_batch: TransactionBatch,
    ) -> Result<(), WorkersCacheError> {
        let Self {
            workers, cancel, ..
        } = self;

        let body = async move {
            let current_worker = workers.get(peer).expect(
                "Failed to fetch worker for peer {peer}.\n\
             Peer existence must be checked before this call using `contains` method.",
            );
            let send_res = current_worker.send_transactions(txs_batch).await;

            if let Err(WorkersCacheError::ReceiverDropped) = send_res {
                // Remove the worker from the cache, if the peer has disconnected.
                if let Some(current_worker) = workers.pop(peer) {
                    // To avoid obscuring the error from send, ignore a possible
                    // `TaskJoinFailure`.
                    let close_result = current_worker.shutdown().await;
                    if let Err(error) = close_result {
                        error!("Error while closing worker: {error}.");
                    }
                }
            }

            send_res
        };

        tokio::select! {
            send_res = body => send_res,
            () = cancel.cancelled() => Err(WorkersCacheError::ShutdownError),
        }
    }

    /// Closes and removes all workers in the cache. This is typically done when
    /// shutting down the system.
    pub(crate) async fn shutdown(&mut self) {
        // Interrupt any outstanding `send_transactions()` calls.
        self.cancel.cancel();

        while let Some((leader, worker)) = self.workers.pop_lru() {
            let res = worker.shutdown().await;
            if let Err(err) = res {
                debug!("Error while shutting down worker for {leader}: {err}");
            }
        }
    }
}

/// [`ShutdownWorker`] takes care of stopping the worker. It's method
/// `shutdown()` should be executed in a separate task to hide the latency of
/// finishing worker gracefully.
pub(crate) struct ShutdownWorker {
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
