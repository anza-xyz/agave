use {
    crate::kcp::{Kcp, KcpOutput},
    solana_connection_cache::client_connection::ClientConnection,
    solana_sdk::transport::Result as TransportResult,
    std::{
        io::{self, ErrorKind, Write},
        net::{SocketAddr, UdpSocket},
        sync::Arc,
    },
};

pub struct KcpClientConnection {
    kcp: Arc<Kcp<KcpOutput<UdpSocket>>>,
    addr: SocketAddr,
}

impl KcpClientConnection {
    pub fn new(socket: UdpSocket, server_addr: SocketAddr, conv: u32) -> Self {
        let kcp_output = KcpOutput(socket);
        let mut kcp = Kcp::new(conv, kcp_output);
        kcp.set_nodelay(true, 10, 2, true);
        kcp.set_wndsize(1024, 1024);
        kcp.set_mtu(1400);

        Self {
            kcp: Arc::new(kcp),
            addr: server_addr,
        }
    }

    fn send_kcp_packet(&self, buf: &[u8]) -> io::Result<usize> {
        let mut kcp = self.kcp.clone();
        kcp.send(buf)?;
        kcp.flush()?;
        Ok(buf.len())
    }
}

impl ClientConnection for KcpClientConnection {
    fn server_addr(&self) -> &SocketAddr {
        &self.addr
    }

    fn send_data(&self, data: &[u8]) -> TransportResult<()> {
        self.send_kcp_packet(data)?;
        Ok(())
    }

    fn send_data_batch(&self, buffers: &[Vec<u8>]) -> TransportResult<()> {
        for buffer in buffers {
            self.send_kcp_packet(buffer)?;
        }
        Ok(())
    }

    fn send_data_async(&self, data: Vec<u8>) -> TransportResult<()> {
        self.send_kcp_packet(&data)?;
        Ok(())
    }

    fn send_data_batch_async(&self, buffers: Vec<Vec<u8>>) -> TransportResult<()> {
        for buffer in buffers {
            self.send_kcp_packet(&buffer)?;
        }
        Ok(())
    }
}
