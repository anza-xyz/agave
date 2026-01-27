//! Simple UDP client that communicates with the given UDP port with UDP and provides
//! an interface for sending data

use {
    async_trait::async_trait, core::iter::repeat,
    solana_connection_cache::nonblocking::client_connection::ClientConnection,
    solana_streamer::nonblocking::sendmmsg::batch_send, solana_transaction_error::TransportResult,
    std::net::SocketAddr, tokio::net::UdpSocket,
};

pub struct UdpClientConnection {
    pub socket: UdpSocket,
    pub addr: SocketAddr,
}

impl UdpClientConnection {
    pub fn new_from_addr(socket: std::net::UdpSocket, server_addr: SocketAddr) -> Self {
        socket.set_nonblocking(true).unwrap();
        let socket = UdpSocket::from_std(socket).unwrap();
        Self {
            socket,
            addr: server_addr,
        }
    }
}

#[async_trait]
impl ClientConnection for UdpClientConnection {
    fn server_addr(&self) -> &SocketAddr {
        &self.addr
    }

    async fn send_data(&self, buffer: &[u8]) -> TransportResult<()> {
        self.socket.send_to(buffer, self.addr).await?;
        Ok(())
    }

    async fn send_data_batch(&self, buffers: &[Vec<u8>]) -> TransportResult<()> {
        let pkts: Vec<_> = buffers.iter().zip(repeat(self.server_addr())).collect();
        batch_send(&self.socket, &pkts).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_net_utils::sockets::{
            SocketConfiguration as SocketConfig, bind_to_async, bind_to_with_config,
            unique_port_range_for_tests,
        },
        solana_packet::{PACKET_DATA_SIZE, Packet},
        solana_streamer::nonblocking::recvmmsg::recv_mmsg,
        std::net::{IpAddr, Ipv4Addr},
        tokio::net::UdpSocket,
    };

    async fn check_send_one(connection: &UdpClientConnection, reader: &UdpSocket) {
        let packet = vec![111u8; PACKET_DATA_SIZE];
        connection.send_data(&packet).await.unwrap();
        let mut packets = vec![Packet::default(); 32];
        let recv = recv_mmsg(reader, &mut packets[..]).await.unwrap();
        assert_eq!(1, recv);
    }

    async fn check_send_batch(connection: &UdpClientConnection, reader: &UdpSocket) {
        let packets: Vec<_> = (0..32).map(|_| vec![0u8; PACKET_DATA_SIZE]).collect();
        connection.send_data_batch(&packets).await.unwrap();
        let mut packets = vec![Packet::default(); 32];
        let recv = recv_mmsg(reader, &mut packets[..]).await.unwrap();
        assert_eq!(32, recv);
    }

    #[tokio::test]
    async fn test_send_from_addr() {
        let mut port_range = unique_port_range_for_tests(4);
        let socket = bind_to_with_config(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port_range.next().unwrap(),
            SocketConfig::default(),
        )
        .unwrap();

        let reader_ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let reader_port = port_range.next().unwrap();
        let connection =
            UdpClientConnection::new_from_addr(socket, SocketAddr::new(reader_ip, reader_port));

        let reader = bind_to_async(reader_ip, reader_port).await.expect("bind");
        check_send_one(&connection, &reader).await;
        check_send_batch(&connection, &reader).await;
    }
}
