//! This module provides [`SlotUpdateService`] that is used to get slot updates using provided
//! stream.
use {
    crate::{
        logging::info,
        node_address_service::{RecentLeaderSlots, SlotEvent, SlotReceiver},
    },
    futures::StreamExt,
    solana_clock::Slot,
    std::pin::pin,
    thiserror::Error,
    tokio::{sync::watch, task::JoinHandle},
    tokio_util::sync::CancellationToken,
};

/// [`SlotUpdateService`] updates the current slot by subscribing to the slot updates using provided
/// stream.
pub struct SlotUpdateService {
    handle: Option<JoinHandle<Result<(), Error>>>,
    cancel: CancellationToken,
}

impl SlotUpdateService {
    /// Run the [`SlotUpdateService`].
    pub fn run(
        initial_current_slot: Slot,
        slot_update_stream: impl StreamExt<Item = SlotEvent> + Send + 'static,
        cancel: CancellationToken,
    ) -> Result<(SlotReceiver, Self), Error> {
        let mut recent_slots = RecentLeaderSlots::new();
        let (slot_sender, slot_receiver) = watch::channel(initial_current_slot);
        let cancel_clone = cancel.clone();

        let main_loop = async move {
            let mut slot_update_stream = pin!(slot_update_stream);
            let mut cached_estimated_slot = initial_current_slot;
            loop {
                tokio::select! {
                    Some(slot_event) = slot_update_stream.next() => {
                        recent_slots.record(slot_event);
                        let estimated_slots = recent_slots.estimate_current_slot();
                        // Send update only if the estimated slot has advanced.
                        if estimated_slots > cached_estimated_slot && slot_sender.send(estimated_slots).is_err() {
                            info!("Stop SlotUpdateService: all slot receivers have been dropped.");
                            break;
                        }
                        cached_estimated_slot = estimated_slots;
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
            SlotReceiver::new(slot_receiver),
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

    #[error("Failed to initialize WebsocketSlotUpdateService.")]
    InitializationFailed,
}
