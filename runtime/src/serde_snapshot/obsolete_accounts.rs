use {
    crate::serde_snapshot::SerializedAccountsFileId,
    dashmap::DashMap,
    rayon::iter::{IntoParallelIterator, ParallelIterator},
    serde::Serialize,
    solana_accounts_db::{
        ObsoleteAccountItem, ObsoleteAccounts, account_info::Offset,
        account_storage_entry::AccountStorageEntry, accounts_db::AccountsFileId,
    },
    solana_clock::Slot,
    std::sync::Arc,
    wincode::{SchemaRead, SchemaWrite},
};

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
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
    pub accounts: Vec<(Offset, usize, Slot)>,
}

impl SerdeObsoleteAccounts {
    /// Creates a new `SerdeObsoleteAccounts` instance from a given storage entry and snapshot slot.
    fn new_from_storage_entry_at_slot(storage: &AccountStorageEntry, snapshot_slot: Slot) -> Self {
        let accounts = storage
            .obsolete_accounts_for_snapshots(snapshot_slot)
            .accounts
            .into_iter()
            .map(|item| (item.offset, item.data_len, item.slot))
            .collect();

        SerdeObsoleteAccounts {
            id: storage.id() as SerializedAccountsFileId,
            bytes: storage.get_obsolete_bytes(Some(snapshot_slot)) as u64,
            accounts,
        }
    }

    pub(crate) fn into_tuple(self) -> (ObsoleteAccounts, AccountsFileId, usize) {
        let accounts = self
            .accounts
            .into_iter()
            .map(|(offset, data_len, slot)| ObsoleteAccountItem {
                offset,
                data_len,
                slot,
            })
            .collect();

        (
            ObsoleteAccounts { accounts },
            self.id as AccountsFileId,
            self.bytes as usize,
        )
    }
}

/// Represents a map of obsolete accounts data for multiple slots.
/// This struct is serialized/deserialized as part of the snapshot process
/// to capture and restore obsolete accounts information for account storages.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "7Hb4XtqaUBY27yyz7V1kVXZ9aTnary8GDroB12JRvGjz")
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

    pub(crate) fn into_dashmap(self) -> DashMap<Slot, SerdeObsoleteAccounts> {
        self.map.into_iter().collect()
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::serde_snapshot::{deserialize_wincode_from, serialize_into},
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
        let obsolete_accounts = DashMap::<Slot, ObsoleteAccounts>::new();
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
            .map(|entry| {
                let accounts = entry
                    .value()
                    .accounts
                    .iter()
                    .map(|item| (item.offset, item.data_len, item.slot))
                    .collect();
                let serde_obsolete_accounts = SerdeObsoleteAccounts {
                    id: *entry.key() as SerializedAccountsFileId,
                    bytes: num_obsolete_accounts_per_storage as u64 * 1000,
                    accounts,
                };
                (*entry.key(), serde_obsolete_accounts)
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
        let map = deserialized_obsolete_accounts.into_dashmap();

        // Verify the deserialized data matches the original obsolete accounts
        assert_eq!(map.len(), obsolete_accounts.len());
        for (slot, obsolete_accounts) in obsolete_accounts {
            let deserialized_obsolete_accounts = map.remove(&slot).unwrap();
            assert_eq!(
                obsolete_accounts,
                deserialized_obsolete_accounts.1.into_tuple().0
            );
        }
    }
}
