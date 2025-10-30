//! This module provides [`WebsocketSlotUpdateService`] that is used to get slot
//! updates via WebSocket interface.
use {
    crate::{
        logging::{error, info},
        node_address_service::{slot_receiver::EstimatedSlot, RecentLeaderSlots, SlotReceiver},
    },
    futures_util::stream::StreamExt,
    solana_clock::Slot,
    solana_pubsub_client::{
        nonblocking::pubsub_client::PubsubClient, pubsub_client::PubsubClientError,
    },
    solana_rpc_client_api::response::SlotUpdate,
    thiserror::Error,
    tokio::{sync::watch, task::JoinHandle},
    tokio_util::sync::CancellationToken,
};

/// [`WebsocketSlotUpdateService`] updates the current slot by subscribing to
/// the slot updates using websockets.
pub struct WebsocketSlotUpdateService {
    handle: Option<JoinHandle<Result<(), WebsocketSlotUpdateServiceError>>>,
    cancel: CancellationToken,
}

impl WebsocketSlotUpdateService {
    /// Run the [`WebsocketSlotUpdateService`].
    pub fn run(
        current_slot: Slot,
        websocket_url: String,
        cancel: CancellationToken,
    ) -> Result<(SlotReceiver, Self), WebsocketSlotUpdateServiceError> {
        assert!(!websocket_url.is_empty(), "Websocket URL must not be empty");
        let mut cached_estimated_slots = EstimatedSlot::Single(current_slot);
        let (slot_sender, slot_receiver) = watch::channel(cached_estimated_slots.clone());
        let cancel_clone = cancel.clone();
        let main_loop = async move {
            let mut recent_slots = RecentLeaderSlots::new();
            let pubsub_client = PubsubClient::new(&websocket_url).await?;
            let mut result = Ok(());
            let (mut notifications, unsubscribe) = pubsub_client.slot_updates_subscribe().await?;
            loop {
                tokio::select! {
                    // biased to always prefer slot update over fallback slot injection
                    biased;
                    maybe_update = notifications.next() => {
                        match maybe_update {
                            Some(update) => {
                                match update {
                                    // This update indicates that we have just
                                    // received the first shred from the leader
                                    // for this slot and they are probably still
                                    // accepting transactions.
                                    SlotUpdate::FirstShredReceived { slot, .. } => recent_slots.record_slot_start(slot),
                                    // This update indicates that a full slot
                                    // was received by the connected node so we
                                    // can stop sending transactions to the
                                    // leader for that slot
                                    SlotUpdate::Completed { slot, .. } => recent_slots.record_slot_end(slot),
                                    _ => continue,
                                }
                                let estimated_slots = recent_slots.estimate_current_slot();
                                if estimated_slots.requires_update(&cached_estimated_slots) {
                                    if let Err(err) = slot_sender.send(estimated_slots).map_err(|_| WebsocketSlotUpdateServiceError::ChannelClosed) {
                                        result = Err(err);
                                        break;
                                    }

                                    cached_estimated_slots = estimated_slots;
                                }
                            }
                            None => continue,
                        }
                    }

                    _ = cancel.cancelled() => {
                        info!("LeaderTracker cancelled, exiting slot watcher.");
                        break;
                    }
                }
            }

            // `notifications` requires a valid reference to `pubsub_client`, so
            // `notifications` must be dropped before moving `pubsub_client` via
            // `shutdown()`.
            drop(notifications);
            unsubscribe().await;
            pubsub_client.shutdown().await?;
            result
        };

        let handle = tokio::spawn(main_loop);

        Ok((
            SlotReceiver::new(slot_receiver),
            Self {
                handle: Some(handle),
                cancel: cancel_clone,
            },
        ))
    }

    /// Shutdown the [`WebsocketSlotUpdateService`].
    pub async fn shutdown(&mut self) -> Result<(), WebsocketSlotUpdateServiceError> {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            handle.await??;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum WebsocketSlotUpdateServiceError {
    #[error(transparent)]
    PubsubError(#[from] PubsubClientError),

    #[error(transparent)]
    JoinError(#[from] tokio::task::JoinError),

    #[error("Unexpectly dropped a channel.")]
    ChannelClosed,

    #[error("Failed to initialize WebsocketSlotUpdateService.")]
    InitializationFailed,
}
