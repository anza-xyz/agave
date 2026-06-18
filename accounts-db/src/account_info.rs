//! AccountInfo represents a reference to AccountSharedData in an AppendVec
//! AccountInfo is not persisted anywhere between program runs.
//! AccountInfo is purely runtime state.
//! Note that AccountInfo is saved to disk buckets during runtime, but disk buckets are recreated at startup.
use {
    crate::{
        accounts_db::AccountsFileId,
        accounts_file::ALIGN_BOUNDARY_OFFSET,
        accounts_index::{DiskIndexValue, IndexValue, IsCached},
        is_zero_lamport::IsZeroLamport,
    },
    modular_bitfield::prelude::*,
};

/// offset within an append vec to account data
pub type Offset = usize;

/// specify where account data is located
#[derive(Debug, PartialEq, Eq)]
pub enum StorageLocation {
    AppendVec(AccountsFileId, Offset),
}

impl StorageLocation {
    pub fn is_offset_equal(&self, other: &StorageLocation) -> bool {
        match self {
            StorageLocation::AppendVec(_, offset) => match other {
                StorageLocation::AppendVec(_, other_offset) => other_offset == offset,
            },
        }
    }
    pub fn is_store_id_equal(&self, other: &StorageLocation) -> bool {
        match self {
            StorageLocation::AppendVec(store_id, _) => match other {
                StorageLocation::AppendVec(other_store_id, _) => other_store_id == store_id,
            },
        }
    }
}

/// how large the offset we store in AccountInfo is
/// Note this is a smaller datatype than 'Offset'
/// AppendVecs store accounts aligned to u64, so offset is always a multiple of 8 (sizeof(u64))
pub type OffsetReduced = u32;

#[bitfield(bits = 32)]
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct PackedOffsetAndFlags {
    /// this provides 2^31 bits, which when multiplied by 8 (sizeof(u64)) = 16G, which is the maximum size of an append vec
    offset_reduced: B31,
    /// use 1 bit to specify that the entry is zero lamport
    is_zero_lamport: bool,
}

#[derive(Default, Debug, PartialEq, Eq, Clone, Copy)]
pub struct AccountInfo {
    /// index identifying the append storage
    store_id: AccountsFileId,

    /// offset = 'packed_offset_and_flags.offset_reduced()' * ALIGN_BOUNDARY_OFFSET into the storage
    /// Note this is a smaller type than 'Offset'
    account_offset_and_flags: PackedOffsetAndFlags,
}

// Ensure the size of AccountInfo never changes unexpectedly
const _: () = assert!(size_of::<AccountInfo>() == 8);

impl IsZeroLamport for AccountInfo {
    fn is_zero_lamport(&self) -> bool {
        self.account_offset_and_flags.is_zero_lamport()
    }
}

impl IsCached for AccountInfo {
    fn is_cached(&self) -> bool {
        false
    }
}

impl IndexValue for AccountInfo {}

impl DiskIndexValue for AccountInfo {}

impl AccountInfo {
    pub fn new(storage_location: StorageLocation, is_zero_lamport: bool) -> Self {
        let mut packed_offset_and_flags = PackedOffsetAndFlags::default();
        let store_id = match storage_location {
            StorageLocation::AppendVec(store_id, offset) => {
                packed_offset_and_flags.set_offset_reduced(Self::get_reduced_offset(offset));
                assert_eq!(
                    Self::reduced_offset_to_offset(packed_offset_and_flags.offset_reduced()),
                    offset,
                    "illegal offset"
                );
                store_id
            }
        };
        packed_offset_and_flags.set_is_zero_lamport(is_zero_lamport);
        Self {
            store_id,
            account_offset_and_flags: packed_offset_and_flags,
        }
    }

    pub fn get_reduced_offset(offset: usize) -> OffsetReduced {
        (offset / ALIGN_BOUNDARY_OFFSET) as OffsetReduced
    }

    pub fn store_id(&self) -> AccountsFileId {
        self.store_id
    }

    pub fn offset(&self) -> Offset {
        Self::reduced_offset_to_offset(self.account_offset_and_flags.offset_reduced())
    }

    pub fn reduced_offset_to_offset(reduced_offset: OffsetReduced) -> Offset {
        (reduced_offset as Offset) * ALIGN_BOUNDARY_OFFSET
    }

    pub fn storage_location(&self) -> StorageLocation {
        StorageLocation::AppendVec(self.store_id, self.offset())
    }
}

#[cfg(test)]
mod test {
    use {super::*, crate::append_vec::MAXIMUM_APPEND_VEC_FILE_SIZE};

    #[test]
    fn test_limits() {
        for offset in [
            // MAXIMUM_APPEND_VEC_FILE_SIZE is too big. That would be an offset at the first invalid byte in the max file size.
            // MAXIMUM_APPEND_VEC_FILE_SIZE - 8 bytes would reference the very last 8 bytes in the file size. It makes no sense to reference that since element sizes are always more than 8.
            // MAXIMUM_APPEND_VEC_FILE_SIZE - 16 bytes would reference the second to last 8 bytes in the max file size. This is still likely meaningless, but it is 'valid' as far as the index
            // is concerned.
            (MAXIMUM_APPEND_VEC_FILE_SIZE - 2 * (ALIGN_BOUNDARY_OFFSET as u64)) as Offset,
            0,
            ALIGN_BOUNDARY_OFFSET,
            4 * ALIGN_BOUNDARY_OFFSET,
        ] {
            let info = AccountInfo::new(StorageLocation::AppendVec(0, offset), true);
            assert!(info.offset() == offset);
        }
    }

    #[test]
    #[should_panic(expected = "illegal offset")]
    fn test_alignment() {
        let offset = 1; // not aligned
        AccountInfo::new(StorageLocation::AppendVec(0, offset), true);
    }
}
