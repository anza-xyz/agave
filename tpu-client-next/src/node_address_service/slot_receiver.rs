//! This module provides [`EstimatedSlot`] along with [`SlotReceiver`].
use {solana_clock::Slot, thiserror::Error, tokio::sync::watch};

/// [`EstimatedSlot`] represents either a single estimated slot or multiple. In
/// case of uncertanty about current slot when we received FirstShred from two
/// consequent slots without receving Processed, it will contain two slots in
/// sorted order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EstimatedSlot {
    Single(Slot),
    Multiple([Slot; 2]),
}

impl EstimatedSlot {
    pub fn as_slice(&self) -> &[Slot] {
        match self {
            Self::Single(s) => std::slice::from_ref(s),
            Self::Multiple(arr) => arr,
        }
    }

    /// Returns true if the current estimated slot is newer that the prvevious
    /// one.
    pub fn requires_update(&self, previous: &EstimatedSlot) -> bool {
        match (self, previous) {
            (EstimatedSlot::Single(current_slot), EstimatedSlot::Single(prev_slot)) => {
                current_slot > prev_slot
            }
            (EstimatedSlot::Single(s1), EstimatedSlot::Multiple([prev_slot1, _])) => {
                s1 > prev_slot1
            }
            (EstimatedSlot::Multiple([_, cur_slot2]), EstimatedSlot::Single(prev_slot)) => {
                cur_slot2 > prev_slot
            }
            (EstimatedSlot::Multiple([cur_slot1, _]), EstimatedSlot::Multiple([prev_slot1, _])) => {
                cur_slot1 > prev_slot1
            }
        }
    }

    /// Returns the first slot in the estimated slots.
    pub fn first_slot(&self) -> Slot {
        match self {
            EstimatedSlot::Single(slot) => *slot,
            EstimatedSlot::Multiple([slot, _]) => *slot,
        }
    }
}

/// Receiver for slot updates from slot update services.
#[derive(Clone)]
pub struct SlotReceiver {
    receiver: watch::Receiver<EstimatedSlot>,
}

impl SlotReceiver {
    pub fn new(receiver: watch::Receiver<EstimatedSlot>) -> Self {
        Self { receiver }
    }

    pub fn slot(&self) -> EstimatedSlot {
        *self.receiver.borrow()
    }

    pub async fn changed(&mut self) -> Result<(), SlotReceiverError> {
        self.receiver
            .changed()
            .await
            .map_err(|_| SlotReceiverError::ChannelClosed)
    }
}

#[derive(Debug, Error)]
pub enum SlotReceiverError {
    #[error("Unexpectly dropped a channel.")]
    ChannelClosed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimated_slot_requires_update() {
        use EstimatedSlot::*;

        let s1 = 10;
        let s2 = 11;
        let s3 = 12;

        assert_eq!(Single(s2).requires_update(&Multiple([s1, s2])), true);

        assert_eq!(Multiple([s1, s2]).requires_update(&Single(s1)), true);

        assert_eq!(
            Multiple([s1, s2]).requires_update(&Multiple([s1, s2])),
            false
        );

        assert_eq!(
            Multiple([s2, s3]).requires_update(&Multiple([s1, s2])),
            true
        );

        assert_eq!(Single(s1).requires_update(&Single(s1)), false);

        assert_eq!(Single(s2).requires_update(&Single(s1)), true);

        assert_eq!(Single(s1).requires_update(&Single(s2)), false);

        assert_eq!(Single(s1).requires_update(&Multiple([s1, s2])), false);

        assert_eq!(Multiple([s1, s2]).requires_update(&Single(s2)), false);

        assert_eq!(Single(s3).requires_update(&Multiple([s1, s2])), true);
    }
}
