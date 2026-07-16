//! Simple UDP client that communicates with the given UDP port with UDP and provides
//! an interface for sending data

use {
    async_trait::async_trait,
    solana_connection_cache::nonblocking::client_connection::ClientConnection,
    solana_transaction_error::TransportResult, std::net::SocketAddr,
};

pub struct UdpClientConnection {}

#[async_trait]
impl ClientConnection for UdpClientConnection {
    fn server_addr(&self) -> &SocketAddr {
        unreachable!()
    }

    async fn send_data(&self, _buffer: &[u8]) -> TransportResult<()> {
        unreachable!()
    }

    async fn send_data_batch(&self, _buffers: &[Vec<u8>]) -> TransportResult<()> {
        unreachable!()
    }
}
