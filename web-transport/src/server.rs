use std::sync::Arc;

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};

use crate::{connect::Connect, settings::Settings, ServerError, Session};

pub const ALPN: &[u8] = b"h3";

pub struct ServerBuilder {
    addr: std::net::SocketAddr,
}

impl ServerBuilder {
    pub fn new(addr: std::net::SocketAddr) -> Self {
        Self { addr }
    }

    pub fn build(
        self,
        cert_chain: Vec<rustls::pki_types::CertificateDer<'static>>,
        key: rustls::pki_types::PrivateKeyDer<'static>,
    ) -> Result<Server, ServerError> {
        let mut config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(cert_chain, key)
            .map_err(|e| ServerError::IoError(Arc::new(std::io::Error::other(e))))?;

        config.alpn_protocols = vec![ALPN.to_vec()];

        let config: quinn::crypto::rustls::QuicServerConfig = config
            .try_into()
            .map_err(|e| ServerError::IoError(Arc::new(std::io::Error::other(e))))?;

        let server_config = quinn::ServerConfig::with_crypto(Arc::new(config));

        let endpoint = quinn::Endpoint::server(server_config, self.addr)
            .map_err(|e| ServerError::IoError(Arc::new(e)))?;

        Ok(Server::new(endpoint))
    }

    pub fn build_from_endpoint(endpoint: quinn::Endpoint) -> Server {
        Server::new(endpoint)
    }
}

pub struct Server {
    endpoint: quinn::Endpoint,
    accept: FuturesUnordered<BoxFuture<'static, Result<Request, ServerError>>>,
}

impl Server {
    fn new(endpoint: quinn::Endpoint) -> Self {
        Self {
            endpoint,
            accept: FuturesUnordered::new(),
        }
    }

    pub async fn accept(&mut self) -> Option<Request> {
        loop {
            tokio::select! {
                res = self.endpoint.accept() => {
                    let incoming = res?;
                    self.accept.push(Box::pin(async move {
                        let conn = incoming.await?;
                        Request::accept(conn).await
                    }));
                }
                Some(res) = self.accept.next() => {
                    match res {
                        Ok(request) => return Some(request),
                        Err(e) => {
                            log::warn!("failed to accept WebTransport session: {e}");
                            continue;
                        }
                    }
                }
            }
        }
    }

    pub fn local_addr(&self) -> std::io::Result<std::net::SocketAddr> {
        self.endpoint.local_addr()
    }
}

pub struct Request {
    conn: quinn::Connection,
    settings: Settings,
    connect: Connect,
}

impl Request {
    async fn accept(conn: quinn::Connection) -> Result<Self, ServerError> {
        let settings = Settings::connect(&conn).await?;
        let connect = Connect::accept(&conn).await?;

        Ok(Self {
            conn,
            settings,
            connect,
        })
    }

    pub fn url(&self) -> &url::Url {
        self.connect.url()
    }

    pub fn remote_address(&self) -> std::net::SocketAddr {
        self.conn.remote_address()
    }

    pub async fn ok(mut self) -> Result<Session, ServerError> {
        self.connect.respond(http::StatusCode::OK).await?;
        Ok(Session::new(self.conn, self.settings, self.connect))
    }

    pub async fn reject(mut self, status: http::StatusCode) -> Result<(), ServerError> {
        self.connect.respond(status).await?;
        Ok(())
    }
}
