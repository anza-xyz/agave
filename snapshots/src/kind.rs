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
    pub fn includes_archive(&self) -> bool {
        match self {
            SnapshotKind::FullSnapshot => true,
            SnapshotKind::IncrementalSnapshot(_) => true,
        }
    }
}

/// Archives come in two kinds, Full and Incremental.  The IncrementalArchive has a Slot field,
/// which is the incremental archive base slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveKind {
    FullArchive,
    IncrementalArchive(Slot),
}

impl ArchiveKind {
    pub fn is_full_archive(&self) -> bool {
        matches!(self, ArchiveKind::FullArchive)
    }
}
impl TryFrom<SnapshotKind> for ArchiveKind {
    type Error = ();

    fn try_from(snapshot_kind: SnapshotKind) -> Result<Self, Self::Error> {
        match snapshot_kind {
            SnapshotKind::FullSnapshot => Ok(ArchiveKind::FullArchive),
            SnapshotKind::IncrementalSnapshot(slot) => Ok(ArchiveKind::IncrementalArchive(slot)),
        }
    }
}
