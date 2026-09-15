//! Entry marker types for the PoH recording pipeline.
//!
//! This module defines `EntryOrMarker`, a wrapper type that allows regular entries, block markers,
//! and slot lifecycle notifications to flow through the same PoH recording channel.
use crate::{block_component::VersionedBlockMarker, entry::Entry};

/// Wraps either a regular entry or a block metadata/control marker.
///
/// The PoH recorder uses this type to stream entries, block markers, and control notifications
/// through a unified channel to downstream consumers, e.g., broadcast stage.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum EntryOrMarker {
    /// The working bank for a new slot has been installed tell broadcast to wake up
    SlotStart,
    /// A regular entry containing transactions and/or ticks
    Entry(Entry),
    /// A block metadata marker (header or footer)
    Marker(VersionedBlockMarker),
}

#[cfg(feature = "dev-context-only-utils")]
impl EntryOrMarker {
    pub fn unwrap_entry(self) -> Entry {
        match self {
            Self::SlotStart => panic!("Attempting to unwrap slot start as entry"),
            Self::Entry(e) => e,
            Self::Marker(marker) => panic!("Attempting to unwrap marker as entry {marker:?}"),
        }
    }
}

/// Converts an Entry into an EntryOrMarker.
impl From<Entry> for EntryOrMarker {
    fn from(entry: Entry) -> Self {
        EntryOrMarker::Entry(entry)
    }
}

/// Converts a VersionedBlockMarker into an EntryOrMarker.
impl From<VersionedBlockMarker> for EntryOrMarker {
    fn from(marker: VersionedBlockMarker) -> Self {
        EntryOrMarker::Marker(marker)
    }
}
