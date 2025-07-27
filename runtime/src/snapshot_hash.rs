//! Helper types and functions for handling and dealing with snapshot hashes.
use {
    solana_clock::Slot, solana_hash::Hash,
    solana_lattice_hash::lt_hash::Checksum as AccountsLtHashChecksum,
};

/// At startup, when loading from snapshots, the starting snapshot hashes need to be passed to
/// SnapshotPackagerService, which is in charge of pushing the hashes to CRDS.  This struct wraps
/// up those values make it easier to pass from bank_forks_utils, through validator, to
/// SnapshotPackagerService.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartingSnapshotHashes {
    pub full: FullSnapshotHash,
    pub incremental: Option<IncrementalSnapshotHash>,
}

/// Used by SnapshotPackagerService and SnapshotGossipManager, this struct adds type safety to
/// ensure a full snapshot hash is pushed to the right CRDS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullSnapshotHash(pub (Slot, SnapshotHash));

/// Used by SnapshotPackagerService and SnapshotGossipManager, this struct adds type safety to
/// ensure an incremental snapshot hash is pushed to the right CRDS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncrementalSnapshotHash(pub (Slot, SnapshotHash));

/// The hash used for snapshot archives
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct SnapshotHash(pub Hash);

impl SnapshotHash {
    /// Make a snapshot hash from accounts hashes
    #[must_use]
    pub fn new(accounts_lt_hash_checksum: AccountsLtHashChecksum) -> Self {
        let accounts_hash = Hash::new_from_array(accounts_lt_hash_checksum.0);
        Self(accounts_hash)
    }
}
