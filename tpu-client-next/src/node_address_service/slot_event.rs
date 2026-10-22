//! This module provides [`SlotEvent`] enum that encapsulates slot start and end events.
//!
//! The implementation of the slot update provider defines semantics of these events. But typically,
//! `SlotEvent::Start` means `FirstShredReceived`` while `SlotEvent::End` means `Completed``.
use solana_clock::Slot;

/// [`SlotEvent`] represents slot start and end events.
#[derive(Debug, Clone)]
pub enum SlotEvent {
    Start(Slot),
    End(Slot),
}

impl SlotEvent {
    /// Get the slot associated with the event.
    pub fn slot(&self) -> Slot {
        match self {
            SlotEvent::Start(slot) | SlotEvent::End(slot) => *slot,
        }
    }

    /// Check if the event is a start event.
    pub fn is_start(&self) -> bool {
        matches!(self, SlotEvent::Start(_))
    }
}
