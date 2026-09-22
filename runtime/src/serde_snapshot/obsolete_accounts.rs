use {
    crate::serde_snapshot::SerializedAccountsFileId,
    rayon::iter::{IntoParallelIterator, ParallelIterator},
    solana_accounts_db::{
        ObsoleteAccountItem, ObsoleteAccounts, account_storage_entry::AccountStorageEntry,
        accounts_db::AccountsFileId, append_vec_file_offset_from_logical,
        append_vec_logical_offset_from_file,
    },
    solana_clock::Slot,
    std::{collections::HashMap, sync::Arc},
    wincode::{SchemaRead, SchemaWrite},
};

#[repr(C)]
#[cfg_attr(feature = "frozen-abi", derive(StableAbi, StableAbiSample))]
#[derive(Debug, SchemaRead, SchemaWrite)]
pub struct SerdeObsoleteAccountItem {
    /// File offset of the account in the account storage entry
    pub offset: u64,
    /// Length of the account data
    pub data_len: usize,
    /// Slot when the account was marked obsolete
    pub slot: Slot,
}

#[cfg_attr(feature = "frozen-abi", derive(StableAbi, StableAbiSample))]
#[derive(Debug, Default, SchemaRead, SchemaWrite)]
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
            .map(|item| {
                // yes, append vec impl is hard coded here for now
                let Some(offset) = append_vec_logical_offset_from_file(item.offset) else {
                    panic!("invalid logical offset from file offset: {}", item.offset);
                };
                ObsoleteAccountItem {
                    offset,
                    data_len: item.data_len,
                    slot: item.slot,
                }
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
            .map(|item| {
                // yes, append vec impl is hard coded here for now
                let offset = append_vec_file_offset_from_logical(item.offset);
                SerdeObsoleteAccountItem {
                    offset,
                    data_len: item.data_len,
                    slot: item.slot,
                }
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
        abi_digest = "Bzyq9V5sWxtx4EVzcMQiyco1tgfUngC2Zp4YV54HWaD3",
        abi_serializer = "wincode"
    )
)]
#[derive(Debug, SchemaRead, SchemaWrite)]
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

    #[test_case(1; "unaligned")]
    #[test_case(1 << 34; "out of range")]
    #[should_panic(expected = "invalid logical offset from file offset")]
    fn test_serde_obsolete_accounts_into_tuple_bad_offset(offset: u64) {
        let serde_obsolete_accounts = SerdeObsoleteAccounts {
            id: 42,
            bytes: 136,
            accounts: vec![SerdeObsoleteAccountItem {
                offset,
                data_len: 0,
                slot: 10,
            }],
        };
        _ = serde_obsolete_accounts.into_tuple();
    }
}
