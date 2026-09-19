//! This module provides [`SlotUpdateService`] that is used to get slot updates using provided
//! stream.
use {
    crate::{
        leader_updater::SlotEstimate,
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
        let initial_slot_estimate = SlotEstimate {
            slot: initial_current_slot,
            leader_window_end_ms: None,
        };
        let (slot_sender, slot_receiver) = watch::channel(initial_slot_estimate);
        let cancel_clone = cancel.clone();

        let main_loop = async move {
            let mut slot_update_stream = pin!(slot_update_stream);
            let mut cached_estimate = initial_slot_estimate;
            loop {
                tokio::select! {
                    slot_event = slot_update_stream.next() => {
                        let Some(slot_event) = slot_event else {
                            info!("Slot update stream closed, exiting slot watcher.");
                            break;
                        };
                        recent_slots.record(slot_event);
                        let estimated_slot = recent_slots.estimate_slot();
                        if should_publish(&estimated_slot, &cached_estimate)
                        {
                            if slot_sender.send(estimated_slot).is_err() {
                                info!("Stop SlotUpdateService: all slot receivers have been dropped.");
                                break;
                            }
                            cached_estimate = estimated_slot;
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

/// Returns whether to publish the next estimate: the slot advances or the same slot's estimated
/// window end changes. Ignores older slots and unchanged estimates.
fn should_publish(current: &SlotEstimate, previous: &SlotEstimate) -> bool {
    current.slot > previous.slot
        || (current.slot == previous.slot
            && current.leader_window_end_ms != previous.leader_window_end_ms)
}

#[cfg(test)]
mod tests {
    use {super::*, futures::channel::mpsc, std::time::Duration, tokio::time::timeout};

    #[tokio::test]
    async fn test_publishes_timing_changes_for_same_slot() {
        let (event_sender, event_receiver) = mpsc::unbounded();
        let (mut receiver, mut service) =
            SlotUpdateService::run(7, event_receiver, CancellationToken::new()).unwrap();

        let start = |slot, timestamp| SlotEvent::Start { slot, timestamp };
        let end = |slot, timestamp| SlotEvent::End { slot, timestamp };
        let cases = [
            // End(7) advances to slot 8 with fallback timing at 350 ms per slot.
            (end(7, 1750), 8, 3150),
            // Start(8) refines the window start without advancing the slot.
            (start(8, 1800), 8, 3200),
            // A delayed Start(7) refines the duration to 400 ms, still in slot 8.
            (start(7, 1400), 8, 3400),
            // Advancing the slot must also publish when the timing is unchanged.
            (end(8, 2150), 9, 3400),
        ];

        for (event, slot, leader_window_end_ms) in cases {
            event_sender.unbounded_send(event).unwrap();
            timeout(Duration::from_secs(1), receiver.changed())
                .await
                .expect("slot estimate was not published")
                .unwrap();
            assert_eq!(
                receiver.slot(),
                SlotEstimate {
                    slot,
                    leader_window_end_ms: Some(leader_window_end_ms),
                },
            );
        }

        service.shutdown().await.unwrap();
    }
}
