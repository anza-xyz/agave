use {
    solana_accounts_db::{
        account_storage_entry::AccountStorageEntry, accounts_db::AccountsFileId,
        accounts_file::AccountsFile,
    },
    solana_clock::Slot,
    std::sync::Arc,
    wincode::{SchemaRead, SchemaWrite},
};

/// Identifies a storage belonging to a bank snapshot.
///
/// The file is local-only (never archived) and gated behind `SNAPSHOT_FASTBOOT_VERSION`, so the
/// on-disk encoding can use `AccountsFileId` (`u32`) directly without worrying about
/// forward-compat with potential future widening.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, SchemaRead, SchemaWrite)]
pub enum StorageListItem {
    AppendVec {
        id: AccountsFileId,
        slot: Slot,
    },
    Split {
        has_data_file: bool,
        id: AccountsFileId,
        slot: Slot,
    },
}

const _: () = assert!(size_of::<StorageListItem>() == 16);

/// On-disk format of the storages list file.
///
/// Written next to the bank snapshot on graceful exit to record which storages in the account
/// run dirs make up the snapshot; read on startup to prune anything else before fastboot.
#[derive(Debug, SchemaRead, SchemaWrite)]
pub struct StoragesList {
    pub items: Vec<StorageListItem>,
}

impl StoragesList {
    pub fn new_from_storages(snapshot_storages: &[Arc<AccountStorageEntry>]) -> Self {
        let items = snapshot_storages
            .iter()
            .map(|storage| {
                let slot = storage.slot();
                let id = storage.id();
                match &storage.accounts {
                    AccountsFile::AppendVec(_) => StorageListItem::AppendVec { slot, id },
                    AccountsFile::Split(split) => StorageListItem::Split {
                        slot,
                        id,
                        has_data_file: split.data_file().is_some(),
                    },
                }
            })
            .collect();
        Self { items }
    }
}

impl From<LegacyStoragesList> for StoragesList {
    fn from(legacy: LegacyStoragesList) -> StoragesList {
        let items = legacy
            .items
            .into_iter()
            .map(|legacy_item| StorageListItem::AppendVec {
                slot: legacy_item.slot,
                id: legacy_item.id,
            })
            .collect();
        StoragesList { items }
    }
}

/// Legacy storage list item format, used before fastboot v5.0.0
#[repr(C)]
#[derive(Debug, SchemaRead, SchemaWrite)]
pub struct LegacyStorageListItem {
    pub slot: Slot,
    pub id: AccountsFileId,
}

#[derive(Debug, SchemaRead, SchemaWrite)]
pub struct LegacyStoragesList {
    pub items: Vec<LegacyStorageListItem>,
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
            items: vec![
                StorageListItem::AppendVec { slot: 0, id: 0 },
                StorageListItem::AppendVec { slot: 7, id: 42 },
                StorageListItem::Split {
                    slot: 11,
                    id: 13,
                    has_data_file: false,
                },
                StorageListItem::Split {
                    slot: Slot::MAX,
                    id: AccountsFileId::MAX,
                    has_data_file: true,
                },
            ],
        };

        let mut buf = Vec::new();
        serialize_into(Cursor::new(&mut buf), &list).unwrap();

        let decoded: StoragesList = deserialize_wincode_from(Cursor::new(&buf)).unwrap();
        assert_eq!(decoded.items, list.items);
    }

    #[test]
    fn test_from_legacy_storages_list() {
        let legacy_list = LegacyStoragesList {
            items: vec![
                LegacyStorageListItem { slot: 7, id: 42 },
                LegacyStorageListItem { slot: 11, id: 13 },
                LegacyStorageListItem {
                    slot: Slot::MAX,
                    id: AccountsFileId::MAX,
                },
            ],
        };

        let mut buf = Vec::new();
        serialize_into(Cursor::new(&mut buf), &legacy_list).unwrap();

        let expected_current: Vec<_> = legacy_list
            .items
            .iter()
            .map(|item| StorageListItem::AppendVec {
                slot: item.slot,
                id: item.id,
            })
            .collect();

        let decoded: LegacyStoragesList = deserialize_wincode_from(Cursor::new(&buf)).unwrap();
        let actual_current = StoragesList::from(decoded);
        assert_eq!(actual_current.items, expected_current);
    }
}
