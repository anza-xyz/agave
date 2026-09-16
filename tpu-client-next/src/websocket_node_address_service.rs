//! This module provides [`WebsocketNodeAddressService`] that is used to get slot
//! updates via WebSocket interface.
use {
    crate::{
        logging::info,
        node_address_service::{
            LeaderTpuCacheServiceConfig, NodeAddressProvider, NodeAddressService,
            NodeAddressServiceError, SlotEvent,
        },
    },
    futures::Stream,
    futures_util::stream::StreamExt,
    solana_pubsub_client::nonblocking::pubsub_client::{PubsubClient, PubsubClientError},
    solana_rpc_client::nonblocking::rpc_client::RpcClient,
    solana_rpc_client_api::{client_error::Error as ClientError, response::SlotUpdate},
    std::{sync::Arc, time::Duration},
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
    /// Run the [`WebsocketNodeAddressService`].
    pub async fn run(
        rpc_client: Arc<RpcClient>,
        websocket_url: String,
        config: LeaderTpuCacheServiceConfig,
        cancel: CancellationToken,
    ) -> Result<(NodeAddressProvider, Self), Error> {
        let (websocket_slot_event_stream, ws_task_handle) =
            websocket_slot_event_stream(websocket_url);
        let (provider, service) =
            NodeAddressService::run(rpc_client, websocket_slot_event_stream, config, cancel)
                .await?;

        Ok((
            provider,
            Self {
                service,
                ws_task_handle: Some(ws_task_handle),
            },
        ))
    }

    pub async fn shutdown(mut self) -> Result<(), Error> {
        self.service.shutdown().await?;
        if let Some(handle) = self.ws_task_handle.take() {
            handle.await??;
        }
        Ok(())
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
