use {
    serde::{Deserialize, Serialize},
    solana_accounts_db::account_storage_entry::AccountStorageEntry,
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
    fn current_len(&self) -> usize;
}

impl SerializableStorage for SerializableAccountStorageEntry {
    fn current_len(&self) -> usize {
        self.accounts_current_len
    }
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::abi_example::TransparentAsHelper for SerializableAccountStorageEntry {}
