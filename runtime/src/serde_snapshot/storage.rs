use {
    serde::{Deserialize, Serialize},
    wincode::{SchemaRead, SchemaWrite},
};

/// The serialized AccountsFileId type is fixed as usize
pub(crate) type SerializedAccountsFileId = usize;

// Serializable version of AccountStorageEntry, no longer written nor read back; only kept to name
// the wire shape of the deprecated storage entries map in the snapshot manifest.
#[repr(C)]
#[cfg_attr(
    feature = "frozen-abi",
    derive(StableAbi, StableAbiSample),
    frozen_abi(
        abi_digest = "CMckX3HiC6K5FSmFo4tH44wU1mvGfabNtYAs65uaGvGU",
        abi_serializer = ["bincode", "wincode"],
        test_roundtrip = "eq_and_wire"
    )
)]
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, SchemaRead, SchemaWrite,
)]
pub struct SerializableAccountStorageEntry {
    id: SerializedAccountsFileId,
    accounts_current_len: usize,
}
