//! Provides interfaces for rebuilding snapshot storages

use {
    super::{SnapshotError, SnapshotFrom},
    crate::serde_snapshot::{
        SerdeObsoleteAccounts, SerdeObsoleteAccountsMap, reconstruct_single_storage,
        remap_and_reconstruct_single_storage,
    },
    agave_fs::FileInfo,
    log::*,
    solana_accounts_db::{
        account_storage::AccountStorageMap,
        accounts_db::{AccountsFileId, AtomicAccountsFileId},
    },
    solana_clock::Slot,
    std::{
        collections::HashMap,
        path::PathBuf,
        str::FromStr as _,
        sync::{Arc, atomic::Ordering},
        time::{Duration, Instant},
    },
};

const PROGRESS_LOG_INTERVAL: Duration = Duration::from_secs(2);

/// Stores state for rebuilding snapshot storages
#[derive(Debug)]
pub(crate) struct SnapshotStorageRebuilder {
    /// Snapshot storage lengths - from the snapshot file
    snapshot_storage_lengths: HashMap<Slot, usize>,
    /// Container for storing rebuilt snapshot storages
    storage: AccountStorageMap,
    /// Tracks next append_vec_id
    next_append_vec_id: Arc<AtomicAccountsFileId>,
    /// Tracks the number of collisions in AccountsFileId
    num_collisions: usize,
    /// Number of storage slots that have been rebuilt so far
    processed_slot_count: usize,
    /// Rebuild from the snapshot files or archives
    snapshot_from: SnapshotFrom,
    /// obsolete accounts for all storages
    obsolete_accounts: HashMap<Slot, SerdeObsoleteAccounts>,
}

impl SnapshotStorageRebuilder {
    /// Rebuild snapshot storages on the current thread by consuming `files`.
    pub(crate) fn rebuild_storages(
        snapshot_storage_lengths: HashMap<Slot, usize>,
        files: impl IntoIterator<Item = FileInfo>,
        next_append_vec_id: Arc<AtomicAccountsFileId>,
        snapshot_from: SnapshotFrom,
        obsolete_accounts: Option<SerdeObsoleteAccountsMap>,
    ) -> Result<AccountStorageMap, SnapshotError> {
        let mut rebuilder = Self {
            storage: AccountStorageMap::with_capacity(snapshot_storage_lengths.len()),
            snapshot_storage_lengths,
            next_append_vec_id,
            num_collisions: 0,
            processed_slot_count: 0,
            snapshot_from,
            obsolete_accounts: obsolete_accounts
                .map(|map| map.into_hashmap())
                .unwrap_or_default(),
        };

        let mut last_log_time = Instant::now();

        for file_info in files {
            rebuilder.process_append_vec_file(file_info)?;
            let now = Instant::now();
            if now.duration_since(last_log_time) >= PROGRESS_LOG_INTERVAL {
                rebuilder.log_progress();
                last_log_time = now;
            }
        }

        Ok(rebuilder.storage)
    }

    fn log_progress(&self) {
        info!(
            "rebuilt storages for {}/{} slots with {} collisions",
            self.processed_slot_count,
            self.snapshot_storage_lengths.len(),
            self.num_collisions,
        );
    }

    fn process_append_vec_file(&mut self, file_info: FileInfo) -> Result<(), SnapshotError> {
        let filename = file_info.path.file_name().unwrap().to_str().unwrap();
        if let Ok((slot, append_vec_id)) = get_slot_and_append_vec_id(filename) {
            if self.snapshot_from == SnapshotFrom::Dir {
                // Keep track of the highest append_vec_id in the system, so the future append_vecs
                // can be assigned to unique IDs.  This is only needed when loading from a snapshot
                // dir.  When loading from a snapshot archive, the max of the appendvec IDs is
                // updated in remap_append_vec_file(), which is not in the from_dir route.
                self.next_append_vec_id
                    .fetch_max((append_vec_id + 1) as AccountsFileId, Ordering::Relaxed);
            }
            self.process_complete_slot(slot, file_info)?;
            self.processed_slot_count += 1;
        }
        Ok(())
    }

    /// Process a slot that has received all storage entries
    fn process_complete_slot(
        &mut self,
        slot: Slot,
        file_info: FileInfo,
    ) -> Result<(), SnapshotError> {
        let filename = file_info.path.file_name().unwrap().to_str().unwrap();
        let (_, old_append_vec_id) = get_slot_and_append_vec_id(filename)?;
        let Some(&current_len) = self.snapshot_storage_lengths.get(&slot) else {
            return Err(SnapshotError::RebuildStorages(format!(
                "account storage file '{filename}' for slot outside of expected range in snapshot"
            )));
        };

        let storage_entry = match &self.snapshot_from {
            SnapshotFrom::Archive => remap_and_reconstruct_single_storage(
                slot,
                old_append_vec_id,
                current_len,
                file_info,
                &self.next_append_vec_id,
                &mut self.num_collisions,
            )?,
            SnapshotFrom::Dir => reconstruct_single_storage(
                &slot,
                file_info,
                current_len,
                old_append_vec_id as AccountsFileId,
                self.obsolete_accounts
                    .remove(&slot)
                    .map(|accounts| accounts.into_tuple()),
            )?,
        };

        let storage_id = storage_entry.id();
        if let Some(other) = self.storage.insert(slot, storage_entry) {
            Err(SnapshotError::RebuildStorages(format!(
                "there must be exactly one storage per slot, but slot {slot} has duplicate \
                 storages: {} vs {storage_id}",
                other.id()
            )))
        } else {
            Ok(())
        }
    }
}

/// Get the slot and append vec id from the filename
pub(crate) fn get_slot_and_append_vec_id(filename: &str) -> Result<(Slot, usize), SnapshotError> {
    let mut parts = filename.splitn(2, '.');
    let slot = parts.next().and_then(|s| Slot::from_str(s).ok());
    let id = parts.next().and_then(|s| usize::from_str(s).ok());

    slot.zip(id)
        .ok_or_else(|| SnapshotError::InvalidAppendVecPath(PathBuf::from(filename)))
}

#[cfg(test)]
mod tests {
    use {super::*, solana_accounts_db::accounts_file::AccountsFile};

    #[test]
    fn test_get_slot_and_append_vec_id() {
        let expected_slot = 12345;
        let expected_id = 9987;
        let (slot, id) =
            get_slot_and_append_vec_id(&AccountsFile::file_name(expected_slot, expected_id))
                .unwrap();
        assert_eq!(expected_slot, slot);
        assert_eq!(expected_id as usize, id);
    }
}
