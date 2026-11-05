pub use solana_connection_cache::connection_cache::{DEFAULT_MAX_CONNECTIONS, Protocol};
use {
    solana_connection_cache::{
        client_connection::ClientConnection,
        connection_cache::{
            BaseClientConnection, ConnectionCache as BackendConnectionCache, ConnectionPool,
            NewConnectionConfig,
        },
    },
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_quic_client::{QuicConfig, QuicConnectionManager, QuicPool},
    solana_streamer::streamer::StakedNodes,
    solana_tls_utils::NotifyKeyUpdate,
    solana_transaction_error::TransportResult,
    std::{
        net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
        sync::{Arc, RwLock},
    },
};

const DEFAULT_CONNECTION_POOL_SIZE: usize = 4;

/// A thin wrapper over connection-cache/ConnectionCache to ease
/// construction of the ConnectionCache for code dealing both with udp and quic.
/// For the scenario only using udp or quic, use connection-cache/ConnectionCache directly.
pub enum ConnectionCache {
    Quic(Arc<BackendConnectionCache<QuicPool, QuicConnectionManager, QuicConfig>>),
    Udp,
}

type QuicBaseClientConnection = <QuicPool as ConnectionPool>::BaseClientConnection;

pub enum BlockingClientConnection {
    Quic(Arc<<QuicBaseClientConnection as BaseClientConnection>::BlockingClientConnection>),
    Udp,
}

pub enum NonblockingClientConnection {
    Quic(Arc<<QuicBaseClientConnection as BaseClientConnection>::NonblockingClientConnection>),
    Udp,
}

impl NotifyKeyUpdate for ConnectionCache {
    fn update_key(&self, key: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        match self {
            Self::Udp => unreachable!(),
            Self::Quic(backend) => backend.update_key(key),
        }
    }
}

impl ConnectionCache {
    pub fn new(name: &'static str) -> Self {
        let cert_info = (&Keypair::new(), IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)));
        ConnectionCache::new_with_client_options(
            name,
            DEFAULT_CONNECTION_POOL_SIZE,
            None, // client_endpoint
            Some(cert_info),
            None, // stake_info
        )
    }

    /// Create a quic connection_cache
    pub fn new_quic(name: &'static str, connection_pool_size: usize) -> Self {
        Self::new_with_client_options(name, connection_pool_size, None, None, None)
    }

    /// Create a quic connection_cache with more client options
    pub fn new_with_client_options(
        name: &'static str,
        connection_pool_size: usize,
        client_socket: Option<UdpSocket>,
        cert_info: Option<(&Keypair, IpAddr)>,
        stake_info: Option<(&Arc<RwLock<StakedNodes>>, &Pubkey)>,
    ) -> Self {
        Self::new_with_max_connections(
            name,
            connection_pool_size,
            DEFAULT_MAX_CONNECTIONS,
            client_socket,
            cert_info,
            stake_info,
        )
    }

    /// Create a quic connection_cache with configurable max connections
    pub fn new_with_max_connections(
        name: &'static str,
        connection_pool_size: usize,
        max_connections: usize,
        client_socket: Option<UdpSocket>,
        cert_info: Option<(&Keypair, IpAddr)>,
        stake_info: Option<(&Arc<RwLock<StakedNodes>>, &Pubkey)>,
    ) -> Self {
        // The minimum pool size is 1.
        let connection_pool_size = 1.max(connection_pool_size);
        let mut config = QuicConfig::new().unwrap();
        if let Some(cert_info) = cert_info {
            config.update_client_certificate(cert_info.0, cert_info.1);
        }
        if let Some(client_socket) = client_socket {
            config.update_client_endpoint(client_socket);
        }
        if let Some(stake_info) = stake_info {
            config.set_staked_nodes(stake_info.0, stake_info.1);
        }
        let connection_manager = QuicConnectionManager::new_with_connection_config(config);
        let cache = BackendConnectionCache::new_with_max_connections(
            name,
            connection_manager,
            connection_pool_size,
            max_connections,
        )
        .unwrap();
        Self::Quic(Arc::new(cache))
    }

    #[inline]
    pub fn protocol(&self) -> Protocol {
        match self {
            Self::Quic(_) => Protocol::QUIC,
            Self::Udp => Protocol::UDP,
        }
    }

    pub fn use_quic(&self) -> bool {
        matches!(self, Self::Quic(_))
    }

    pub fn get_connection(&self, addr: &SocketAddr) -> BlockingClientConnection {
        match self {
            Self::Quic(cache) => BlockingClientConnection::Quic(cache.get_connection(addr)),
            Self::Udp => unreachable!(),
        }
    }
}

macro_rules! dispatch {
    ($(#[$meta:meta])* $vis:vis fn $name:ident$(<$($t:ident: $cons:ident + ?Sized),*>)?(&self $(, $arg:ident: $ty:ty)*) $(-> $out:ty)?) => {
        #[inline]
        $(#[$meta])*
        $vis fn $name$(<$($t: $cons + ?Sized),*>)?(&self $(, $arg:$ty)*) $(-> $out)? {
            match self {
                Self::Quic(this) => this.$name($($arg, )*),
                Self::Udp => unreachable!(),
            }
        }
    };
}

impl ClientConnection for BlockingClientConnection {
    dispatch!(fn server_addr(&self) -> &SocketAddr);
    dispatch!(fn send_data(&self, buffer: &[u8]) -> TransportResult<()>);
    dispatch!(fn send_data_async(&self, buffer: Arc<Vec<u8>>) -> TransportResult<()>);
    dispatch!(fn send_data_batch(&self, buffers: &[Vec<u8>]) -> TransportResult<()>);
    dispatch!(fn send_data_batch_async(&self, buffers: Vec<Vec<u8>>) -> TransportResult<()>);
}

#[async_trait::async_trait]
impl solana_connection_cache::nonblocking::client_connection::ClientConnection
    for NonblockingClientConnection
{
    dispatch!(fn server_addr(&self) -> &SocketAddr);

    async fn send_data(&self, buffer: &[u8]) -> TransportResult<()> {
        match self {
            Self::Quic(cache) => Ok(cache.send_data(buffer).await?),
            Self::Udp => unreachable!(),
        }
    }

    async fn send_data_batch(&self, buffers: &[Vec<u8>]) -> TransportResult<()> {
        match self {
            Self::Quic(cache) => Ok(cache.send_data_batch(buffers).await?),
            Self::Udp => unreachable!(),
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::connection_cache::ConnectionCache,
        solana_net_utils::sockets::{bind_to, localhost_port_range_for_tests},
        std::net::{IpAddr, Ipv4Addr, SocketAddr},
    };

    #[test]
    fn test_connection_with_specified_client_endpoint() {
        let port_range = localhost_port_range_for_tests();
        let mut port_range = port_range.0..port_range.1;
        let client_socket =
            bind_to(IpAddr::V4(Ipv4Addr::LOCALHOST), port_range.next().unwrap()).unwrap();
        let connection_cache = ConnectionCache::new_with_client_options(
            "connection_cache_test",
            1,                   // connection_pool_size
            Some(client_socket), // client_endpoint
            None,                // cert_info
            None,                // stake_info
        );

        // server port 1:
        let port1 = port_range.next().unwrap();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port1);
        let conn = connection_cache.get_connection(&addr);
        assert_eq!(conn.server_addr().port(), port1);

        // server port 2:
        let port2 = port_range.next().unwrap();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port2);
        let conn = connection_cache.get_connection(&addr);
        assert_eq!(conn.server_addr().port(), port2);
    }
}
