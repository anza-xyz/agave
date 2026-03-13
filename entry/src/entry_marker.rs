//! Entry marker types for the PoH recording pipeline.
//!
//! This module defines `EntryMarker`, a wrapper type that allows both regular entries and block
//! markers (headers, footers) to flow through the same PoH recording channel.
use crate::{block_component::VersionedBlockMarker, entry::Entry};

/// Wraps either a regular entry or a block metadata marker.
///
/// The PoH recorder uses this type to stream both transaction-containing entries and block markers
/// through a unified channel to downstream consumers, e.g., broadcast stage.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum EntryMarker {
    /// A regular entry containing transactions and/or ticks
    Entry(Entry),
    /// A block metadata marker (header or footer)
    Marker(VersionedBlockMarker),
}

impl EntryMarker {
    /// Consumes self and returns the inner Entry if this is an Entry variant.
    ///
    /// Returns `None` if this is a Marker variant.
    pub fn into_entry(self) -> Option<Entry> {
        match self {
            EntryMarker::Entry(entry) => Some(entry),
            _ => None,
        }
    }

    /// Returns a reference to the inner Entry if this is an Entry variant.
    ///
    /// Returns `None` if this is a Marker variant.
    pub fn as_entry(&self) -> Option<&Entry> {
        match self {
            EntryMarker::Entry(entry) => Some(entry),
            _ => None,
        }
    }

    /// Consumes self and returns the inner VersionedBlockMarker if this is a Marker variant.
    ///
    /// Returns `None` if this is an Entry variant.
    pub fn into_marker(self) -> Option<VersionedBlockMarker> {
        match self {
            EntryMarker::Marker(marker) => Some(marker),
            _ => None,
        }
    }

    /// Returns a reference to the inner VersionedBlockMarker if this is a Marker variant.
    ///
    /// Returns `None` if this is an Entry variant.
    pub fn as_marker(&self) -> Option<&VersionedBlockMarker> {
        match self {
            EntryMarker::Marker(marker) => Some(marker),
            _ => None,
        }
    }
}

/// Converts an Entry into an EntryMarker.
impl From<Entry> for EntryMarker {
    fn from(entry: Entry) -> Self {
        EntryMarker::Entry(entry)
    }
}

/// Converts a VersionedBlockMarker into an EntryMarker.
impl From<VersionedBlockMarker> for EntryMarker {
    fn from(marker: VersionedBlockMarker) -> Self {
        EntryMarker::Marker(marker)
    }
}
