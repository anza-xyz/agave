#[cfg(feature = "dev-context-only-utils")]
use solana_hash::Hash;
use {
    crate::bank::{Bank, BankFieldsToSerialize, BankHashStats, BankSlotDelta},
    agave_snapshots::{snapshot_hash::SnapshotHash, SnapshotArchiveKind, SnapshotKind},
    solana_accounts_db::accounts_db::AccountStorageEntry,
    solana_clock::Slot,
    std::{
        sync::{atomic::Ordering, Arc},
        time::Instant,
    },
};

mod compare;
pub use compare::*;

/// This struct packages up fields to send to SnapshotPackagerService
pub struct SnapshotPackage {
    pub snapshot_kind: SnapshotKind,
    pub slot: Slot,
    pub block_height: Slot,
    pub hash: SnapshotHash,
    pub snapshot_storages: Vec<Arc<AccountStorageEntry>>,
    pub status_cache_slot_deltas: Vec<BankSlotDelta>,
    pub bank_fields_to_serialize: BankFieldsToSerialize,
    pub bank_hash_stats: BankHashStats,
    pub write_version: u64,

    /// The instant this snapshot package was sent to the queue.
    /// Used to track how long snapshot packages wait before handling.
    pub enqueued: Instant,
}

impl SnapshotPackage {
    pub fn new(
        snapshot_kind: SnapshotKind,
        bank: &Bank,
        snapshot_storages: Vec<Arc<AccountStorageEntry>>,
        status_cache_slot_deltas: Vec<BankSlotDelta>,
    ) -> Self {
        let slot = bank.slot();
        if let SnapshotKind::Archive(SnapshotArchiveKind::Incremental(
            incremental_snapshot_base_slot,
        )) = snapshot_kind
        {
            assert!(
                slot > incremental_snapshot_base_slot,
                "Incremental snapshot base slot must be less than the bank being snapshotted!"
            );
        }

        let bank_fields_to_serialize = bank.get_fields_to_serialize();
        Self {
            snapshot_kind,
            slot,
            block_height: bank.block_height(),
            hash: SnapshotHash::new(bank_fields_to_serialize.accounts_lt_hash.0.checksum()),
            snapshot_storages,
            status_cache_slot_deltas,
            bank_fields_to_serialize,
            bank_hash_stats: bank.get_bank_hash_stats(),
            write_version: bank
                .rc
                .accounts
                .accounts_db
                .write_version
                .load(Ordering::Acquire),
            enqueued: Instant::now(),
        }
    }
}

#[cfg(feature = "dev-context-only-utils")]
impl SnapshotPackage {
    /// Create a new SnapshotPackage where basically every field is defaulted.
    /// Only use for tests; many of the fields are invalid!
    pub fn default_for_tests() -> Self {
        Self {
            snapshot_kind: SnapshotKind::Archive(SnapshotArchiveKind::Full),
            slot: Slot::default(),
            block_height: Slot::default(),
            hash: SnapshotHash(Hash::default()),
            snapshot_storages: Vec::default(),
            status_cache_slot_deltas: Vec::default(),
            bank_fields_to_serialize: BankFieldsToSerialize::default_for_tests(),
            bank_hash_stats: BankHashStats::default(),
            write_version: u64::default(),
            enqueued: Instant::now(),
        }
    }
}

impl std::fmt::Debug for SnapshotPackage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotPackage")
            .field("kind", &self.snapshot_kind)
            .field("slot", &self.slot)
            .field("block_height", &self.block_height)
            .finish_non_exhaustive()
    }
}
