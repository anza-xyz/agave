//! This module provides [`NodeAddressService`] structure that implements
//! [`LeaderUpdater`] trait to track upcoming leaders and maintains an
//! up-to-date mapping of leader id to TPU socket address.
use {
    crate::leader_updater::LeaderUpdater,
    async_trait::async_trait,
    solana_clock::{Slot, DEFAULT_MS_PER_SLOT},
    solana_commitment_config::CommitmentConfig,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_time_utils::timestamp,
    std::{net::SocketAddr, sync::Arc},
    tokio::{sync::watch, task::JoinHandle, time::Duration},
    tokio_util::sync::CancellationToken,
};

pub mod error;
pub mod leader_tpu_cache_service;
pub mod websocket_slot_update_service;
pub use {
    error::NodeAddressServiceError,
    leader_tpu_cache_service::{LeaderTpuCacheService, LeaderTpuCacheServiceConfig},
    websocket_slot_update_service::WebsocketSlotUpdateService,
};

/// [`NodeAddressService`] is a convenience wrapper for
/// [`WebsocketSlotUpdateService`] and [`LeaderTpuCacheService`] to track
/// upcoming leaders and maintains an up-to-date mapping of leader id to TPU
/// socket address.
pub struct NodeAddressService {
    leaders_receiver: watch::Receiver<(Slot, Vec<SocketAddr>)>,
    slot_watcher_service_handle: JoinHandle<Result<(), NodeAddressServiceError>>,
    leader_tpu_service_handle: JoinHandle<Result<(), NodeAddressServiceError>>,
    cancel: CancellationToken,
}

impl NodeAddressService {
    pub async fn run(
        rpc_client: Arc<RpcClient>,
        websocket_url: &str,
        config: LeaderTpuCacheServiceConfig,
        cancel: CancellationToken,
    ) -> Result<Self, NodeAddressServiceError> {
        let start_slot = rpc_client
            .get_slot_with_commitment(CommitmentConfig::processed())
            .await?;

        let estimated_slot_duration = Duration::from_millis(DEFAULT_MS_PER_SLOT);
        let (slot_sender, slot_receiver) =
            watch::channel((start_slot, timestamp(), estimated_slot_duration));
        let (leaders_sender, leaders_receiver) =
            watch::channel::<(Slot, Vec<SocketAddr>)>((start_slot, vec![]));
        let slot_watcher_service_handle = tokio::spawn(WebsocketSlotUpdateService::run(
            start_slot,
            slot_sender,
            slot_receiver.clone(),
            websocket_url.to_string(),
            cancel.clone(),
        ));
        let leader_cache_service = LeaderTpuCacheService::new(
            rpc_client,
            slot_receiver,
            leaders_sender,
            config,
            cancel.clone(),
        )
        .await?;
        let leader_tpu_service_handle = tokio::spawn(leader_cache_service.run());

        Ok(Self {
            leaders_receiver,
            slot_watcher_service_handle,
            leader_tpu_service_handle,
            cancel,
        })
    }

    pub async fn join(self) -> Result<(), NodeAddressServiceError> {
        self.slot_watcher_service_handle.await??;
        self.leader_tpu_service_handle.await??;

        Ok(())
    }
}

#[async_trait]
impl LeaderUpdater for NodeAddressService {
    //TODO(klykov): we need to consier removing next leaders with lookahead_leaders in the future
    fn next_leaders(&mut self, _lookahead_leaders: usize) -> Vec<SocketAddr> {
        self.leaders_receiver.borrow().1.clone()
    }

    //TODO(klykov): stop should return error to handle join and also stop should
    //consume the object.
    async fn stop(&mut self) {
        self.cancel.cancel();
    }
}
