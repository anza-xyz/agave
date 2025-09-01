use {
    crate::node_address_service::error::NodeAddressServiceError,
    futures_util::stream::StreamExt,
    log::*,
    solana_clock::{Slot, DEFAULT_MS_PER_SLOT},
    solana_pubsub_client::nonblocking::pubsub_client::PubsubClient,
    solana_rpc_client_api::response::SlotUpdate,
    solana_time_utils::timestamp,
    std::collections::VecDeque,
    tokio::{
        sync::watch,
        time::{interval, Duration, Instant},
    },
    tokio_util::sync::CancellationToken,
};

/// [`WebsocketSlotUpdateService`] updates the current slot by subscribing to
/// the slot updates using websockets.
pub struct WebsocketSlotUpdateService;

impl WebsocketSlotUpdateService {
    pub async fn run(
        start_slot: Slot,
        slot_sender: watch::Sender<(Slot, u64, Duration)>,
        slot_receiver: watch::Receiver<(Slot, u64, Duration)>,
        websocket_url: String,
        cancel: CancellationToken,
    ) -> Result<(), NodeAddressServiceError> {
        assert!(!websocket_url.is_empty(), "Websocket URL must not be empty");
        let mut recent_slots = RecentLeaderSlots::new(start_slot);
        let pubsub_client = PubsubClient::new(&websocket_url).await?;

        let (mut notifications, unsubscribe) = pubsub_client.slot_updates_subscribe().await?;

        // Track the last time a slot update was received. In case of current
        // leader is not sending relevant shreds for some reason, the slot will
        // not update.
        let mut last_slot_time = Instant::now();
        const FALLBACK_SLOT_TIMEOUT: Duration = Duration::from_millis(DEFAULT_MS_PER_SLOT);

        let mut interval = interval(FALLBACK_SLOT_TIMEOUT);

        loop {
            tokio::select! {
                // biased to always prefer slot update over fallback slot injection
                biased;
                maybe_update = notifications.next() => {
                    match maybe_update {
                        Some(update) => {
                            let current_slot = match update {
                                // This update indicates that we have just
                                // received the first shred from the leader for
                                // this slot and they are probably still
                                // accepting transactions.
                                SlotUpdate::FirstShredReceived { slot, .. } => slot,
                                //TODO(klykov): fall back on bank created to use
                                // with solana test validator This update
                                // indicates that a full slot was received by
                                // the connected node so we can stop sending
                                // transactions to the leader for that slot
                                SlotUpdate::Completed { slot, .. } => slot.saturating_add(1),
                                _ => continue,
                            };
                            recent_slots.record_slot(current_slot);
                            last_slot_time = Instant::now();
                            let cached_estimated_slot = slot_receiver.borrow().0;
                            let estimated_slot = recent_slots.estimate_current_slot();
                            if cached_estimated_slot < estimated_slot {
                                slot_sender.send((estimated_slot, timestamp(), Duration::from_millis(DEFAULT_MS_PER_SLOT)))
                                    .expect("Failed to send slot update");
                            }
                        }
                        None => continue,
                    }
                }

                _ = interval.tick(), if last_slot_time.elapsed() > FALLBACK_SLOT_TIMEOUT => {
                    let estimated = recent_slots.estimate_current_slot().saturating_add(1);
                    info!("Injecting fallback slot {estimated}");
                    recent_slots.record_slot(estimated);
                    last_slot_time = Instant::now();
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

        Ok(())
    }
}

// 48 chosen because it's unlikely that 12 leaders in a row will miss their slots
const MAX_SLOT_SKIP_DISTANCE: u64 = 48;

#[derive(Debug)]
pub(crate) struct RecentLeaderSlots(VecDeque<Slot>);
impl RecentLeaderSlots {
    pub(crate) fn new(current_slot: Slot) -> Self {
        let mut recent_slots = VecDeque::new();
        recent_slots.push_back(current_slot);
        Self(recent_slots)
    }

    pub(crate) fn record_slot(&mut self, current_slot: Slot) {
        self.0.push_back(current_slot);
        // 12 recent slots should be large enough to avoid a misbehaving
        // validator from affecting the median recent slot
        while self.0.len() > 12 {
            self.0.pop_front();
        }
    }

    // Estimate the current slot from recent slot notifications.
    #[allow(clippy::arithmetic_side_effects)]
    pub(crate) fn estimate_current_slot(&self) -> Slot {
        let mut recent_slots: Vec<Slot> = self.0.iter().cloned().collect();
        assert!(!recent_slots.is_empty());
        recent_slots.sort_unstable();
        debug!("Recent slots: {:?}", recent_slots);

        // Validators can broadcast invalid blocks that are far in the future
        // so check if the current slot is in line with the recent progression.
        let max_index = recent_slots.len() - 1;
        let median_index = max_index / 2;
        let median_recent_slot = recent_slots[median_index];
        let expected_current_slot = median_recent_slot + (max_index - median_index) as u64;
        let max_reasonable_current_slot = expected_current_slot + MAX_SLOT_SKIP_DISTANCE;

        // Return the highest slot that doesn't exceed what we believe is a
        // reasonable slot.
        let res = recent_slots
            .into_iter()
            .rev()
            .find(|slot| *slot <= max_reasonable_current_slot)
            .unwrap();
        debug!(
            "expected_current_slot: {}, max reasonable current slot: {}, found: {}",
            expected_current_slot, max_reasonable_current_slot, res
        );

        res
    }
}

#[cfg(test)]
impl From<Vec<Slot>> for RecentLeaderSlots {
    fn from(recent_slots: Vec<Slot>) -> Self {
        assert!(!recent_slots.is_empty());
        Self(recent_slots.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_clock::Slot};

    fn assert_slot(recent_slots: RecentLeaderSlots, expected_slot: Slot) {
        assert_eq!(recent_slots.estimate_current_slot(), expected_slot);
    }

    #[test]
    fn test_recent_leader_slots() {
        assert_slot(RecentLeaderSlots::new(0), 0);

        let mut recent_slots: Vec<Slot> = (1..=12).collect();
        assert_slot(RecentLeaderSlots::from(recent_slots.clone()), 12);

        recent_slots.reverse();
        assert_slot(RecentLeaderSlots::from(recent_slots), 12);

        assert_slot(
            RecentLeaderSlots::from(vec![0, 1 + MAX_SLOT_SKIP_DISTANCE]),
            1 + MAX_SLOT_SKIP_DISTANCE,
        );
        assert_slot(
            RecentLeaderSlots::from(vec![0, 2 + MAX_SLOT_SKIP_DISTANCE]),
            0,
        );

        assert_slot(RecentLeaderSlots::from(vec![1]), 1);
        assert_slot(RecentLeaderSlots::from(vec![1, 100]), 1);
        assert_slot(RecentLeaderSlots::from(vec![1, 2, 100]), 2);
        assert_slot(RecentLeaderSlots::from(vec![1, 2, 3, 100]), 3);
        assert_slot(RecentLeaderSlots::from(vec![1, 2, 3, 99, 100]), 3);
    }
}
