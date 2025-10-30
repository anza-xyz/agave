//! This module provides [`NodeAddressService`] structure that implements
//! [`LeaderUpdater`] trait to track upcoming leaders and maintains an
//! up-to-date mapping of leader id to TPU socket address.
//!
use {
    crate::{
        leader_updater::LeaderUpdater, logging::error,
        node_address_service::leader_tpu_cache_service::LeaderUpdateReceiver,
    },
    async_trait::async_trait,
    solana_clock::Slot,
    solana_commitment_config::CommitmentConfig,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    std::{net::SocketAddr, sync::Arc},
    tokio::join,
    tokio_util::sync::CancellationToken,
};

pub mod error;
pub mod leader_tpu_cache_service;
pub mod recent_leader_slots;
pub mod slot_receiver;
pub mod websocket_slot_update_service;
pub use {
    error::NodeAddressServiceError,
    leader_tpu_cache_service::{Config as LeaderTpuCacheServiceConfig, LeaderTpuCacheService},
    recent_leader_slots::RecentLeaderSlots,
    slot_receiver::SlotReceiver,
    websocket_slot_update_service::WebsocketSlotUpdateService,
};

/// [`NodeAddressService`] is a convenience wrapper for
/// [`WebsocketSlotUpdateService`] and [`LeaderTpuCacheService`] to track
/// upcoming leaders and maintains an up-to-date mapping of leader id to TPU
/// socket address.
pub struct NodeAddressService {
    leaders_receiver: LeaderUpdateReceiver,
    slot_receiver: SlotReceiver,
    slot_update_service: WebsocketSlotUpdateService,
    leader_cache_service: LeaderTpuCacheService,
}

impl NodeAddressService {
    /// Run the [`NodeAddressService`].
    pub async fn run(
        rpc_client: Arc<RpcClient>,
        websocket_url: String,
        config: LeaderTpuCacheServiceConfig,
        cancel: CancellationToken,
    ) -> Result<Self, NodeAddressServiceError> {
        let start_slot = rpc_client
            .get_slot_with_commitment(CommitmentConfig::processed())
            .await?;

        let (slot_receiver, slot_update_service) =
            WebsocketSlotUpdateService::run(start_slot, websocket_url, cancel.clone())?;
        let (leaders_receiver, leader_cache_service) =
            LeaderTpuCacheService::run(rpc_client, slot_receiver.clone(), config, cancel).await?;

        Ok(Self {
            leaders_receiver,
            slot_receiver,
            slot_update_service,
            leader_cache_service,
        })
    }

    pub async fn shutdown(&mut self) -> Result<(), NodeAddressServiceError> {
        let (slot_update_service_res, leader_cache_service_res) = join!(
            self.slot_update_service.shutdown(),
            self.leader_cache_service.shutdown(),
        );
        slot_update_service_res?;
        leader_cache_service_res?;
        Ok(())
    }

    pub fn estimated_current_slot(&self) -> Slot {
        self.slot_receiver.slot().first_slot()
    }
}

#[async_trait]
impl LeaderUpdater for NodeAddressService {
    fn next_leaders(&mut self, lookahead: usize) -> Vec<SocketAddr> {
        self.leaders_receiver.next_leaders(lookahead)
    }

    async fn stop(&mut self) {
        if let Err(e) = self.shutdown().await {
            error!("Failed to shutdown NodeAddressService: {e}");
        }
    }
}
