use {
    crate::connection_cache::ConnectionCache,
    solana_keypair::Keypair,
    std::{net::UdpSocket, sync::Arc},
    tokio::runtime::Handle as RuntimeHandle,
    tokio_util::sync::CancellationToken,
};

/// [`ClientOption`] enum represents the available client types for TPU
/// communication:
/// * [`ConnectionCacheClient`]: Uses a shared [`ConnectionCache`] to manage
///   connections efficiently.
/// * [`TpuClientNextClient`]: Relies on the `tpu-client-next` crate and
///   requires a reference to a [`Keypair`].
pub enum ClientOption<'a> {
    ConnectionCache(Arc<ConnectionCache>),
    TpuClientNext(&'a Keypair, UdpSocket, RuntimeHandle, CancellationToken),
}
