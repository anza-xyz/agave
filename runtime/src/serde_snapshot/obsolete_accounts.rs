use {
    crate::serde_snapshot::{deserialize_from, utils, SerializedAccountsFileId},
    bincode::{Error, Options},
    dashmap::DashMap,
    serde::{Deserialize, Serialize},
    solana_accounts_db::{account_info::Offset, accounts_db::AccountStorageEntry},
    solana_clock::Slot,
    std::{
        collections::HashMap,
        io::{BufReader, BufWriter, Read, Write},
    },
};

/// This structure handles the load/store of obsolete accounts during snapshot restoration.
#[cfg_attr(
    feature = "frozen-abi",
    derive(AbiExample),
    frozen_abi(digest = "gdRJ8jL5YUArjpxF46N82NMsZ2NZ9sL3y4DJS6onxat")
)]
#[derive(Debug, Default, Serialize, Deserialize)]
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
    pub fn new_from_storage_entry_at_slot(
        storage: &AccountStorageEntry,
        snapshot_slot: Slot,
    ) -> Self {
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
}

pub(crate) fn serialize_obsolete_accounts<W>(
    stream: &mut BufWriter<W>,
    obsolete_accounts_map: &HashMap<Slot, SerdeObsoleteAccounts>,
) -> Result<(), Error>
where
    W: Write,
{
    let bincode = bincode::DefaultOptions::new().with_fixint_encoding();
    bincode.serialize_into(
        stream,
        &utils::serialize_iter_as_tuple(obsolete_accounts_map.iter()),
    )
}

pub(crate) fn deserialize_obsolete_accounts<R>(
    stream: &mut BufReader<R>,
) -> Result<DashMap<Slot, SerdeObsoleteAccounts>, Error>
where
    R: Read,
{
    let obsolete_accounts = DashMap::default();
    while let Ok((slot, accounts)) = { deserialize_from(&mut *stream) } {
        obsolete_accounts.insert(slot, accounts);
    }

    Ok(obsolete_accounts)
}

#[cfg(test)]
mod test {
    use {
        super::*,
        crate::serde_snapshot::obsolete_accounts::{
            deserialize_obsolete_accounts, serialize_obsolete_accounts,
        },
        std::{
            collections::HashMap,
            io::{BufReader, BufWriter, Cursor},
        },
        test_case::test_case,
    };

    // Test serialization and deserialization of obsolete accounts with various scenarios
    #[test_case(0, 0)]
    #[test_case(1, 0)]
    #[test_case(10, 15)]
    fn test_serialize_and_deserialize_obsolete_accounts(
        num_storages: u64,
        num_obsolete_accounts_per_storage: usize,
    ) {
        // Create a sample obsolete accounts map
        let mut obsolete_accounts_map = HashMap::new();
        for slot in 1..=num_storages {
            let obsolete_accounts = (0..num_obsolete_accounts_per_storage)
                .map(|j| (j, j * 10, slot + 1))
                .collect();

            obsolete_accounts_map.insert(
                slot,
                SerdeObsoleteAccounts {
                    bytes: num_obsolete_accounts_per_storage as u64 * 1000,
                    id: slot as usize,
                    accounts: obsolete_accounts,
                },
            );
        }

        // Serialize the obsolete accounts
        let mut buf = Vec::new();
        let cursor = Cursor::new(&mut buf);
        let mut writer = BufWriter::new(cursor);
        serialize_obsolete_accounts(&mut writer, &obsolete_accounts_map).unwrap();
        drop(writer);

        // Deserialize the obsolete accounts
        let cursor = Cursor::new(buf.as_slice());
        let mut reader = BufReader::new(cursor);
        let deserialized_obsolete_accounts = deserialize_obsolete_accounts(&mut reader).unwrap();

        // Verify the deserialized data matches the original
        assert_eq!(
            deserialized_obsolete_accounts.len(),
            obsolete_accounts_map.len()
        );
        for (slot, obsolete_accounts) in obsolete_accounts_map {
            let deserialized_obsolete_accounts = deserialized_obsolete_accounts.get(&slot).unwrap();
            assert_eq!(
                obsolete_accounts.accounts,
                deserialized_obsolete_accounts.accounts
            );
        }
    }
}
