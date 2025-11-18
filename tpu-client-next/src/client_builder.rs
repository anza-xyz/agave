//! This module provides a [`ClientBuilder`] structure that builds [`TransactionSender`] and [`Client`].
//!
//! TPU client establishes connections to TPU nodes. To avoid recreating these connections every
//! leader window, it is desirable to cache them and orchestrate their usage which is implemented in
//! [`ConnectionWorkersScheduler`]. [`ClientBuilder`] hides the complexity of creating scheduler and
//! provides a simple but configurable way to create [`TransactionSender`] and [`Client`].
//! [`TransactionSender`] is used to send transactions in batches while [`Client`] runs the
//! background tasks.
//!
//! # Example
//!
//! ```ignore
//!  let builder = ClientBuilder::with_leader_updater(leader_updater)
//!        .cancel_token(cancel.child_token())
//!        .bind_addr(SocketAddr::new(
//!            IpAddr::V4(Ipv4Addr::LOCALHOST),
//!            0u16
//!        ))
//!        .leader_send_fanout(1)
//!        .identity(&identity_keypair)
//!        .max_cache_size(128);
//!        .metric_reporter({
//!            let successfully_sent = successfully_sent.clone();
//!            |stats: Arc<SendTransactionStats>, cancel: CancellationToken| async move {
//!                let mut interval = interval(Duration::from_millis(10));
//!                cancel
//!                    .run_until_cancelled(async {
//!                        loop {
//!                            interval.tick().await;
//!                            let view = stats.read_and_reset();
//!                            successfully_sent.fetch_add(view.successfully_sent, Ordering::Relaxed);
//!                        }
//!                    })
//!                    .await;
//!            }
//!        });
//!    let (transaction_sender, client) = builder
//!        .build::<NonblockingBroadcaster>()
//!        .expect("Client should be built successfully.");
//!    transaction_sender.send_transactions_in_batch(wire_transactions).await?;
//! ```
use {
    crate::{
        connection_workers_scheduler::{
            BindTarget, ConnectionWorkersSchedulerConfig, Fanout, StakeIdentity, WorkersBroadcaster,
        },
        leader_updater::LeaderUpdater,
        transaction_batch::TransactionBatch,
        ConnectionWorkersScheduler, ConnectionWorkersSchedulerError, SendTransactionStats,
    },
    solana_keypair::Keypair,
    std::{future::Future, net::UdpSocket, pin::Pin, sync::Arc},
    thiserror::Error,
    tokio::{
        runtime,
        sync::{mpsc, watch},
        task::{JoinError, JoinHandle},
    },
    tokio_util::sync::CancellationToken,
};

/// [`TransactionSender`] provides an interface to send transactions in batches.
#[derive(Clone)]
pub struct TransactionSender(mpsc::Sender<TransactionBatch>);

/// [`Client`] runs the background tasks required for sending transactions and update certificate
/// used the endpoint.
pub struct Client {
    update_certificate_sender: watch::Sender<Option<StakeIdentity>>,
    scheduler_handle:
        CancellableHandle<Result<Arc<SendTransactionStats>, ConnectionWorkersSchedulerError>>,
    reporter_handle: Option<CancellableHandle<()>>,
}

/// [`ClientBuilder`] is a builder structure to create [`TransactionSender`] along with [`Client`].
pub struct ClientBuilder {
    runtime_handle: Option<runtime::Handle>,
    leader_updater: Box<dyn LeaderUpdater>,
    bind_target: Option<BindTarget>,
    identity: Option<StakeIdentity>,
    num_connections: usize,
    leader_send_fanout: usize,
    skip_check_transaction_age: bool,
    sender_channel_size: usize,
    worker_channel_size: usize,
    max_reconnect_attempts: usize,
    report_fn: Option<ReportFn>,
    cancel_scheduler: CancellationToken,
    cancel_reporter: CancellationToken,
}

impl ClientBuilder {
    pub fn new(leader_updater: Box<dyn LeaderUpdater>) -> Self {
        Self {
            runtime_handle: None,
            leader_updater,
            bind_target: None,
            identity: None,
            num_connections: 64,
            leader_send_fanout: 2,
            skip_check_transaction_age: true,
            worker_channel_size: 2,
            sender_channel_size: 64,
            max_reconnect_attempts: 2,
            report_fn: None,
            cancel_scheduler: CancellationToken::new(),
            cancel_reporter: CancellationToken::new(),
        }
    }

    /// Set the runtime handle for the client. If not set, the current runtime will be used.
    ///
    /// Note that if the runtime handle is not set, the caller must ensure that the `build` is
    /// called in tokio runtime context. Otherwise, `build` will panic.
    pub fn runtime_handle(mut self, handle: runtime::Handle) -> Self {
        self.runtime_handle = Some(handle);
        self
    }

    /// Set the UDP socket to bind to.
    pub fn bind_socket(mut self, bind_socket: UdpSocket) -> Self {
        self.bind_target = Some(BindTarget::Socket(bind_socket));
        self
    }

    /// Set the leader send fanout.
    pub fn leader_send_fanout(mut self, fanout: usize) -> Self {
        self.leader_send_fanout = fanout;
        self
    }

    /// Set the identity keypair for the client.
    pub fn identity<'a>(mut self, identity: impl Into<Option<&'a Keypair>>) -> Self {
        self.identity = identity.into().map(StakeIdentity::new);
        self
    }

    /// Set the maximum number of cached connections.
    pub fn max_cache_size(mut self, num_connections: usize) -> Self {
        self.num_connections = num_connections;
        self
    }

    /// Set the cancellation token for the client.
    ///
    /// This token is used to create child tokens for the scheduler and reporter tasks. It is useful
    /// if user wants to immediately cancel all internal tasks, otherwise calling `Client::shutdown`
    /// is prefered way because it ensures orderly shutdown of internal tasks, see
    /// `Client::shutdown` for details.
    pub fn cancel_token(mut self, cancel: CancellationToken) -> Self {
        self.cancel_scheduler = cancel.child_token();
        self.cancel_reporter = cancel.child_token();
        self
    }

    /// Set the worker channel size.
    ///
    /// See [`ConnectionWorkersSchedulerConfig::worker_channel_size`] for details.
    pub fn worker_channel_size(mut self, size: usize) -> Self {
        self.worker_channel_size = size;
        self
    }

    /// Set the sender channel size.
    ///
    /// This defines the size of the channel used in [`TransactionSender`].
    pub fn sender_channel_size(mut self, size: usize) -> Self {
        self.sender_channel_size = size;
        self
    }

    /// Set the maximum number of reconnect attempts when connection has failed.
    pub fn max_reconnect_attempts(mut self, attempts: usize) -> Self {
        self.max_reconnect_attempts = attempts;
        self
    }

    /// Set the reporting function which runs in the background to report metrics.
    pub fn metric_reporter<F, Fut>(mut self, f: F) -> Self
    where
        F: FnOnce(Arc<SendTransactionStats>, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.report_fn = Some(Box::new(move |stats, cancel| Box::pin(f(stats, cancel))));
        self
    }

    /// Build the [`TransactionSender`] and [`Client`] using the provided configuration.
    pub fn build<Broadcaster>(self) -> Result<(TransactionSender, Client), ClientBuilderError>
    where
        Broadcaster: WorkersBroadcaster + 'static,
    {
        let bind = self.bind_target.ok_or(ClientBuilderError::Misconfigured)?;
        let (sender, receiver) = mpsc::channel(self.sender_channel_size);

        let (update_certificate_sender, update_certificate_receiver) = watch::channel(None);

        let config = ConnectionWorkersSchedulerConfig {
            bind,
            stake_identity: self.identity,
            num_connections: self.num_connections,
            skip_check_transaction_age: self.skip_check_transaction_age,
            worker_channel_size: self.worker_channel_size,
            max_reconnect_attempts: self.max_reconnect_attempts,
            // We open connection to one more leader in advance, which time-wise means ~1.6s
            leaders_fanout: Fanout {
                connect: self.leader_send_fanout.saturating_add(1),
                send: self.leader_send_fanout,
            },
        };

        let scheduler = ConnectionWorkersScheduler::new(
            self.leader_updater,
            receiver,
            update_certificate_receiver,
            self.cancel_scheduler.clone(),
        );
        let runtime_handle = self
            .runtime_handle
            .unwrap_or_else(tokio::runtime::Handle::current);
        let reporter_handle = if let Some(report_fn) = self.report_fn {
            let stats = scheduler.get_stats();
            let cancel = self.cancel_reporter.clone();
            let handle = runtime_handle.spawn(report_fn(stats, self.cancel_reporter));
            Some(CancellableHandle { handle, cancel })
        } else {
            None
        };
        let scheduler_handle =
            runtime_handle.spawn(scheduler.run_with_broadcaster::<Broadcaster>(config));
        let client = Client {
            update_certificate_sender,
            scheduler_handle: CancellableHandle {
                handle: scheduler_handle,
                cancel: self.cancel_scheduler,
            },
            reporter_handle,
        };
        Ok((TransactionSender(sender), client))
    }
}

pub type ReportFn = Box<
    dyn FnOnce(
            Arc<SendTransactionStats>,
            CancellationToken,
        ) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

impl TransactionSender {
    pub async fn send_transactions_in_batch<T>(
        &self,
        wire_transactions: Vec<T>,
    ) -> Result<(), ClientError>
    where
        T: AsRef<[u8]> + Send + 'static,
    {
        self.0
            .send(TransactionBatch::new(wire_transactions))
            .await
            .map_err(ClientError::SendError)
    }

    pub fn try_send_transactions_in_batch<T>(
        &self,
        wire_transactions: Vec<T>,
    ) -> Result<(), ClientError>
    where
        T: AsRef<[u8]> + Send + 'static,
    {
        self.0
            .try_send(TransactionBatch::new(wire_transactions))
            .map_err(ClientError::TrySendError)
    }
}

impl Client {
    pub fn update_identity(&self, identity: &Keypair) -> Result<(), ClientError> {
        let stake_identity = StakeIdentity::new(identity);
        self.update_certificate_sender
            .send(Some(stake_identity))
            .map_err(|_| ClientError::FailedToUpdateIdentity)
    }

    /// Shutdown the client and all its internal tasks in an orderly manner.
    ///
    /// Note that the token provided by user in `Builder` is not cancelled, only internal child
    /// tokens are cancelled. If user, instead, calls cancel on the provided token directly, the
    /// order of internal tasks shutdown is not guaranteed, which means that it might happen that
    /// some metrics are not reported. This might metter for the test code.
    pub async fn shutdown(self) -> Result<(), ClientError> {
        self.scheduler_handle.shutdown().await??;
        if let Some(reporter_handle) = self.reporter_handle {
            reporter_handle.shutdown().await?;
        }
        drop(self.update_certificate_sender);
        Ok(())
    }
}

/// Represents [`ClientBuilder`] errors.
#[derive(Debug, Error)]
pub enum ClientBuilderError {
    /// Error during building client.
    #[error("ClientBuilder is misconfigured.")]
    Misconfigured,
}

/// Represents [`Client`] errors.
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("Failed to update identity.")]
    FailedToUpdateIdentity,

    #[error(transparent)]
    JoinError(#[from] JoinError),

    #[error(transparent)]
    ConnectionWorkersSchedulerError(#[from] ConnectionWorkersSchedulerError),

    #[error(transparent)]
    SendError(#[from] mpsc::error::SendError<TransactionBatch>),

    #[error(transparent)]
    TrySendError(#[from] mpsc::error::TrySendError<TransactionBatch>),
}

/// Helper structure for graceful shutdown of spawned tasks.
struct CancellableHandle<T> {
    handle: JoinHandle<T>,
    cancel: CancellationToken,
}

impl<T> CancellableHandle<T> {
    pub async fn shutdown(self) -> Result<T, JoinError> {
        self.cancel.cancel();
        self.handle.await
    }
}
