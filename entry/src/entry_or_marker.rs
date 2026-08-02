//! Entry marker types for the PoH recording pipeline.
//!
//! This module defines `EntryOrMarker`, a wrapper type that allows both regular entries and block
//! markers (headers, footers) to flow through the same PoH recording channel.
use crate::{block_component::VersionedBlockMarker, entry::Entry};

/// Wraps either a regular entry or a block metadata marker.
///
/// The PoH recorder uses this type to stream both transaction-containing entries and block markers
/// through a unified channel to downstream consumers, e.g., broadcast stage.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum EntryOrMarker {
    /// A regular entry containing transactions and/or ticks
    Entry(Entry),
    /// A block metadata marker (header or footer)
    Marker(VersionedBlockMarker),
}

#[cfg(feature = "dev-context-only-utils")]
impl EntryOrMarker {
    pub fn unwrap_entry(self) -> Entry {
        match self {
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
