use std::num::NonZeroU64;

/// The interval in between taking snapshots
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum SnapshotInterval {
    /// Snapshots are disabled
    Disabled,
    /// Snapshots are taken every this many slots
    Slots(NonZeroU64),
}
