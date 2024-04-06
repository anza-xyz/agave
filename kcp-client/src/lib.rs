#![allow(clippy::arithmetic_side_effects)]

pub mod kcp_client;
pub mod nonblocking;

#[macro_use]
extern crate solana_metrics;

use {
    crate::{
        kcp_client::KcpClientConnection as BlockingKcpConnection,
        nonblocking::kcp_client::KcpClientConnection as NonblockingKcpConnection,
    },
    rand::{thread_rng, Rng},
    solana_connection_cache::{
        connection_cache::{
            BaseClientConnection, ClientError, ConnectionManager, ConnectionPool,
            ConnectionPoolError, NewConnectionConfig, Protocol,
        },
        connection_cache_stats::ConnectionCacheStats,
    },
    solana_sdk::signature::Keypair,
    std::{
        net::{SocketAddr, UdpSocket},
        sync::Arc,
    },
};

pub struct KcpPool {
    connections: Vec<Arc<Kcp>>,
}

impl ConnectionPool for KcpPool {
    type BaseClientConnection = Kcp;
    type NewConnectionConfig = KcpConfig;

    fn add_connection(&mut self, config: &Self::NewConnectionConfig, addr: &SocketAddr) -> usize {
        let connection = self.create_pool_entry(config, addr);
        let idx = self.connections.len();
        self.connections.push(connection);
        idx
    }

    fn num_connections(&self) -> usize {
        self.connections.len()
    }

    fn get(&self, index: usize) -> Result<Arc<Self::BaseClientConnection>, ConnectionPoolError> {
        self.connections
            .get(index)
            .cloned()
            .ok_or(ConnectionPoolError::IndexOutOfRange)
    }

    fn create_pool_entry(
        &self,
        config: &Self::NewConnectionConfig,
        addr: &SocketAddr,
    ) -> Arc<Self::BaseClientConnection> {
        Arc::new(Kcp(BlockingKcpConnection::new(
            config.udp_socket.try_clone().unwrap(),
            *addr,
            config.conv,
        )))
    }
}

pub struct KcpConfig {
    udp_socket: UdpSocket,
    conv: u32,
}

impl NewConnectionConfig for KcpConfig {
    fn new() -> Result<Self, ClientError> {
        let mut rng = thread_rng();
        let conv: u32 = rng.gen();
        let socket = UdpSocket::bind("0.0.0.0:0").map_err(Into::<ClientError>::into)?;
        Ok(Self {
            udp_socket: socket,
            conv,
        })
    }
}

pub struct Kcp(BlockingKcpConnection);

impl BaseClientConnection for Kcp {
    type BlockingClientConnection = BlockingKcpConnection;
    type NonblockingClientConnection = NonblockingKcpConnection;

    fn new_blocking_connection(
        &self,
        addr: SocketAddr,
        _stats: Arc<ConnectionCacheStats>,
    ) -> Arc<Self::BlockingClientConnection> {
        Arc::new(BlockingKcpConnection::new(
            self.0.kcp.output.0.try_clone().unwrap(),
            addr,
            self.0.kcp.conv,
        ))
    }

    fn new_nonblocking_connection(
        &self,
        addr: SocketAddr,
        _stats: Arc<ConnectionCacheStats>,
    ) -> Arc<Self::NonblockingClientConnection> {
        Arc::new(NonblockingKcpConnection::new(
            self.0.kcp.output.0.try_clone().unwrap(),
            addr,
            self.0.kcp.conv,
        ))
    }
}

#[derive(Default)]
pub struct KcpConnectionManager {}

impl ConnectionManager for KcpConnectionManager {
    type ConnectionPool = KcpPool;
    type NewConnectionConfig = KcpConfig;

    const PROTOCOL: Protocol = Protocol::KCP;

    fn new_connection_pool(&self) -> Self::ConnectionPool {
        KcpPool {
            connections: Vec::default(),
        }
    }

    fn new_connection_config(&self) -> Self::NewConnectionConfig {
        KcpConfig::new().unwrap()
    }

    fn update_key(&self, _key: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }
}
