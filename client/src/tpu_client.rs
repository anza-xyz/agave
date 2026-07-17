pub use solana_tpu_client::{
    nonblocking::tpu_client::TpuSenderError,
    tpu_client::{DEFAULT_FANOUT_SLOTS, MAX_FANOUT_SLOTS, TpuClientConfig},
};
use {
    solana_connection_cache::connection_cache::{
        ConnectionCache as BackendConnectionCache, ConnectionManager, ConnectionPool,
        NewConnectionConfig,
    },
    solana_message::Message,
    solana_quic_client::{QuicConfig, QuicConnectionManager, QuicPool},
    solana_rpc_client::rpc_client::RpcClient,
    solana_signer::signers::Signers,
    solana_tpu_client::tpu_client::{Result, TpuClient as BackendTpuClient},
    solana_transaction::{Transaction, versioned::VersionedTransaction},
    solana_transaction_error::{TransactionError, TransportResult},
    solana_udp_client::{UdpConfig, UdpConnectionManager, UdpPool},
    std::sync::Arc,
};

pub enum TpuClientWrapper {
    Quic(BackendTpuClient<QuicPool, QuicConnectionManager, QuicConfig>),
    Udp(BackendTpuClient<UdpPool, UdpConnectionManager, UdpConfig>),
}

/// Client which sends transactions directly to the current leader's TPU port over UDP.
/// The client uses RPC to determine the current leader and fetch node contact info
/// This is just a thin wrapper over the "BackendTpuClient", use that directly for more efficiency.
pub struct TpuClient<
    P, // ConnectionPool
    M, // ConnectionManager
    C, // NewConnectionConfig
> {
    tpu_client: BackendTpuClient<P, M, C>,
}

impl<P, M, C> TpuClient<P, M, C>
where
    P: ConnectionPool<NewConnectionConfig = C>,
    M: ConnectionManager<ConnectionPool = P, NewConnectionConfig = C>,
    C: NewConnectionConfig,
{
    /// Serialize and send transaction to the current and upcoming leader TPUs according to fanout
    /// size
    pub fn send_transaction(&self, transaction: &Transaction) -> bool {
        self.tpu_client.send_transaction(transaction)
    }

    /// Send a wire transaction to the current and upcoming leader TPUs according to fanout size
    pub fn send_wire_transaction(&self, wire_transaction: Vec<u8>) -> bool {
        self.tpu_client.send_wire_transaction(wire_transaction)
    }

    /// Serialize and send transaction to the current and upcoming leader TPUs according to fanout
    /// size
    /// Returns the last error if all sends fail
    pub fn try_send_transaction(&self, transaction: &VersionedTransaction) -> TransportResult<()> {
        self.tpu_client.try_send_transaction(transaction)
    }

    /// Serialize and send a batch of transactions to the current and upcoming leader TPUs according
    /// to fanout size
    /// Returns the last error if all sends fail
    pub fn try_send_transaction_batch(
        &self,
        transactions: &[VersionedTransaction],
    ) -> TransportResult<()> {
        self.tpu_client.try_send_transaction_batch(transactions)
    }

    /// Send a wire transaction to the current and upcoming leader TPUs according to fanout size
    /// Returns the last error if all sends fail
    pub fn try_send_wire_transaction(&self, wire_transaction: Vec<u8>) -> TransportResult<()> {
        self.tpu_client.try_send_wire_transaction(wire_transaction)
    }
}

impl TpuClient<QuicPool, QuicConnectionManager, QuicConfig> {
    /// Create a new client that disconnects when dropped
    pub fn new(
        rpc_client: Arc<RpcClient>,
        websocket_url: &str,
        config: TpuClientConfig,
    ) -> Result<Self> {
        let connection_manager = QuicConnectionManager::new_with_connection_config(
            QuicConfig::new().expect("QUIC client config must be constructible"),
        );
        Ok(Self {
            tpu_client: BackendTpuClient::new(
                "connection_cache_tpu_client",
                rpc_client,
                websocket_url,
                config,
                connection_manager,
            )?,
        })
    }
}

impl<P, M, C> TpuClient<P, M, C>
where
    P: ConnectionPool<NewConnectionConfig = C>,
    M: ConnectionManager<ConnectionPool = P, NewConnectionConfig = C>,
    C: NewConnectionConfig,
{
    /// Create a new client that disconnects when dropped
    pub fn new_with_connection_cache(
        rpc_client: Arc<RpcClient>,
        websocket_url: &str,
        config: TpuClientConfig,
        connection_cache: Arc<BackendConnectionCache<P, M, C>>,
    ) -> Result<Self> {
        Ok(Self {
            tpu_client: BackendTpuClient::new_with_connection_cache(
                rpc_client,
                websocket_url,
                config,
                connection_cache,
            )?,
        })
    }
    #[deprecated(
        since = "4.3.0",
        note = "prefer send_and_confirm_transactions_in_parallel_v3"
    )]
    pub fn send_and_confirm_messages_with_spinner<T: Signers + ?Sized>(
        &self,
        messages: &[Message],
        signers: &T,
    ) -> Result<Vec<Option<TransactionError>>> {
        #[allow(deprecated)]
        self.tpu_client
            .send_and_confirm_messages_with_spinner(messages, signers)
    }

    pub fn rpc_client(&self) -> &RpcClient {
        self.tpu_client.rpc_client()
    }
}
