//! Client implementation for sending transactions to TPU nodes.
//!
//! Running TPU client requires the caller to establish connections to TPU
//! nodes. To avoid recreating these connections every leader window, it is
//! desirable to cache them. [`Client`] provides this functionality while
//! [`ClientBuilder`] helps configuring it.
//!
//! # Example
//!
//! ```rust, no_run
//!  let builder = ClientBuilder::with_leader_updater(leader_updater)
//!        .cancel_token(cancel.child_token())
//!        .bind_addr(SocketAddr::new(
//!            IpAddr::V4(Ipv4Addr::LOCALHOST),
//!            0u16
//!        ))
//!        .leader_send_fanout(2)
//!        .identity(&identity_keypair)
//!        .max_cache_size(128);
//!        .report({
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
//!    let client = builder
//!        .build::<NonblockingBroadcaster>()
//!        .await
//!        .expect("Client should be built successfully.");
//! ```
use {
    crate::{
        connection_workers_scheduler::{
            BindTarget, ConnectionWorkersSchedulerConfig, Fanout, StakeIdentity, WorkersBroadcaster,
        },
        leader_updater::LeaderUpdater,
        transaction_batch::TransactionBatch,
        ConnectionWorkersScheduler, SendTransactionStats,
    },
    solana_keypair::Keypair,
    std::{
        future::Future,
        net::{SocketAddr, UdpSocket},
        pin::Pin,
        sync::Arc,
    },
    thiserror::Error,
    tokio::sync::{mpsc, watch},
    tokio_util::{sync::CancellationToken, task::TaskTracker},
};

/// TODO(klykov) Documentation to be added when we agree on general API.
#[derive(Clone)]
pub struct Client {
    sender: mpsc::Sender<TransactionBatch>,
    update_certificate_sender: watch::Sender<Option<StakeIdentity>>,
    tasks: TaskTracker,
    cancel: CancellationToken,
}

pub struct ClientBuilder {
    leader_updater: Box<dyn LeaderUpdater>,
    bind_target: Option<BindTarget>,
    identity: Option<StakeIdentity>,
    num_connections: usize,
    leader_send_fanout: usize,
    skip_check_transaction_age: bool,
    input_channel_size: usize,
    worker_channel_size: usize,
    max_reconnect_attempts: usize,
    report_fn: Option<ReportFn>,
    cancel: CancellationToken,
}

impl ClientBuilder {
    pub fn with_leader_updater(leader_updater: Box<dyn LeaderUpdater>) -> Self {
        Self {
            leader_updater,
            bind_target: None,
            identity: None,
            num_connections: 64,
            leader_send_fanout: 2,
            skip_check_transaction_age: true,
            worker_channel_size: 2,
            input_channel_size: 64,
            max_reconnect_attempts: 2,
            report_fn: None,
            cancel: CancellationToken::new(),
        }
    }

    pub fn bind_socket(mut self, bind_socket: UdpSocket) -> Self {
        self.bind_target = Some(BindTarget::Socket(bind_socket));
        self
    }

    pub fn bind_addr(mut self, bind_addr: SocketAddr) -> Self {
        self.bind_target = Some(BindTarget::Address(bind_addr));
        self
    }

    pub fn leader_send_fanout(mut self, fanout: usize) -> Self {
        self.leader_send_fanout = fanout;
        self
    }

    pub fn identity<'a>(mut self, identity: impl Into<Option<&'a Keypair>>) -> Self {
        self.identity = identity.into().map(StakeIdentity::new);
        self
    }

    pub fn max_cache_size(mut self, num_connections: usize) -> Self {
        self.num_connections = num_connections;
        self
    }

    pub fn cancel_token(mut self, cancel: CancellationToken) -> Self {
        self.cancel = cancel;
        self
    }

    pub fn worker_channel_size(mut self, size: usize) -> Self {
        self.worker_channel_size = size;
        self
    }

    pub fn report<F, Fut>(mut self, f: F) -> Self
    where
        F: FnOnce(Arc<SendTransactionStats>, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.report_fn = Some(Box::new(move |stats, cancel| Box::pin(f(stats, cancel))));
        self
    }

    /// TODO(klykov): API-wise, it is also possible to split the result into Sender (to
    /// send txs) and Client which will be background task running the
    /// scheduler. Not sure if we need this flexibility.
    pub async fn build<Broadcaster>(self) -> Result<Client, ClientBuilderError>
    where
        Broadcaster: WorkersBroadcaster + 'static,
    {
        let bind = self.bind_target.ok_or(ClientBuilderError::Misconfigured)?;
        let (sender, receiver) = mpsc::channel(self.input_channel_size);

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
                connect: self.leader_send_fanout + 1,
                send: self.leader_send_fanout,
            },
        };

        let scheduler = ConnectionWorkersScheduler::new(
            self.leader_updater,
            receiver,
            update_certificate_receiver,
            self.cancel.clone(),
        );
        let tasks = TaskTracker::new();
        if let Some(report_fn) = self.report_fn {
            let stats = scheduler.get_stats();
            let cancel = self.cancel.clone();
            tasks.spawn(report_fn(stats, cancel));
        }
        tasks.spawn(scheduler.run_with_broadcaster::<Broadcaster>(config));
        tasks.close();
        Ok(Client {
            sender,
            update_certificate_sender,
            tasks,
            cancel: self.cancel,
        })
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

impl Client {
    pub async fn send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
    ) -> Result<(), mpsc::error::SendError<TransactionBatch>> {
        self.sender
            .send(TransactionBatch::new(wire_transactions))
            .await
    }

    pub fn try_send_transactions_in_batch(
        &self,
        wire_transactions: Vec<Vec<u8>>,
    ) -> Result<(), mpsc::error::TrySendError<TransactionBatch>> {
        self.sender
            .try_send(TransactionBatch::new(wire_transactions))
    }

    pub fn update_identity(&self, identity: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        let stake_identity = StakeIdentity::new(identity);
        self.update_certificate_sender
            .send(Some(stake_identity))
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
    }

    pub async fn shutdown(self) {
        self.cancel.cancel();
        drop(self.sender);
        drop(self.update_certificate_sender);
        self.tasks.wait().await;
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
    /// Error during building client.
    #[error("ClientBuilder is misconfigured.")]
    Misconfigured,
}
