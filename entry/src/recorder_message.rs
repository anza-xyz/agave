//! Message types emitted by the PoH recording pipeline.
//!
//! This module defines `RecorderMessage`, a wrapper type that allows regular entries, block markers,
//! and slot lifecycle notifications to flow through the same PoH recording channel.
use crate::{block_component::VersionedBlockMarker, entry::Entry};

/// A message emitted by the PoH recorder for an entry, block marker, or control notification.
///
/// The PoH recorder uses this type to stream entries, block markers, and control notifications
/// through a unified channel to downstream consumers, e.g., broadcast stage.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum RecorderMessage {
    /// The working bank for a new slot has been installed; tell broadcast to wake up.
    SlotStart,
    /// A regular entry containing transactions and/or ticks
    Entry(Entry),
    /// A block metadata marker (header or footer)
    Marker(VersionedBlockMarker),
}

#[cfg(feature = "dev-context-only-utils")]
impl RecorderMessage {
    pub fn unwrap_entry(self) -> Entry {
        match self {
            Self::SlotStart => panic!("Attempting to unwrap slot start as entry"),
            Self::Entry(e) => e,
            Self::Marker(marker) => panic!("Attempting to unwrap marker as entry {marker:?}"),
        }
    }
}

/// Converts an Entry into a RecorderMessage.
impl From<Entry> for RecorderMessage {
    fn from(entry: Entry) -> Self {
        RecorderMessage::Entry(entry)
    }
}

/// Converts a VersionedBlockMarker into a RecorderMessage.
impl From<VersionedBlockMarker> for RecorderMessage {
    fn from(marker: VersionedBlockMarker) -> Self {
        RecorderMessage::Marker(marker)
    }
}
