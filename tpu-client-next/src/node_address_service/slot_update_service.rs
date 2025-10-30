//! This module provides [`SlotUpdateService`] that is used to get slot
//! updates using provided stream.
use {
    crate::node_address_service::{
        slot_receiver::EstimatedSlot, RecentLeaderSlots, SlotEvent, SlotReceiver,
    },
    futures::StreamExt,
    log::info,
    solana_clock::Slot,
    std::pin::pin,
    thiserror::Error,
    tokio::{sync::watch, task::JoinHandle},
    tokio_util::sync::CancellationToken,
};

/// [`SlotUpdateService`] updates the current slot by subscribing to
/// the slot updates using provided stream.
pub struct SlotUpdateService {
    handle: Option<JoinHandle<Result<(), Error>>>,
    cancel: CancellationToken,
}

impl SlotUpdateService {
    /// Run the [`SlotUpdateService`].
    pub fn run(
        current_slot: Slot,
        slot_update_stream: impl StreamExt<Item = SlotEvent> + Send + 'static,
        cancel: CancellationToken,
    ) -> Result<(SlotReceiver, Self), Error> {
        let mut recent_slots = RecentLeaderSlots::new();
        let (slot_sender, slot_receiver) = watch::channel(EstimatedSlot::Single(current_slot));
        let slot_receiver_clone = slot_receiver.clone();
        let cancel_clone = cancel.clone();

        let main_loop = async move {
            let mut slot_update_stream = pin!(slot_update_stream);
            loop {
                tokio::select! {
                    Some(slot_event) = slot_update_stream.next() => {
                        if slot_event.is_start() {
                            recent_slots.record_slot_start(slot_event.slot());
                        } else {
                            recent_slots.record_slot_end(slot_event.slot());
                        }
                        let estimated_slots = recent_slots.estimate_current_slot();
                        let cached_estimated_slots = *slot_receiver.borrow();
                        if estimated_slots.requires_update(&cached_estimated_slots) {
                            slot_sender.send(estimated_slots).map_err(|_| Error::ChannelClosed)?;
                        }
                    }

                    _ = cancel.cancelled() => {
                        info!("LeaderTracker cancelled, exiting slot watcher.");
                        break;
                    }
                }
            }
            Ok(())
        };

        let handle = tokio::spawn(main_loop);

        Ok((
            SlotReceiver::new(slot_receiver_clone),
            Self {
                handle: Some(handle),
                cancel: cancel_clone,
            },
        ))
    }

    /// Shutdown the [`SlotUpdateService`].
    pub async fn shutdown(&mut self) -> Result<(), Error> {
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            handle.await??;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    JoinError(#[from] tokio::task::JoinError),

    #[error("Unexpectly dropped a channel.")]
    ChannelClosed,

    #[error("Failed to initialize WebsocketSlotUpdateService.")]
    InitializationFailed,
}
