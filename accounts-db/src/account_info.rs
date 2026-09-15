//! AccountInfo represents a reference to AccountSharedData in an AccountsFile
//! AccountInfo is not persisted anywhere between program runs.
//! AccountInfo is purely runtime state.
//! Note that AccountInfo is saved to disk buckets during runtime, but disk buckets are recreated at startup.
use {
    crate::{
        accounts_db::AccountsFileId,
        accounts_index::{DiskIndexValue, IndexValue},
        is_zero_lamport::IsZeroLamport,
    },
    modular_bitfield::prelude::*,
};

/// offset within an accounts file to account data
pub type Offset = u32;
pub const MAX_OFFSET: Offset = (1 << 31) - 1;

/// specify where account data is located
#[derive(Debug, PartialEq, Eq)]
pub enum StorageLocation {
    AccountsFile(AccountsFileId, Offset),
}

impl StorageLocation {
    pub fn is_offset_equal(&self, other: &StorageLocation) -> bool {
        match self {
            StorageLocation::AccountsFile(_, offset) => match other {
                StorageLocation::AccountsFile(_, other_offset) => other_offset == offset,
            },
        }
    }
    pub fn is_store_id_equal(&self, other: &StorageLocation) -> bool {
        match self {
            StorageLocation::AccountsFile(store_id, _) => match other {
                StorageLocation::AccountsFile(other_store_id, _) => other_store_id == store_id,
            },
        }
    }
}

#[bitfield(bits = 32)]
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct PackedOffsetAndFlags {
    /// logical offset of an account in an accounts storage file
    /// this provides 2^31 bits, which when multiplied by 8 (sizeof(u64)) = 16G, which is the maximum size of an append vec
    offset: B31,
    /// use 1 bit to specify that the entry is zero lamport
    is_zero_lamport: bool,
}

#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
pub struct AccountInfo {
    store_id: AccountsFileId,
    account_offset_and_flags: PackedOffsetAndFlags,
}

// Ensure the size of AccountInfo never changes unexpectedly
const _: () = assert!(size_of::<AccountInfo>() == 8);

impl IsZeroLamport for AccountInfo {
    fn is_zero_lamport(&self) -> bool {
        self.account_offset_and_flags.is_zero_lamport()
    }
}

impl IndexValue for AccountInfo {}

impl DiskIndexValue for AccountInfo {}

impl AccountInfo {
    pub fn new(storage_location: StorageLocation, is_zero_lamport: bool) -> Self {
        let mut packed_offset_and_flags = PackedOffsetAndFlags::default();
        let store_id = match storage_location {
            StorageLocation::AccountsFile(store_id, offset) => {
                assert!(offset <= MAX_OFFSET, "illegal offset");
                packed_offset_and_flags.set_offset(offset);
                store_id
            }
        };
        packed_offset_and_flags.set_is_zero_lamport(is_zero_lamport);
        Self {
            store_id,
            account_offset_and_flags: packed_offset_and_flags,
        }
    }

    pub fn store_id(&self) -> AccountsFileId {
        self.store_id
    }

    pub fn offset(&self) -> Offset {
        self.account_offset_and_flags.offset()
    }

    pub fn storage_location(&self) -> StorageLocation {
        StorageLocation::AccountsFile(self.store_id, self.offset())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_limits() {
        for offset in [0, 1, MAX_OFFSET - 1, MAX_OFFSET] {
            let info = AccountInfo::new(StorageLocation::AccountsFile(0, offset), true);
            assert_eq!(info.offset(), offset);
        }
    }

    #[test]
    #[should_panic(expected = "illegal offset")]
    fn test_offset_too_large() {
        let offset = MAX_OFFSET + 1;
        AccountInfo::new(StorageLocation::AccountsFile(0, offset), true);
    }
}
