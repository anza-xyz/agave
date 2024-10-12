//! This module provides [`LeaderUpdater`] trait along with it's implementations
//! `LeaderUpdaterService` and `PinnedLeaderUpdater`. Currently, the main
//! purpose of [`LeaderUpdater`] is to abstract over leader updates, hiding the
//! details of how leaders are retrieved and which structures are used.
//! Specifically, `LeaderUpdaterService` keeps [`LeaderTpuService`] internal to
//! this module. Yet, it also allows to implement custom leader estimation.
use {
    async_trait::async_trait,
    log::*,
    solana_connection_cache::connection_cache::Protocol,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_tpu_client::nonblocking::tpu_client::LeaderTpuService,
    std::{
        fmt,
        net::SocketAddr,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
    },
};

/// [`LeaderUpdater`] trait abstracts out functionality required for the
/// [`ConnectionWorkersScheduler`](crate::network::ConnectionWorkersScheduler)
/// to identify next leaders to send transactions to.
#[async_trait]
pub trait LeaderUpdater: Send {
    fn next_num_lookahead_slots_leaders(&self) -> Vec<SocketAddr>;
    async fn stop(&mut self);
}

pub struct LeaderUpdaterError;

impl fmt::Display for LeaderUpdaterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Leader updater encountered an error")
    }
}

impl fmt::Debug for LeaderUpdaterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LeaderUpdaterError")
    }
}

/// Creates a [`LeaderUpdater`] based on the configuration provided by the
/// caller. If a pinned address is provided, it will return a
/// `PinnedLeaderUpdater` that always returns the provided address instead of
/// checking leader schedule. Otherwise, it creates a `LeaderUpdaterService`
/// which dynamically updates the leaders by connecting to the network via the
/// [`LeaderTpuService`].
pub async fn create_leader_updater(
    rpc_client: Arc<RpcClient>,
    websocket_url: String,
    lookahead_slots: u64,
    pinned_address: Option<SocketAddr>,
) -> Result<Box<dyn LeaderUpdater>, LeaderUpdaterError> {
    if let Some(pinned_address) = pinned_address {
        return Ok(Box::new(PinnedLeaderUpdater {
            address: vec![pinned_address],
        }));
    }

    let exit = Arc::new(AtomicBool::new(false));
    let leader_tpu_service =
        LeaderTpuService::new(rpc_client, &websocket_url, Protocol::QUIC, exit.clone())
            .await
            .map_err(|error| {
                error!("Failed to create a LeaderTpuService: {error}");
                LeaderUpdaterError
            })?;
    Ok(Box::new(LeaderUpdaterService {
        leader_tpu_service,
        exit,
        lookahead_slots,
    }))
}

/// `LeaderUpdaterService` is an implementation of the [`LeaderUpdater`] trait
/// that dynamically retrieves the current and upcoming leaders by communicating
/// with the Solana network using [`LeaderTpuService`].
struct LeaderUpdaterService {
    leader_tpu_service: LeaderTpuService,
    exit: Arc<AtomicBool>,
    /// Number of *estimated* next leaders. If the estimation for the current
    /// leader is wrong and we have sent to only one leader, we may lose all the
    /// txs (depends on the forwarding policy).
    lookahead_slots: u64,
}

#[async_trait]
impl LeaderUpdater for LeaderUpdaterService {
    fn next_num_lookahead_slots_leaders(&self) -> Vec<SocketAddr> {
        self.leader_tpu_service
            .leader_tpu_sockets(self.lookahead_slots)
    }

    async fn stop(&mut self) {
        self.exit.store(true, Ordering::Relaxed);
        self.leader_tpu_service.join().await;
    }
}

/// `PinnedLeaderUpdater` is an implementation of [`LeaderUpdater`] that always
/// returns a fixed, "pinned" leader address. It is mainly used for testing.
struct PinnedLeaderUpdater {
    address: Vec<SocketAddr>,
}

#[async_trait]
impl LeaderUpdater for PinnedLeaderUpdater {
    fn next_num_lookahead_slots_leaders(&self) -> Vec<SocketAddr> {
        self.address.clone()
    }

    async fn stop(&mut self) {}
}
