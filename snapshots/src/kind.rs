use solana_clock::Slot;

/// All snapshots also write archives at this time, but it is possible that
/// in the future there could be other kinds of snapshots that do not write archives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotKind {
    Archive(SnapshotArchiveKind),
}

impl SnapshotKind {
    pub fn is_full_snapshot(&self) -> bool {
        matches!(self, SnapshotKind::Archive(SnapshotArchiveKind::Full))
    }
    pub fn is_incremental_snapshot(&self) -> bool {
        matches!(
            self,
            SnapshotKind::Archive(SnapshotArchiveKind::Incremental(_))
        )
    }
}

/// Archives come in two kinds, Full and Incremental. The incremental archive has a Slot field,
/// which is the incremental archive base slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotArchiveKind {
    Full,
    Incremental(Slot),
}
