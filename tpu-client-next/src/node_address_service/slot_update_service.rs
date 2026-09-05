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
                        if estimated_slots > cached_estimated_slot {
                            cached_estimated_slot = estimated_slots;
                            if slot_sender.send(estimated_slots).is_err() {
                                info!("Stop SlotUpdateService: all slot receivers have been dropped.");
                                break;
                            }
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        futures::{SinkExt, channel::mpsc},
        std::time::Duration,
        tokio::time,
    };

    /// Run a `SlotUpdateService` over `events`, feeding them one at a time with
    /// backpressure so every published value on the watch channel is observed.
    async fn collect_published_slots(
        events: Vec<SlotEvent>,
    ) -> (Vec<Slot>, SlotUpdateService) {
        let cancel = CancellationToken::new();
        // Capacity 1: each `send().await` completes only after the service has
        // consumed the previous event, so each event is processed in turn.
        let (mut tx, rx) = mpsc::channel(1);
        let (mut slot_receiver, service) = SlotUpdateService::run(0, rx, cancel.clone())
            .expect("SlotUpdateService should run");

        let mut published = Vec::new();
        for event in events {
            tx.send(event).await.expect("service dropped the channel");
            // Wait a short window for the service to publish an updated estimate.
            // `changed()` resolves immediately if the value changed since the
            // last observation; otherwise it times out.
            match time::timeout(Duration::from_millis(100), slot_receiver.changed()).await {
                Ok(Ok(())) => published.push(slot_receiver.slot()),
                Ok(Err(_)) => break, // channel closed
                Err(_) => {}         // no update for this event
            }
        }

        (published, service)
    }

    fn backward_estimate_events() -> Vec<SlotEvent> {
        let mut events = Vec::new();
        // 1. Consecutive start events advance the estimate to 300.
        for slot in 1..=300 {
            events.push(SlotEvent::Start(slot));
        }
        // 2. A far-future start event within MAX_SLOT_SKIP_DISTANCE of the median
        //    is adopted as the estimate and published: 332.
        events.push(SlotEvent::Start(332));
        // 3. Exactly 48 events with lower slots evict 332 from the 48-event window.
        //    The raw estimate falls back to 297, below the last published 332.
        for slot in 250..=297 {
            events.push(SlotEvent::Start(slot));
        }
        // 4. The next event yields an estimate of 298: above the evicted cache
        //    (297) but below the last published slot (332). Without the send
        //    guard protecting the cache, this would be delivered as a backward
        //    slot update.
        events.push(SlotEvent::Start(298));
        events
    }

    #[tokio::test]
    async fn test_published_slots_are_monotonic() {
        let (published, mut service) =
            collect_published_slots(backward_estimate_events()).await;

        assert_eq!(
            published.first(),
            Some(&1),
            "expected first published slot to be the first recorded start event"
        );
        let mut last = 0;
        for slot in &published {
            assert!(*slot >= last, "slot went backward: {slot} < {last}");
            last = *slot;
        }

        service.shutdown().await.expect("clean shutdown");
    }

    #[tokio::test]
    async fn test_eviction_drops_estimate_without_regressing_published_slot() {
        let (published, mut service) =
            collect_published_slots(backward_estimate_events()).await;

        let max = published.iter().copied().max().expect("published slots");
        assert_eq!(max, 332, "far-future estimate should have been published");
        // The last published value must remain the max; no backward delivery.
        assert_eq!(*published.last().unwrap(), 332);

        service.shutdown().await.expect("clean shutdown");
    }
}
