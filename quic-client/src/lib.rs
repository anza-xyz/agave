#![cfg(feature = "agave-unstable-api")]
#![allow(clippy::arithmetic_side_effects)]

pub mod nonblocking;
pub mod quic_client;

#[macro_use]
extern crate solana_metrics;

use {
    crate::{
        nonblocking::quic_client::{
            QuicClient, QuicClientConnection as NonblockingQuicClientConnection,
            QuicLazyInitializedEndpoint,
        },
        quic_client::{
            close_quic_connection, QuicClientConnection as BlockingQuicClientConnection,
        },
    },
    log::debug,
    quic_client::get_runtime,
    quinn::{Endpoint, EndpointConfig, TokioRuntime},
    solana_connection_cache::{
        connection_cache::{
            BaseClientConnection, ClientError, ConnectionCache, ConnectionManager, ConnectionPool,
            ConnectionPoolError, NewConnectionConfig, Protocol,
        },
        connection_cache_stats::ConnectionCacheStats,
    },
    solana_keypair::Keypair,
    solana_pubkey::Pubkey,
    solana_signer::Signer,
    solana_streamer::streamer::StakedNodes,
    solana_tls_utils::{new_dummy_x509_certificate, QuicClientCertificate},
    std::{
        net::{IpAddr, SocketAddr, UdpSocket},
        sync::{Arc, RwLock},
    },
};

pub struct QuicPool {
    connections: Vec<Arc<Quic>>,
    endpoint: Arc<QuicLazyInitializedEndpoint>,
}
impl ConnectionPool for QuicPool {
    type BaseClientConnection = Quic;
    type NewConnectionConfig = QuicConfig;

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
        _config: &Self::NewConnectionConfig,
        addr: &SocketAddr,
    ) -> Arc<Self::BaseClientConnection> {
        Arc::new(Quic(Arc::new(QuicClient::new(
            self.endpoint.clone(),
            *addr,
        ))))
    }
}

impl Drop for QuicPool {
    fn drop(&mut self) {
        debug!(
            "Dropping QuicPool with {} connections",
            self.connections.len()
        );
        for connection in self.connections.drain(..) {
            // Explicitly drop each connection to ensure resources are released
            close_quic_connection(connection.0.clone());
        }
    }
}

pub struct QuicConfig {
    // Arc to prevent having to copy the struct
    client_certificate: RwLock<Arc<QuicClientCertificate>>,
    maybe_staked_nodes: Option<Arc<RwLock<StakedNodes>>>,
    maybe_client_pubkey: Option<Pubkey>,

    // The optional specified endpoint for the quic based client connections
    // If not specified, the connection cache will create as needed.
    client_endpoint: Option<Endpoint>,
}

impl Clone for QuicConfig {
    fn clone(&self) -> Self {
        let cert_guard = self.client_certificate.read().unwrap();
        QuicConfig {
            client_certificate: RwLock::new(cert_guard.clone()),
            maybe_staked_nodes: self.maybe_staked_nodes.clone(),
            maybe_client_pubkey: self.maybe_client_pubkey,
            client_endpoint: self.client_endpoint.clone(),
        }
    }
}

impl NewConnectionConfig for QuicConfig {
    fn new() -> Result<Self, ClientError> {
        let (cert, priv_key) = new_dummy_x509_certificate(&Keypair::new());
        Ok(Self {
            client_certificate: RwLock::new(Arc::new(QuicClientCertificate {
                certificate: cert,
                key: priv_key,
            })),
            maybe_staked_nodes: None,
            maybe_client_pubkey: None,
            client_endpoint: None,
        })
    }
}

impl QuicConfig {
    fn create_endpoint(&self) -> QuicLazyInitializedEndpoint {
        let cert_guard = self.client_certificate.read().unwrap();
        QuicLazyInitializedEndpoint::new(cert_guard.clone(), self.client_endpoint.as_ref().cloned())
    }

    pub fn update_client_certificate(&mut self, keypair: &Keypair, _ipaddr: IpAddr) {
        let (cert, priv_key) = new_dummy_x509_certificate(keypair);

        let mut cert_guard = self.client_certificate.write().unwrap();

        *cert_guard = Arc::new(QuicClientCertificate {
            certificate: cert,
            key: priv_key,
        });
    }

    pub fn update_keypair(&self, keypair: &Keypair) {
        let (cert, priv_key) = new_dummy_x509_certificate(keypair);

        let mut cert_guard = self.client_certificate.write().unwrap();

        *cert_guard = Arc::new(QuicClientCertificate {
            certificate: cert,
            key: priv_key,
        });
    }

    pub fn set_staked_nodes(
        &mut self,
        staked_nodes: &Arc<RwLock<StakedNodes>>,
        client_pubkey: &Pubkey,
    ) {
        self.maybe_staked_nodes = Some(staked_nodes.clone());
        self.maybe_client_pubkey = Some(*client_pubkey);
    }

    pub fn update_client_endpoint(&mut self, client_socket: UdpSocket) {
        let runtime = get_runtime();
        let _guard = runtime.enter();
        let config = EndpointConfig::default();
        self.client_endpoint = Some(
            quinn::Endpoint::new(config, None, client_socket, Arc::new(TokioRuntime))
                .expect("QuicNewConnection::create_endpoint quinn::Endpoint::new"),
        );
    }
}

pub struct Quic(Arc<QuicClient>);
impl BaseClientConnection for Quic {
    type BlockingClientConnection = BlockingQuicClientConnection;
    type NonblockingClientConnection = NonblockingQuicClientConnection;

    fn new_blocking_connection(
        &self,
        _addr: SocketAddr,
        stats: Arc<ConnectionCacheStats>,
    ) -> Arc<Self::BlockingClientConnection> {
        Arc::new(BlockingQuicClientConnection::new_with_client(
            self.0.clone(),
            stats,
        ))
    }

    fn new_nonblocking_connection(
        &self,
        _addr: SocketAddr,
        stats: Arc<ConnectionCacheStats>,
    ) -> Arc<Self::NonblockingClientConnection> {
        Arc::new(NonblockingQuicClientConnection::new_with_client(
            self.0.clone(),
            stats,
        ))
    }
}

pub struct QuicConnectionManager {
    connection_config: QuicConfig,
}

impl ConnectionManager for QuicConnectionManager {
    type ConnectionPool = QuicPool;
    type NewConnectionConfig = QuicConfig;

    const PROTOCOL: Protocol = Protocol::QUIC;

    fn new_connection_pool(&self) -> Self::ConnectionPool {
        QuicPool {
            connections: Vec::default(),
            endpoint: Arc::new(self.connection_config.create_endpoint()),
        }
    }

    fn new_connection_config(&self) -> QuicConfig {
        self.connection_config.clone()
    }

    fn update_key(&self, key: &Keypair) -> Result<(), Box<dyn std::error::Error>> {
        self.connection_config.update_keypair(key);
        Ok(())
    }
}

impl QuicConnectionManager {
    pub fn new_with_connection_config(connection_config: QuicConfig) -> Self {
        Self { connection_config }
    }
}

pub type QuicConnectionCache = ConnectionCache<QuicPool, QuicConnectionManager, QuicConfig>;

pub fn new_quic_connection_cache(
    name: &'static str,
    keypair: &Keypair,
    ipaddr: IpAddr,
    staked_nodes: &Arc<RwLock<StakedNodes>>,
    connection_pool_size: usize,
) -> Result<QuicConnectionCache, ClientError> {
    let mut config = QuicConfig::new()?;
    config.update_client_certificate(keypair, ipaddr);
    config.set_staked_nodes(staked_nodes, &keypair.pubkey());
    let connection_manager = QuicConnectionManager::new_with_connection_config(config);
    ConnectionCache::new(name, connection_manager, connection_pool_size)
}
