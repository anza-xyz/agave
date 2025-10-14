use {
    serde::{Deserialize, Serialize},
    solana_accounts_db::{accounts_db::AccountStorageEntry, ObsoleteAccounts},
    solana_clock::Slot,
};

/// The serialized AccountsFileId type is fixed as usize
pub(crate) type SerializedAccountsFileId = usize;

// Serializable version of AccountStorageEntry for snapshot format
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SerializableAccountStorageEntry {
    id: SerializedAccountsFileId,
    accounts_current_len: usize,
}

impl SerializableAccountStorageEntry {
    /// Creates a new SerializableAccountStorageEntry from the current
    /// AccountStorageEntry and a given snapshot slot. When obsolete accounts
    /// are enabled, the saved size is decreased by the amount of obsolete bytes
    /// in the storage. The number of obsolete bytes is determined by the snapshot
    /// slot, as an entry's obsolescence is dependent on the slot that marked it
    /// as such.
    pub fn new(
        accounts: &AccountStorageEntry,
        snapshot_slot: Slot,
    ) -> SerializableAccountStorageEntry {
        SerializableAccountStorageEntry {
            id: accounts.id() as SerializedAccountsFileId,
            accounts_current_len: accounts.accounts.len()
                - accounts.get_obsolete_bytes(Some(snapshot_slot)),
        }
    }
}

pub(crate) trait SerializableStorage {
    fn id(&self) -> SerializedAccountsFileId;
    fn current_len(&self) -> usize;
}

impl SerializableStorage for SerializableAccountStorageEntry {
    fn id(&self) -> SerializedAccountsFileId {
        self.id
    }
    fn current_len(&self) -> usize {
        self.accounts_current_len
    }
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::abi_example::TransparentAsHelper for SerializableAccountStorageEntry {}

/// This structure handles the load/store of obsolete accounts during snapshot restoration.
#[derive(Debug, Default)]
pub(crate) struct SerdeObsoleteAccounts {
    /// The ID of the associated account file. Used for verification to ensure the restored
    /// obsolete accounts correspond to the correct account file
    pub id: SerializedAccountsFileId,
    /// The number of obsolete bytes in the storage. These bytes are removed during archive
    /// serialization/deserialization but are present when restoring from directories. This value
    /// is used to validate the size when creating the accounts file.
    pub bytes: u64,
    /// A list of accounts that are obsolete in the storage being restored.
    pub accounts: ObsoleteAccounts,
}
