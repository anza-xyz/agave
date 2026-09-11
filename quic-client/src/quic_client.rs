//! Simple client that connects to a given UDP port with the QUIC protocol and provides
//! an interface for sending data which is restricted by the server's flow control.

use {
    crate::nonblocking::quic_client::{
        QuicClient, QuicClientConnection as NonblockingQuicConnection, QuicLazyInitializedEndpoint,
    },
    log::*,
    solana_connection_cache::{
        client_connection::{ClientConnection, ClientStats},
        connection_cache_stats::ConnectionCacheStats,
        nonblocking::client_connection::ClientConnection as NonblockingClientConnection,
    },
    solana_transaction_error::{TransportError, TransportResult},
    std::{
        net::SocketAddr,
        sync::{Arc, Condvar, Mutex, MutexGuard, atomic::Ordering},
        time::Duration,
    },
    tokio::{
        runtime::Runtime,
        sync::mpsc::{Receiver, Sender, error::TrySendError},
        time::timeout,
    },
};

pub const MAX_OUTSTANDING_TASK: u64 = 2000;
const SEND_DATA_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-peer send-queue depth. Packets beyond this (e.g. while a peer is
/// unreachable or its connection attempt is in flight) are dropped.
/// Sized to exceed the reasonable amount of votes we may send while
/// connection is getting set up.
const PEER_QUEUE_CAPACITY: usize = 1024;
/// How long a peer's driver task waits idle for the next packet before tearing
/// down and releasing its async-send permit. The cached connection itself is
/// unaffected, so a later send simply re-spawns a driver and reuses it.
const DRIVER_IDLE_TIMEOUT: Duration = Duration::from_secs(1);

/// A semaphore used for limiting the number of asynchronous tasks spawn to the
/// runtime. Before spawning a task, use acquire. After the task is done (be it
/// success or failure), call release.
struct AsyncTaskSemaphore {
    /// Keep the counter info about the usage
    counter: Mutex<u64>,
    /// Conditional variable for signaling when counter is decremented
    cond_var: Condvar,
    /// The maximum usage allowed by this semaphore.
    permits: u64,
}

impl AsyncTaskSemaphore {
    pub fn new(permits: u64) -> Self {
        Self {
            counter: Mutex::new(0),
            cond_var: Condvar::new(),
            permits,
        }
    }

    /// When returned, the lock has been locked and usage count has been
    /// incremented. When the returned MutexGuard is dropped the lock is dropped
    /// without decrementing the usage count.
    pub fn acquire(&self) -> MutexGuard<'_, u64> {
        let mut count = self.counter.lock().unwrap();
        *count += 1;
        while *count > self.permits {
            count = self.cond_var.wait(count).unwrap();
        }
        count
    }

    /// Increments semaphore and returns true if a permit was available, otherwise
    /// returns false. The caller is responsible for a matching `release` on success.
    pub fn try_acquire(&self) -> bool {
        let mut count = self.counter.lock().unwrap();
        if *count >= self.permits {
            return false;
        }
        *count += 1;
        true
    }

    /// Acquire the lock and decrement the usage count
    pub fn release(&self) {
        let mut count = self.counter.lock().unwrap();
        *count -= 1;
        self.cond_var.notify_one();
    }
}

static ASYNC_TASK_SEMAPHORE: std::sync::LazyLock<AsyncTaskSemaphore> =
    std::sync::LazyLock::new(|| AsyncTaskSemaphore::new(MAX_OUTSTANDING_TASK));
static RUNTIME: std::sync::LazyLock<Runtime> = std::sync::LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .thread_name("solQuicClientRt")
        .enable_all()
        .build()
        .unwrap()
});

pub fn get_runtime() -> &'static Runtime {
    &RUNTIME
}

/// Single long-lived task that drains one peer's send queue. Exactly one of
/// these exists per peer. It holds one async-send permit for its lifetime and
/// serves every queued packet over the cached connection. Because there is at most
/// one driver per peer, the number of outstanding permits is bounded by the
/// peer count rather than by send volume.
async fn drive_connection_for_peer(
    connection: Arc<NonblockingQuicConnection>,
    mut rx: Receiver<Arc<Vec<u8>>>,
) {
    while let Ok(Some(buffer)) = timeout(DRIVER_IDLE_TIMEOUT, rx.recv()).await {
        let result = timeout(SEND_DATA_TIMEOUT, connection.send_data(&buffer)).await;
        if handle_send_result(result, connection.clone()).is_err() {
            // Send/connect failed or timed out (e.g. unreachable peer).
            // Tear down and drop any still-queued packets; the next send
            // re-registers a driver and re-dials.
            break;
        }
    }

    // Unregister so the next caller spawns a fresh driver.
    connection.client.pkt_tx.store(None);
    ASYNC_TASK_SEMAPHORE.release();
}

async fn send_data_batch_async(
    connection: Arc<NonblockingQuicConnection>,
    buffers: Vec<Vec<u8>>,
) -> TransportResult<()> {
    let result = timeout(
        u32::try_from(buffers.len())
            .map(|size| SEND_DATA_TIMEOUT.saturating_mul(size))
            .unwrap_or(Duration::MAX),
        connection.send_data_batch(&buffers),
    )
    .await;
    ASYNC_TASK_SEMAPHORE.release();
    handle_send_result(result, connection)
}

/// Check the send result and update stats if timedout. Returns the checked result.
fn handle_send_result(
    result: Result<Result<(), TransportError>, tokio::time::error::Elapsed>,
    connection: Arc<NonblockingQuicConnection>,
) -> Result<(), TransportError> {
    match result {
        Ok(result) => result,
        Err(_err) => {
            let client_stats = ClientStats::default();
            client_stats.send_timeout.fetch_add(1, Ordering::Relaxed);
            let stats = connection.connection_stats();
            stats.add_client_stats(&client_stats, 0, false);
            info!("Timedout sending data {:?}", connection.server_addr());
            Err(TransportError::Custom("Timedout sending data".to_string()))
        }
    }
}

pub struct QuicClientConnection {
    pub inner: Arc<NonblockingQuicConnection>,
}

impl QuicClientConnection {
    pub fn new(
        endpoint: Arc<QuicLazyInitializedEndpoint>,
        server_addr: SocketAddr,
        connection_stats: Arc<ConnectionCacheStats>,
    ) -> Self {
        let inner = Arc::new(NonblockingQuicConnection::new(
            endpoint,
            server_addr,
            connection_stats,
        ));
        Self { inner }
    }

    pub fn new_with_client(
        client: Arc<QuicClient>,
        connection_stats: Arc<ConnectionCacheStats>,
    ) -> Self {
        let inner = Arc::new(NonblockingQuicConnection::new_with_client(
            client,
            connection_stats,
        ));
        Self { inner }
    }
}

impl ClientConnection for QuicClientConnection {
    fn server_addr(&self) -> &SocketAddr {
        self.inner.server_addr()
    }

    fn send_data_batch(&self, buffers: &[Vec<u8>]) -> TransportResult<()> {
        RUNTIME.block_on(self.inner.send_data_batch(buffers))?;
        Ok(())
    }

    fn send_data_async(&self, mut data: Arc<Vec<u8>>) -> TransportResult<()> {
        let client = &self.inner.client;
        loop {
            // Fast path: a driver is already running for this peer; just enqueue.
            // This never blocks and never touches the global send semaphore.
            if let Some(tx) = client.pkt_tx.load_full() {
                match tx.try_send(data) {
                    Ok(()) => return Ok(()),
                    Err(TrySendError::Full(_)) => {
                        // Queue saturated: drop rather than block the caller.
                        self.inner
                            .connection_stats
                            .send_backpressure_drops
                            .fetch_add(1, Ordering::Relaxed);
                        return Ok(());
                    }
                    // Driver just tore down; reclaim the packet and start a new one.
                    Err(TrySendError::Closed(d)) => data = d,
                }
            }
            // No active driver: try to become one
            let (tx, rx) = tokio::sync::mpsc::channel(PEER_QUEUE_CAPACITY);
            let tx = Arc::new(tx);
            if client
                .pkt_tx
                .compare_and_swap(&None::<Arc<Sender<Arc<Vec<u8>>>>>, Some(tx.clone()))
                .is_some()
            {
                // Lost the race; another caller just installed a driver. Retry
                // the loop to enqueue into theirs.
                continue;
            }
            // Won the race. Bound the number of concurrent driver tasks without
            // blocking the caller.
            if !ASYNC_TASK_SEMAPHORE.try_acquire() {
                client.pkt_tx.store(None);
                self.inner
                    .connection_stats
                    .send_backpressure_drops
                    .fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            // The channel is empty, so this enqueue always succeeds.
            let _ = tx.try_send(data);
            let _handle = RUNTIME.spawn(drive_connection_for_peer(self.inner.clone(), rx));
            return Ok(());
        }
    }

    fn send_data_batch_async(&self, buffers: Vec<Vec<u8>>) -> TransportResult<()> {
        let _lock = ASYNC_TASK_SEMAPHORE.acquire();
        let inner = self.inner.clone();
        let _handle = RUNTIME.spawn(send_data_batch_async(inner, buffers));
        Ok(())
    }

    fn send_data(&self, buffer: &[u8]) -> TransportResult<()> {
        RUNTIME.block_on(self.inner.send_data(buffer))?;
        Ok(())
    }
}

pub(crate) fn close_quic_connection(connection: Arc<QuicClient>) {
    // Close the connection and release resources
    trace!("Closing QUIC connection to {}", connection.server_addr());
    // Connection caches can be dropped by async callers, such as admin RPC set-identity;
    // blocking on RUNTIME from a Tokio worker would panic.
    if tokio::runtime::Handle::try_current().is_ok() {
        let _handle = RUNTIME.spawn(async move {
            connection.close().await;
        });
    } else {
        RUNTIME.block_on(connection.close());
    }
}
