pub use solana_tpu_client_next::node_address_service::LeaderTpuCacheServiceConfig;
use {
    solana_keypair::Keypair,
    solana_net_utils::bind_to_unspecified,
    solana_pubsub_client::nonblocking::pubsub_client::PubsubClientError,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::client_error::Error as ClientError,
    solana_signer::SignerError,
    solana_tpu_client_next::{
        Client, ClientBuilder, TransactionSender,
        connection_workers_scheduler::NonblockingBroadcaster,
        websocket_node_address_service::{self, WebsocketNodeAddressService},
    },
    std::sync::Arc,
    thiserror::Error,
    tokio_util::sync::{CancellationToken, DropGuard},
};

#[derive(Error, Debug)]
pub enum TpuSenderError {
    #[error("Pubsub error: {0:?}")]
    PubsubError(#[from] PubsubClientError),
    #[error("RPC error: {0:?}")]
    RpcError(#[from] ClientError),
    #[error("IO error: {0:?}")]
    IoError(#[from] std::io::Error),
    #[error("Signer error: {0:?}")]
    SignerError(#[from] SignerError),
    #[error("Custom error: {0}")]
    Custom(String),
}

pub type Result<T> = std::result::Result<T, TpuSenderError>;

impl From<websocket_node_address_service::Error> for TpuSenderError {
    fn from(err: websocket_node_address_service::Error) -> Self {
        match err {
            websocket_node_address_service::Error::RpcError(err) => Self::RpcError(err),
            websocket_node_address_service::Error::PubsubError(err) => Self::PubsubError(err),
            err => Self::Custom(err.to_string()),
        }
    }
}

impl From<solana_tpu_client_next::ClientError> for TpuSenderError {
    fn from(err: solana_tpu_client_next::ClientError) -> Self {
        Self::Custom(err.to_string())
    }
}

#[derive(Clone, Debug, Default)]
pub struct TpuClientConfig {
    /// Number of upcoming leaders each transaction is sent to. `None` keeps the
    /// tpu-client-next default.
    pub leader_send_fanout: Option<usize>,
    /// Configuration of the leader schedule / TPU address tracking.
    pub leader_cache: LeaderTpuCacheServiceConfig,
}

/// Client which sends transactions directly to the upcoming leaders' TPU over QUIC.
///
/// Leaders are tracked through RPC and websocket slot updates. Sending is
/// fire-and-forget: use [`send_and_confirm_transactions_in_parallel_v3`] with
/// `SendTransport::Tpu(tpu_client.transaction_sender())` to confirm landing.
///
/// Call [`TpuClient::shutdown`] for an orderly shutdown; dropping the client
/// cancels all background tasks.
///
/// [`send_and_confirm_transactions_in_parallel_v3`]: crate::send_and_confirm_transactions_in_parallel::send_and_confirm_transactions_in_parallel_v3
pub struct TpuClient {
    sender: TransactionSender,
    client: Client,
    node_address_service: WebsocketNodeAddressService,
    _cancel_on_drop: DropGuard,
}

impl TpuClient {
    /// Creates a new client. Must be called within a tokio runtime.
    pub async fn new(
        rpc_client: Arc<RpcClient>,
        websocket_url: String,
        config: TpuClientConfig,
    ) -> Result<Self> {
        let cancel = CancellationToken::new();
        let (provider, node_address_service) = WebsocketNodeAddressService::run(
            rpc_client,
            websocket_url,
            config.leader_cache,
            cancel.child_token(),
        )
        .await?;

        let mut builder = ClientBuilder::new(Box::new(provider))
            .cancel_token(cancel.clone())
            .bind_socket(bind_to_unspecified()?)
            .broadcaster(NonblockingBroadcaster);
        if let Some(fanout) = config.leader_send_fanout {
            builder = builder.leader_send_fanout(fanout);
        }
        let (sender, client) = builder
            .build()
            .map_err(|err| TpuSenderError::Custom(err.to_string()))?;

        Ok(Self {
            sender,
            client,
            node_address_service,
            _cancel_on_drop: cancel.drop_guard(),
        })
    }

    /// Returns a handle for enqueueing transactions, e.g. for
    /// `SendTransport::Tpu`. It stops accepting transactions once the client is
    /// shut down or dropped.
    pub fn transaction_sender(&self) -> TransactionSender {
        self.sender.clone()
    }

    /// Enqueues a serialized transaction for sending to the upcoming leaders.
    ///
    /// Waits for channel capacity. Success means the transaction was enqueued,
    /// not that it was delivered.
    pub async fn send_wire_transaction(&self, wire_transaction: Vec<u8>) -> Result<()> {
        self.sender.send_transaction(wire_transaction).await?;
        Ok(())
    }

    /// Updates the identity used for QUIC connections, e.g. to get stake-weighted QoS.
    pub fn update_identity(&self, identity: &Keypair) -> Result<()> {
        self.client.update_identity(identity)?;
        Ok(())
    }

    /// Shuts down the client and its background tasks in an orderly manner.
    pub async fn shutdown(self) -> Result<()> {
        let Self {
            sender,
            client,
            node_address_service,
            _cancel_on_drop,
        } = self;
        drop(sender);
        let client_result = client.shutdown().await;
        let service_result = node_address_service.shutdown().await;
        client_result?;
        service_result?;
        Ok(())
    }
}
