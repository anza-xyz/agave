use {
    crate::node_address_service::{NodeAddressServiceError, SlotReceiver},
    futures_util::stream::StreamExt,
    log::*,
    solana_clock::Slot,
    solana_pubsub_client::nonblocking::pubsub_client::PubsubClient,
    solana_rpc_client_api::response::SlotUpdate,
    solana_time_utils::timestamp,
    std::collections::VecDeque,
    tokio::{sync::watch, task::JoinHandle},
    tokio_util::sync::CancellationToken,
};

/// [`WebsocketSlotUpdateService`] updates the current slot by subscribing to
/// the slot updates using websockets.
pub struct WebsocketSlotUpdateService {
    handle: JoinHandle<Result<(), NodeAddressServiceError>>,
    cancel: CancellationToken,
}

impl WebsocketSlotUpdateService {
    pub fn run(
        current_slot: Slot,
        websocket_url: String,
        cancel: CancellationToken,
    ) -> Result<(SlotReceiver, Self), NodeAddressServiceError> {
        assert!(!websocket_url.is_empty(), "Websocket URL must not be empty");
        let (slot_sender, slot_receiver) = watch::channel((current_slot, timestamp()));
        let slot_receiver_clone = slot_receiver.clone();
        let cancel_clone = cancel.clone();
        let main_loop = async move {
            let mut recent_slots = RecentLeaderSlots::new();
            let pubsub_client = PubsubClient::new(&websocket_url).await?;

            let (mut notifications, unsubscribe) = pubsub_client.slot_updates_subscribe().await?;

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
                                let estimated_slot = recent_slots.estimate_current_slot();
                                let cached_estimated_slot = *slot_receiver.borrow();
                                if cached_estimated_slot.0 < estimated_slot {
                                    slot_sender.send((estimated_slot, timestamp())).map_err(|_| NodeAddressServiceError::ChannelClosed)?;
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
            Ok(())
        };

        let handle = tokio::spawn(main_loop);

        Ok((
            SlotReceiver::new(slot_receiver_clone),
            Self {
                handle,
                cancel: cancel_clone,
            },
        ))
    }

    pub async fn shutdown(self) -> Result<(), NodeAddressServiceError> {
        self.cancel.cancel();
        self.handle.await??;
        Ok(())
    }
}

// 48 chosen because it's unlikely that 12 leaders in a row will miss their slots
const MAX_SLOT_SKIP_DISTANCE: u64 = 48;

const RECENT_LEADER_SLOTS_CAPACITY: usize = 12;

pub trait SlotEstimator {
    fn record_slot(&mut self, current_slot: Slot);
    fn estimate_current_slot(&self) -> Slot;
}

#[derive(Debug)]
pub struct RecentLeaderSlots(VecDeque<Slot>);
impl RecentLeaderSlots {
    pub fn new() -> Self {
        Self(VecDeque::with_capacity(RECENT_LEADER_SLOTS_CAPACITY))
    }
}

impl SlotEstimator for RecentLeaderSlots {
    fn record_slot(&mut self, current_slot: Slot) {
        self.0.push_back(current_slot);
        // 12 recent slots should be large enough to avoid a misbehaving
        // validator from affecting the median recent slot
        while self.0.len() > RECENT_LEADER_SLOTS_CAPACITY {
            self.0.pop_front();
        }
    }

    // Estimate the current slot from recent slot notifications.
    #[allow(clippy::arithmetic_side_effects)]
    fn estimate_current_slot(&self) -> Slot {
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
