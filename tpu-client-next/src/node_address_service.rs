//! This module provides [`NodeAddressService`] structure that implements [`LeaderUpdater`] trait to
//! track upcoming leaders and maintains an up-to-date mapping of leader id to TPU socket address.
//!
//! # Examples
//!
//! This example shows how to use [`NodeAddressService`] to implement [`LeaderUpdater`] using some
//! custom slot update provider. Typically, it can be done with zero-cost abstraction as shown
//! below. The case of `WebSocketNodeAddressService` requires, contrary, introducing task and
//! channel due to specifics of the PubsubClient API implementation.
//!
//! For the sake of the example, let's assume we have some custom slot updates that we receive by
//! UDP.
//!
//! ```ignore
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
            slot_update_service::Error as SlotUpdateServiceError,
        },
    },
    async_trait::async_trait,
    futures::StreamExt,
    solana_clock::Slot,
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
    leader_tpu_cache_service::{
        ClusterInfoProvider, Config as LeaderTpuCacheServiceConfig, LeaderTpuCacheService,
    },
    recent_leader_slots::RecentLeaderSlots,
    slot_event::SlotEvent,
    slot_receiver::SlotReceiver,
    slot_update_service::SlotUpdateService,
};

/// [`NodeAddressService`] is a convenience wrapper for [`SlotUpdateService`] and
/// [`LeaderTpuCacheService`] to track upcoming leaders and maintains an up-to-date mapping of
/// leader id to TPU socket address.
pub struct NodeAddressService {
    leaders_receiver: LeaderUpdateReceiver,
    slot_receiver: SlotReceiver,
    slot_update_service: SlotUpdateService,
    leader_cache_service: LeaderTpuCacheService,
}

impl NodeAddressService {
    /// Run the [`NodeAddressService`].
    ///
    /// On success it starts [`SlotUpdateService`] together with [`LeaderTpuCacheService`] and
    /// returns [`NodeAddressService`] instance which provides method to fetch next leaders. To run
    /// mentioned services, it takes `cluster_info_provider` which abstracts access to information
    /// about the cluster (see [`ClusterInfoProvider`]), `slot_update_stream` provides stream of
    /// slot updates, `start_slot` is the initial slot to start from, `config` provides
    /// configuration for the leader TPU cache service, and finally `cancel` is a cancellation token
    /// to stop the service.
    ///
    /// On failure, it will return appropriate error.
    pub async fn run(
        cluster_info_provider: Arc<impl ClusterInfoProvider + 'static>,
        slot_update_stream: impl StreamExt<Item = SlotEvent> + Send + 'static,
        config: LeaderTpuCacheServiceConfig,
        cancel: CancellationToken,
    ) -> Result<Self, NodeAddressServiceError> {
        let initial_slot = cluster_info_provider
            .initial_slot()
            .await
            .map_err(NodeAddressServiceError::from)?;
        let (slot_receiver, slot_update_service) =
            SlotUpdateService::run(initial_slot, slot_update_stream, cancel.clone())?;
        let (leaders_receiver, leader_cache_service) = LeaderTpuCacheService::run(
            cluster_info_provider,
            slot_receiver.clone(),
            config,
            cancel,
        )
        .await?;

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
    pub fn estimated_current_slot(&self) -> Slot {
        self.slot_receiver.slot()
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
    SlotUpdateServiceError(#[from] SlotUpdateServiceError),

    #[error(transparent)]
    LeaderTpuCacheServiceError(#[from] LeaderTpuCacheServiceError),
}
