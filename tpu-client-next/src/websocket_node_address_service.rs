//! This module provides [`WebsocketNodeAddressService`] that is used to get slot
//! updates via WebSocket interface.
use {
    crate::{
        leader_updater::LeaderUpdater,
        logging::{error, info},
        node_address_service::{
            LeaderTpuCacheServiceConfig, NodeAddressService, NodeAddressServiceError, SlotEvent,
        },
    },
    async_trait::async_trait,
    futures::Stream,
    futures_util::stream::StreamExt,
    solana_clock::Slot,
    solana_pubsub_client::nonblocking::pubsub_client::{PubsubClient, PubsubClientError},
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::{client_error::Error as ClientError, response::SlotUpdate},
    std::{net::SocketAddr, sync::Arc, time::Duration},
    thiserror::Error,
    tokio::{
        sync::mpsc::{self, error::SendTimeoutError},
        task::JoinHandle,
    },
    tokio_stream::wrappers::ReceiverStream,
    tokio_util::sync::CancellationToken,
};

/// [`WebsocketNodeAddressService`] provides node updates using WebSocket Pubsub
/// client for the slot updates.
pub struct WebsocketNodeAddressService {
    service: NodeAddressService,
    ws_task_handle: Option<JoinHandle<Result<(), Error>>>,
}

impl WebsocketNodeAddressService {
    pub async fn run(
        rpc_client: Arc<RpcClient>,
        websocket_url: String,
        config: LeaderTpuCacheServiceConfig,
        cancel: CancellationToken,
    ) -> Result<Self, Error> {
        let (websocket_slot_event_stream, ws_task_handle) =
            websocket_slot_event_stream(websocket_url);
        let service =
            NodeAddressService::run(rpc_client, websocket_slot_event_stream, config, cancel)
                .await?;

        Ok(Self {
            service,
            ws_task_handle: Some(ws_task_handle),
        })
    }

    pub async fn shutdown(&mut self) -> Result<(), Error> {
        self.service.shutdown().await?;
        if let Some(handle) = self.ws_task_handle.take() {
            handle.await??;
        }
        Ok(())
    }

    /// Returns the estimated current slot.
    pub fn current_slot(&self) -> Slot {
        self.service.estimated_current_slot()
    }
}

#[async_trait]
impl LeaderUpdater for WebsocketNodeAddressService {
    fn next_leaders(&mut self, lookahead_leaders: usize) -> Vec<SocketAddr> {
        self.service.next_leaders(lookahead_leaders)
    }

    async fn stop(&mut self) {
        if let Err(e) = self.shutdown().await {
            error!("Failed to shutdown WebsocketNodeAddressService: {e}");
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    RpcError(#[from] ClientError),

    #[error(transparent)]
    PubsubError(#[from] PubsubClientError),

    #[error(transparent)]
    JoinError(#[from] tokio::task::JoinError),

    #[error(transparent)]
    NodeAddressServiceError(#[from] NodeAddressServiceError),
}

fn websocket_slot_event_stream(
    websocket_url: String,
) -> (impl Stream<Item = SlotEvent>, JoinHandle<Result<(), Error>>) {
    const SEND_TIMEOUT: Duration = Duration::from_millis(100);
    let (tx, rx) = mpsc::channel::<SlotEvent>(256);

    let handle: JoinHandle<Result<(), Error>> = tokio::spawn(async move {
        let pubsub_client = PubsubClient::new(websocket_url).await?;
        let (mut notifications, unsubscribe) = pubsub_client.slot_updates_subscribe().await?;

        while let Some(event) = notifications.next().await {
            let Some(event) = map_websocket_update_to_slot_event(event) else {
                continue;
            };
            let Err(send_error) = tx.send_timeout(event, SEND_TIMEOUT).await else {
                continue;
            };
            match send_error {
                SendTimeoutError::Closed(_) => {
                    info!("Slot event receiver dropped, exiting websocket slot event stream.");
                    break;
                }
                SendTimeoutError::Timeout(_) => {
                    info!(
                        "Timed out sending slot event: stream is not consumed fast enough, \
                         continuing."
                    );
                }
            }
        }
        // `notifications` requires a valid reference to `pubsub_client`, so
        // `notifications` must be dropped before moving `pubsub_client` via
        // `shutdown()`.
        drop(notifications);
        unsubscribe().await;
        pubsub_client.shutdown().await?;
        Ok(())
    });

    (ReceiverStream::new(rx), handle)
}

fn map_websocket_update_to_slot_event(update: SlotUpdate) -> Option<SlotEvent> {
    match update {
        SlotUpdate::FirstShredReceived { slot, .. } => Some(SlotEvent::Start(slot)),
        SlotUpdate::Completed { slot, .. } => Some(SlotEvent::End(slot)),
        _ => None,
    }
}
