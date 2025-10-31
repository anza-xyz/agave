//! This module provides [`NodeAddressService`] structure that implements
//! [`LeaderUpdater`] trait to track upcoming leaders and maintains an
//! up-to-date mapping of leader id to TPU socket address.
//!
//! # Examples
//!
//! This example shows how to use [`NodeAddressService`] to implement
//! [`LeaderUpdater`] using some custom slot update provider. Typically, it can
//! be done with zero-cost abstraction as shown below. The case of
//! [`WebSocketNodeAddressService`] requires, contrary, introducing task and
//! channel due to specifics of the PubsubClient API implementation.
//!
//! For the sake of the example, let's assume we have some custom slot updates
//! that we receive by UDP.
//!
//! ```
//!  use async_stream::stream;
//!  use tokio::net::UdpSocket;
//!
//!  pub struct SlotUpdaterNodeAddressService {
//!    service: NodeAddressService,
//! }
//!
//! impl SlotUpdaterNodeAddressService {
//!    pub async fn run(
//!        rpc_client: Arc<RpcClient>,
//!        bind_address: SocketAddr,
//!        config: LeaderTpuCacheServiceConfig,
//!        cancel: CancellationToken,
//!    ) -> Result<Self, NodeAddressServiceError> {
//!        let socket = UdpSocket::bind(bind_address)
//!            .await
//!            .map_err(|_e| NodeAddressServiceError::InitializationFailed)?;
//!        let stream = Self::udp_slot_event_stream(socket);
//!        let service = NodeAddressService::run(rpc_client, stream, config, cancel).await?;
//!
//!        Ok(Self { service })
//!    }
//!
//!    fn udp_slot_event_stream(socket: UdpSocket) -> impl Stream<Item = SlotEvent> + Send + 'static {
//!        stream! {
//!            let mut buf = vec![0u8; 2048];
//!
//!            loop {
//!                match socket.recv_from(&mut buf).await {
//!                    Ok((len, from)) => {
//!                        let data = &buf[..len];
//!                        match serde_json::from_slice::<SlotMessage>(data) {
//!                            Ok(msg) => {
//!                                match msg.status {
//!                                    SlotStatus::FirstShredReceived => yield SlotEvent::Start(msg.slot),
//!                                    SlotStatus::Completed => yield SlotEvent::End(msg.slot),
//!                                    _ => continue,
//!                                };
//!                            }
//!                            Err(e) => error!("Failed to parse SlotMessage from {from}: {e}"),
//!                        }
//!                    }
//!                    Err(e) => {
//!                        error!("UDP receive failed: {e}");
//!                        break;
//!                    }
//!                }
//!            }
//!        }
//!    }
//! }
//! ```
//!
use {
    crate::{
        leader_updater::LeaderUpdater,
        logging::error,
        node_address_service::{
            leader_tpu_cache_service::{Error as LeaderTpuCacheServiceError, LeaderUpdateReceiver},
            slot_receiver::SlotReceiverError,
            slot_update_service::Error as SlotUpdateServiceError,
        },
    },
    async_trait::async_trait,
    futures::StreamExt,
    solana_clock::Slot,
    solana_commitment_config::CommitmentConfig,
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::client_error::Error as ClientError,
    std::{net::SocketAddr, sync::Arc},
    thiserror::Error,
    tokio::join,
    tokio_util::sync::CancellationToken,
};

pub mod leader_tpu_cache_service;
pub mod recent_leader_slots;
pub mod slot_event;
pub mod slot_receiver;
pub mod slot_update_service;
pub use {
    leader_tpu_cache_service::{Config as LeaderTpuCacheServiceConfig, LeaderTpuCacheService},
    recent_leader_slots::RecentLeaderSlots,
    slot_event::SlotEvent,
    slot_receiver::SlotReceiver,
    slot_update_service::SlotUpdateService,
};

/// [`NodeAddressService`] is a convenience wrapper for
/// [`SlotUpdateService`] and [`LeaderTpuCacheService`] to track
/// upcoming leaders and maintains an up-to-date mapping of leader id to TPU
/// socket address.
pub struct NodeAddressService {
    leaders_receiver: LeaderUpdateReceiver,
    slot_receiver: SlotReceiver,
    slot_update_service: SlotUpdateService,
    leader_cache_service: LeaderTpuCacheService,
}

impl NodeAddressService {
    /// Run the [`NodeAddressService`].
    pub async fn run(
        rpc_client: Arc<RpcClient>,
        slot_update_stream: impl StreamExt<Item = SlotEvent> + Send + 'static,
        config: LeaderTpuCacheServiceConfig,
        cancel: CancellationToken,
    ) -> Result<Self, NodeAddressServiceError> {
        let start_slot = rpc_client
            .get_slot_with_commitment(CommitmentConfig::processed())
            .await?;

        let (slot_receiver, slot_update_service) =
            SlotUpdateService::run(start_slot, slot_update_stream, cancel.clone())?;
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

    /// Returns the estimated current slot.
    pub fn current_slot(&self) -> Slot {
        self.slot_receiver.slot().first_slot()
    }
}

#[async_trait]
impl LeaderUpdater for NodeAddressService {
    fn next_leaders(&mut self, lookahead_leaders: usize) -> Vec<SocketAddr> {
        self.leaders_receiver.leaders(lookahead_leaders)
    }

    async fn stop(&mut self) {
        if let Err(e) = self.shutdown().await {
            error!("Failed to shutdown NodeAddressService: {e}");
        }
    }
}

#[derive(Debug, Error)]
pub enum NodeAddressServiceError {
    #[error(transparent)]
    RpcError(#[from] ClientError),

    #[error(transparent)]
    SlotReceiverError(#[from] SlotReceiverError),

    #[error(transparent)]
    WebsocketSlotUpdateServiceError(#[from] SlotUpdateServiceError),

    #[error(transparent)]
    LeaderTpuCacheServiceError(#[from] LeaderTpuCacheServiceError),
}
