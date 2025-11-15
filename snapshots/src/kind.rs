use solana_clock::Slot;

/// Snapshots come in two kinds, Full and Incremental.  The IncrementalSnapshot has a Slot field,
/// which is the incremental snapshot base slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotKind {
    FullSnapshot,
    IncrementalSnapshot(Slot),
}

impl SnapshotKind {
    pub fn is_full_snapshot(&self) -> bool {
        matches!(self, SnapshotKind::FullSnapshot)
    }
    pub fn is_incremental_snapshot(&self) -> bool {
        matches!(self, SnapshotKind::IncrementalSnapshot(_))
    }
}

/// Archives come in two kinds, Full and Incremental. The incremental archive has a Slot field,
/// which is the incremental archive base slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotArchiveKind {
    Full,
    Incremental(Slot),
}

impl TryFrom<SnapshotKind> for SnapshotArchiveKind {
    type Error = ();

    fn try_from(snapshot_kind: SnapshotKind) -> Result<Self, Self::Error> {
        match snapshot_kind {
            SnapshotKind::FullSnapshot => Ok(SnapshotArchiveKind::Full),
            SnapshotKind::IncrementalSnapshot(slot) => Ok(SnapshotArchiveKind::Incremental(slot)),
        }
    }
}
