//! This module defines `WorkersCache` along with aux struct `WorkerInfo`. These
//! structures provide mechanisms for caching workers, sending transaction
//! batches, and gathering send transaction statistics.
use {
    super::SendTransactionStats,
    crate::transaction_batch::TransactionBatch,
    log::*,
    lru::LruCache,
    std::{
        collections::HashMap,
        net::{IpAddr, SocketAddr},
    },
    thiserror::Error,
    tokio::{sync::mpsc, task::JoinHandle},
};

/// [`WorkerInfo`] holds information about a worker responsible for sending
/// transaction batches.
pub(crate) struct WorkerInfo {
    pub sender: mpsc::Sender<TransactionBatch>,
    pub handle: JoinHandle<SendTransactionStats>,
}

impl WorkerInfo {
    pub fn new(
        sender: mpsc::Sender<TransactionBatch>,
        handle: JoinHandle<SendTransactionStats>,
    ) -> Self {
        Self { sender, handle }
    }

    async fn queue_txs(&self, txs_batch: TransactionBatch) -> Result<(), WorkersCacheError> {
        self.sender
            .send(txs_batch)
            .await
            .map_err(|_| WorkersCacheError::ReceiverDropped)?;
        Ok(())
    }

    /// Closes the worker by dropping the sender and awaiting the worker's statistics.
    async fn close(self) -> Result<SendTransactionStats, WorkersCacheError> {
        drop(self.sender);
        let stats = self
            .handle
            .await
            .map_err(|_| WorkersCacheError::TaskJoinFailure)?;
        Ok(stats)
    }
}

/// [`WorkersCache`] manages and caches workers. It uses an LRU cache to store and
/// manage workers. It also tracks transaction statistics for each peer.
pub(crate) struct WorkersCache {
    workers: LruCache<SocketAddr, WorkerInfo>,
    send_stats_per_addr: HashMap<IpAddr, SendTransactionStats>,
}

#[derive(Debug, Error, PartialEq)]
pub enum WorkersCacheError {
    /// typically happens when the client could not establish the connection.
    #[error("Work receiver has been dropped unexpectedly.")]
    ReceiverDropped,

    #[error("Task failed to join.")]
    TaskJoinFailure,
}

impl WorkersCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            workers: LruCache::new(capacity),
            send_stats_per_addr: HashMap::new(),
        }
    }

    pub fn contains(&self, peer: &SocketAddr) -> bool {
        self.workers.contains(peer)
    }

    pub async fn push(&mut self, peer: SocketAddr, peer_worker: WorkerInfo) {
        // Although there might be concerns about the performance implications
        // of waiting for the worker to be closed when trying to add a new
        // worker, the idea is that these workers are almost always created in
        // advance so the latency is hidden.
        if let Some((leader, popped_worker)) = self.workers.push(peer, peer_worker) {
            self.shutdown_worker(leader, popped_worker).await;
        }
    }

    /// Sends a batch of transactions to the worker for a given peer. If the
    /// worker for the peer is disconnected or fails, it is removed from the
    /// cache.
    pub async fn send_txs_to_worker(
        &mut self,
        peer: &SocketAddr,
        txs_batch: TransactionBatch,
    ) -> Result<(), WorkersCacheError> {
        let current_worker = self.workers.get(peer).expect(
            "Failed to fetch worker for peer {peer}.\n\
             Peer existence must be checked before this call using `contains` method.",
        );
        let send_res = current_worker.queue_txs(txs_batch).await;
        if let Err(WorkersCacheError::ReceiverDropped) = send_res {
            // Remove the worker from the cache, if the peer has disconnected.
            if let Some(current_worker) = self.workers.pop(peer) {
                // to avoid obscuring the error from send, ignore possible
                // TaskJoinFailure
                let close_result = current_worker.close().await;
                if let Err(error) = close_result {
                    error!("Error while closing worker: {error}.");
                }
            }
        }
        send_res
    }

    pub fn get_transaction_stats(&self) -> &HashMap<IpAddr, SendTransactionStats> {
        &self.send_stats_per_addr
    }

    /// Closes and removes all workers in the cache. This is typically done when
    /// shutting down the system.
    pub async fn close(&mut self) {
        let leaders: Vec<SocketAddr> = self.workers.iter().map(|(key, _value)| *key).collect();
        for leader in leaders {
            if let Some(worker) = self.workers.pop(&leader) {
                self.shutdown_worker(leader, worker).await;
            }
        }
    }

    /// Shuts down a worker for a given peer by closing the worker and gathering
    /// its transaction statistics.
    async fn shutdown_worker(&mut self, leader: SocketAddr, worker: WorkerInfo) {
        let worker_stats = worker.close().await;

        match worker_stats {
            Ok(worker_stats) => {
                self.send_stats_per_addr
                    .entry(leader.ip())
                    .and_modify(|e| e.add(&worker_stats))
                    .or_insert(worker_stats);
            }
            Err(err) => debug!("Error while shutting down worker for {leader}: {err}"),
        }
    }
}
