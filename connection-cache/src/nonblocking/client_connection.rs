//! Trait defining async send functions, to be used for UDP or QUIC sending

use {
    async_trait::async_trait,
    solana_transaction_error::TransportResult,
    std::{net::SocketAddr, sync::Arc},
};

#[async_trait]
pub trait ClientConnection {
    fn server_addr(&self) -> &SocketAddr;

    async fn send_data(&self, buffer: &[u8]) -> TransportResult<()>;

    async fn send_data_batch(&self, buffers: &[Arc<Vec<u8>>]) -> TransportResult<()>;
}
