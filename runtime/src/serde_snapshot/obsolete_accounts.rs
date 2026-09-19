use {
    crate::serde_snapshot::SerializedAccountsFileId,
    rayon::iter::{IntoParallelIterator, ParallelIterator},
    serde::Serialize,
    solana_accounts_db::{
        ObsoleteAccountItem, ObsoleteAccounts, account_storage_entry::AccountStorageEntry,
        accounts_db::AccountsFileId, append_vec_logical_offset_from_file,
    },
    solana_clock::Slot,
    std::{collections::HashMap, io, sync::Arc},
    wincode::{SchemaRead, SchemaWrite},
};

#[repr(C)]
#[cfg_attr(feature = "frozen-abi", derive(StableAbi, StableAbiSample))]
#[derive(Debug, SchemaRead, SchemaWrite, Serialize)]
pub struct SerdeObsoleteAccountItem {
    /// Logical offset of the account in the account storage entry
    pub offset: u32,
    /// Length of the account data
    pub data_len: usize,
    /// Slot when the account was marked obsolete
    pub slot: Slot,
}

#[cfg_attr(feature = "frozen-abi", derive(StableAbi, StableAbiSample))]
#[derive(Debug, Default, Serialize, SchemaRead, SchemaWrite)]
pub(crate) struct SerdeObsoleteAccounts {
    /// The ID of the associated account file. Used for verification to ensure the restored
    /// obsolete accounts correspond to the correct account file
    pub id: SerializedAccountsFileId,
    /// The number of obsolete bytes in the storage. These bytes are removed during archive
    /// serialization/deserialization but are present when restoring from directories. This value
    /// is used to validate the size when creating the accounts file.
    pub bytes: u64,
    /// A list of accounts that are obsolete in the storage being restored.
    pub accounts: Vec<SerdeObsoleteAccountItem>,
}

impl SerdeObsoleteAccounts {
    /// Creates a new `SerdeObsoleteAccounts` instance from a given storage entry and snapshot slot.
    fn new_from_storage_entry_at_slot(storage: &AccountStorageEntry, snapshot_slot: Slot) -> Self {
        let accounts = Self::items_from_obsolete_accounts(
            storage.obsolete_accounts_for_snapshots(snapshot_slot),
        );

        SerdeObsoleteAccounts {
            id: storage.id() as SerializedAccountsFileId,
            bytes: storage.get_obsolete_bytes(Some(snapshot_slot)) as u64,
            accounts,
        }
    }

    /// Converts this SerdeObsoleteAccounts into its corresponding non-serde types.
    pub(crate) fn into_tuple(self) -> (ObsoleteAccounts, AccountsFileId, usize) {
        let accounts = self
            .accounts
            .into_iter()
            .map(|item| ObsoleteAccountItem {
                offset: item.offset,
                data_len: item.data_len,
                slot: item.slot,
            })
            .collect();

        (
            ObsoleteAccounts { accounts },
            self.id as AccountsFileId,
            self.bytes as usize,
        )
    }

    fn items_from_obsolete_accounts(
        obsolete_accounts: ObsoleteAccounts,
    ) -> Vec<SerdeObsoleteAccountItem> {
        obsolete_accounts
            .accounts
            .into_iter()
            .map(|item| SerdeObsoleteAccountItem {
                offset: item.offset,
                data_len: item.data_len,
                slot: item.slot,
            })
            .collect()
    }
}

/// Represents a map of obsolete accounts data for multiple slots.
/// This struct is serialized/deserialized as part of the snapshot process
/// to capture and restore obsolete accounts information for account storages.
#[cfg_attr(
    feature = "frozen-abi",
    derive(StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "7i8BA2maHm88aetwcauQQAkqVNjotFyEg65GgyAjzU3y",
        abi_serializer = "wincode"
    )
)]
#[derive(Serialize, Debug, SchemaRead, SchemaWrite)]
pub(crate) struct SerdeObsoleteAccountsMap {
    map: Vec<(Slot, SerdeObsoleteAccounts)>,
}

impl SerdeObsoleteAccountsMap {
    /// Creates a new `SerdeObsoleteAccountsMap` from a list of storage entries and a snapshot slot.
    pub(crate) fn new_from_storages(
        snapshot_storages: &[Arc<AccountStorageEntry>],
        snapshot_slot: Slot,
    ) -> Self {
        let map = snapshot_storages
            .into_par_iter()
            .map(|storage| {
                (
                    storage.slot(),
                    SerdeObsoleteAccounts::new_from_storage_entry_at_slot(storage, snapshot_slot),
                )
            })
            .collect();
        SerdeObsoleteAccountsMap { map }
    }

    pub(crate) fn into_hashmap(self) -> HashMap<Slot, SerdeObsoleteAccounts> {
        self.map.into_iter().collect()
    }
}

/// Fastboot v2/v3 stored the obsolete accounts' offsets as u64, which are append vec file offsets.
/// We support loading from v2/v3, and thus must support deserialization of the legacy format.
#[repr(C)]
#[derive(SchemaRead)]
struct LegacyObsoleteAccountItem {
    offset: u64,
    data_len: usize,
    slot: Slot,
}

#[derive(SchemaRead)]
struct LegacyObsoleteAccounts {
    id: SerializedAccountsFileId,
    bytes: u64,
    accounts: Vec<LegacyObsoleteAccountItem>,
}

#[derive(SchemaRead)]
pub(crate) struct LegacyObsoleteAccountsMap {
    map: Vec<(Slot, LegacyObsoleteAccounts)>,
}

impl TryFrom<LegacyObsoleteAccountsMap> for SerdeObsoleteAccountsMap {
    type Error = io::Error;

    fn try_from(legacy: LegacyObsoleteAccountsMap) -> Result<Self, Self::Error> {
        let map = legacy
            .map
            .into_iter()
            .map(|(slot, storage)| {
                let accounts = storage
                    .accounts
                    .into_iter()
                    .map(|item| {
                        let offset =
                            append_vec_logical_offset_from_file(item.offset).ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    format!(
                                        "invalid logical offset from file offset: {}",
                                        item.offset,
                                    ),
                                )
                            })?;
                        Ok(SerdeObsoleteAccountItem {
                            offset,
                            data_len: item.data_len,
                            slot: item.slot,
                        })
                    })
                    .collect::<io::Result<Vec<_>>>()?;
                Ok((
                    slot,
                    SerdeObsoleteAccounts {
                        id: storage.id,
                        bytes: storage.bytes,
                        accounts,
                    },
                ))
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self { map })
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::serde_snapshot::{deserialize_wincode_from, serialize_into},
        solana_accounts_db::account_info::Offset,
        std::io::Cursor,
        test_case::test_case,
    };

    /// Tests the serialization and deserialization of obsolete accounts
    #[test_case(0, 0)]
    #[test_case(1, 0)]
    #[test_case(10, 15)]
    fn test_serialize_and_deserialize_obsolete_accounts(
        num_storages: u64,
        num_obsolete_accounts_per_storage: usize,
    ) {
        // Create a set of obsolete accounts
        let mut obsolete_accounts = HashMap::<Slot, ObsoleteAccounts>::new();
        for slot in 1..=num_storages {
            let obsolete_accounts_list = ObsoleteAccounts {
                accounts: (0..num_obsolete_accounts_per_storage)
                    .map(|j| ObsoleteAccountItem {
                        offset: j as Offset,
                        data_len: j * 10,
                        slot: slot + 1,
                    })
                    .collect(),
            };

            obsolete_accounts.insert(slot, obsolete_accounts_list);
        }

        // Convert the obsolete accounts into a SerdeObsoleteAccountsMap
        let map = obsolete_accounts
            .iter()
            .map(|(slot, accounts)| {
                let serde_obsolete_accounts = SerdeObsoleteAccounts {
                    id: *slot as SerializedAccountsFileId,
                    bytes: num_obsolete_accounts_per_storage as u64 * 1000,
                    accounts: SerdeObsoleteAccounts::items_from_obsolete_accounts(accounts.clone()),
                };
                (*slot, serde_obsolete_accounts)
            })
            .collect();
        let obsolete_accounts_map = SerdeObsoleteAccountsMap { map };

        // Serialize the obsolete accounts map
        let mut buf = Vec::new();
        serialize_into(Cursor::new(&mut buf), &obsolete_accounts_map).unwrap();

        // Deserialize the obsolete accounts map
        let cursor = Cursor::new(buf.as_slice());
        let deserialized_obsolete_accounts: SerdeObsoleteAccountsMap =
            deserialize_wincode_from(cursor).unwrap();
        let mut map = deserialized_obsolete_accounts.into_hashmap();

        // Verify the deserialized data matches the original obsolete accounts
        assert_eq!(map.len(), obsolete_accounts.len());
        for (slot, obsolete_accounts) in obsolete_accounts {
            let deserialized_obsolete_accounts = map.remove(&slot).unwrap();
            assert_eq!(
                obsolete_accounts,
                deserialized_obsolete_accounts.into_tuple().0
            );
        }
    }

    #[test]
    fn test_legacy_obsolete_accounts_try_from_empty() {
        let legacy = LegacyObsoleteAccountsMap { map: vec![] };
        let current = SerdeObsoleteAccountsMap::try_from(legacy).unwrap();
        assert!(current.map.is_empty());
    }

    #[test]
    fn test_legacy_obsolete_accounts_try_from_ok() {
        let legacy = LegacyObsoleteAccountsMap {
            map: vec![
                (
                    10,
                    LegacyObsoleteAccounts {
                        id: 42,
                        bytes: 408,
                        accounts: vec![
                            LegacyObsoleteAccountItem {
                                offset: 0,
                                data_len: 0,
                                slot: 11,
                            },
                            LegacyObsoleteAccountItem {
                                offset: 8 * 11,
                                data_len: 5,
                                slot: 12,
                            },
                            LegacyObsoleteAccountItem {
                                offset: 8 * 123,
                                data_len: 99,
                                slot: 13,
                            },
                        ],
                    },
                ),
                (
                    20,
                    LegacyObsoleteAccounts {
                        id: 43,
                        bytes: 0,
                        accounts: vec![],
                    },
                ),
            ],
        };
        let current = SerdeObsoleteAccountsMap::try_from(legacy).unwrap();
        assert_eq!(current.map.len(), 2);

        let (slot, serde_obsolete_accounts) = &current.map[0];
        assert_eq!(*slot, 10);
        assert_eq!(serde_obsolete_accounts.id, 42);
        assert_eq!(serde_obsolete_accounts.bytes, 408);
        assert_eq!(
            serde_obsolete_accounts
                .accounts
                .iter()
                .map(|item| item.offset)
                .collect::<Vec<_>>(),
            vec![0, 11, 123],
        );

        let (slot, serde_obsolete_accounts) = &current.map[1];
        assert_eq!(*slot, 20);
        assert_eq!(serde_obsolete_accounts.id, 43);
        assert_eq!(serde_obsolete_accounts.bytes, 0);
        assert!(serde_obsolete_accounts.accounts.is_empty());
    }

    #[test_case(1; "unaligned")]
    #[test_case(1 << 34; "out of range")]
    fn test_legacy_obsolete_accounts_bad_offset(offset: u64) {
        let slot = 10;
        let legacy = LegacyObsoleteAccountsMap {
            map: vec![(
                slot,
                LegacyObsoleteAccounts {
                    id: 42,
                    bytes: 136,
                    accounts: vec![LegacyObsoleteAccountItem {
                        offset,
                        data_len: 0,
                        slot,
                    }],
                },
            )],
        };
        let err = SerdeObsoleteAccountsMap::try_from(legacy).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(
            err.to_string()
                .contains("invalid logical offset from file offset")
        );
    }
}
