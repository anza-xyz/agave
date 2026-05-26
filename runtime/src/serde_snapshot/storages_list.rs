use {
    solana_accounts_db::{account_storage_entry::AccountStorageEntry, accounts_db::AccountsFileId},
    solana_clock::Slot,
    std::{collections::HashSet, sync::Arc},
    wincode::{SchemaRead, SchemaWrite},
};

/// Identifies a storage file belonging to a bank snapshot, by `(slot, id)`.
///
/// The file is local-only (never archived) and gated behind `SNAPSHOT_FASTBOOT_VERSION`, so the
/// on-disk encoding can use `AccountsFileId` (`u32`) directly without worrying about
/// forward-compat with potential future widening.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub struct StorageListItem {
    pub slot: Slot,
    pub id: AccountsFileId,
}

/// On-disk format of the storages list file.
///
/// Written next to the bank snapshot on graceful exit to record which storages in the account
/// run dirs make up the snapshot; read on startup to prune anything else before fastboot.
#[derive(Debug, SchemaRead, SchemaWrite)]
pub struct StoragesList {
    list: Vec<StorageListItem>,
}

impl StoragesList {
    pub fn new_from_storages(snapshot_storages: &[Arc<AccountStorageEntry>]) -> Self {
        Self::from_items(
            snapshot_storages
                .iter()
                .map(|storage| StorageListItem {
                    slot: storage.slot(),
                    id: storage.id(),
                })
                .collect(),
        )
    }

    /// Build a `StoragesList` from an existing `Vec` of items.
    pub fn from_items(list: Vec<StorageListItem>) -> Self {
        Self { list }
    }

    pub fn into_slot_file_id_set(self) -> HashSet<(Slot, AccountsFileId)> {
        self.list
            .into_iter()
            .map(|item| (item.slot, item.id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::serde_snapshot::{deserialize_wincode_from, serialize_into},
        std::io::Cursor,
    };

    #[test]
    fn test_roundtrip_storages_list() {
        let list = StoragesList {
            list: vec![
                StorageListItem { slot: 7, id: 42 },
                StorageListItem { slot: 11, id: 13 },
                StorageListItem {
                    slot: Slot::MAX,
                    id: AccountsFileId::MAX,
                },
            ],
        };
        let expected_set: HashSet<(Slot, AccountsFileId)> =
            HashSet::from([(7, 42), (11, 13), (Slot::MAX, AccountsFileId::MAX)]);

        let mut buf = Vec::new();
        serialize_into(Cursor::new(&mut buf), &list).unwrap();

        let decoded: StoragesList = deserialize_wincode_from(Cursor::new(&buf)).unwrap();
        assert_eq!(decoded.into_slot_file_id_set(), expected_set);
    }
}
