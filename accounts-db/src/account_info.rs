//! AccountInfo represents a reference to AccountSharedData in either an AppendVec or the write cache.
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
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        sync::atomic::{AtomicU64, Ordering},
    },
};

/// offset within an append vec to account data
pub type Offset = usize;

/// specify where account data is located
#[derive(Debug, PartialEq, Eq)]
pub enum StorageLocation {
    AppendVec(AccountsFileId, Offset),
    Cached,
}

impl StorageLocation {
    pub fn is_offset_equal(&self, other: &StorageLocation) -> bool {
        match self {
            StorageLocation::Cached => {
                matches!(other, StorageLocation::Cached) // technically, 2 cached entries match in offset
            }
            StorageLocation::AppendVec(_, offset) => {
                match other {
                    StorageLocation::Cached => {
                        false // 1 cached, 1 not
                    }
                    StorageLocation::AppendVec(_, other_offset) => other_offset == offset,
                }
            }
        }
    }
    pub fn is_store_id_equal(&self, other: &StorageLocation) -> bool {
        match self {
            StorageLocation::Cached => {
                matches!(other, StorageLocation::Cached) // 2 cached entries are same store id
            }
            StorageLocation::AppendVec(store_id, _) => {
                match other {
                    StorageLocation::Cached => {
                        false // 1 cached, 1 not
                    }
                    StorageLocation::AppendVec(other_store_id, _) => other_store_id == store_id,
                }
            }
        }
    }
}

/// how large the offset we store in AccountInfo is
/// Note this is a smaller datatype than 'Offset'
/// AppendVecs store accounts aligned to u64, so offset is always a multiple of 8 (sizeof(u64))
pub type OffsetReduced = u32;

/// This is an illegal value for 'offset'.
/// Account size on disk would have to be pointing to the very last 8 byte value in the max sized append vec.
/// That would mean there was a maximum size of 8 bytes for the last entry in the append vec.
/// A pubkey alone is 32 bytes, so there is no way for a valid offset to be this high of a value.
/// Realistically, a max offset is (1<<31 - 156) bytes or so for an account with zero data length. Of course, this
/// depends on the layout on disk, compression, etc. But, 8 bytes per account will never be possible.
/// So, we use this last value as a sentinel to say that the account info refers to an entry in the write cache.
const CACHED_OFFSET: OffsetReduced = (1 << (OffsetReduced::BITS - 1)) - 1;

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

impl IsZeroLamport for AccountInfo {
    fn is_zero_lamport(&self) -> bool {
        self.account_offset_and_flags.is_zero_lamport()
    }
}

impl IsCached for AccountInfo {
    fn is_cached(&self) -> bool {
        self.account_offset_and_flags.offset_reduced() == CACHED_OFFSET
    }
}

impl IndexValue for AccountInfo {}

impl DiskIndexValue for AccountInfo {}

impl IsCached for StorageLocation {
    fn is_cached(&self) -> bool {
        matches!(self, StorageLocation::Cached)
    }
}

/// We have to have SOME value for store_id when we are cached
const CACHE_VIRTUAL_STORAGE_ID: AccountsFileId = AccountsFileId::MAX;

impl AccountInfo {
    pub fn new(storage_location: StorageLocation, is_zero_lamport: bool) -> Self {
        let mut packed_offset_and_flags = PackedOffsetAndFlags::default();
        let store_id = match storage_location {
            StorageLocation::AppendVec(store_id, offset) => {
                let reduced_offset = Self::get_reduced_offset(offset);
                assert_ne!(
                    CACHED_OFFSET, reduced_offset,
                    "illegal offset for non-cached item"
                );
                packed_offset_and_flags.set_offset_reduced(Self::get_reduced_offset(offset));
                assert_eq!(
                    Self::reduced_offset_to_offset(packed_offset_and_flags.offset_reduced()),
                    offset,
                    "illegal offset"
                );
                store_id
            }
            StorageLocation::Cached => {
                packed_offset_and_flags.set_offset_reduced(CACHED_OFFSET);
                CACHE_VIRTUAL_STORAGE_ID
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
        // if the account is in a cached store, the store_id is meaningless
        assert!(!self.is_cached());
        self.store_id
    }

    pub fn offset(&self) -> Offset {
        Self::reduced_offset_to_offset(self.account_offset_and_flags.offset_reduced())
    }

    pub fn reduced_offset_to_offset(reduced_offset: OffsetReduced) -> Offset {
        (reduced_offset as Offset) * ALIGN_BOUNDARY_OFFSET
    }

    pub fn storage_location(&self) -> StorageLocation {
        if self.is_cached() {
            StorageLocation::Cached
        } else {
            StorageLocation::AppendVec(self.store_id, self.offset())
        }
    }
}

#[derive(Debug)]
pub struct IrregularEntry {
    pub foo: u64,
    pub bar: u32,
    pub baz: u64,
}

impl IrregularEntry {
    pub fn from_regular(regular: RegularEntry) -> Self {
        Self {
            foo: regular.file_id() as u64,
            bar: regular.offset_reduced() as u32,
            baz: 1,
        }
    }
}

#[bitfield(bits = 64)]
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct RegularEntry {
    pub common_info: CommonEntryInfo,
    /// Bits available for future use
    pub reserved: B9,
    /// Storage id, capped at 2^20-1, since maximum number of storages is 1000000
    pub file_id: B20,
    pub offset_reduced: B31,
    pub is_zero_lamports: bool,
}

#[bitfield(bits = 3)]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq, BitfieldSpecifier)]
pub struct CommonEntryInfo {
    pub is_regular: bool,
    pub reserved: B2,
}

#[derive(Debug)]
pub enum FixedKindIndexEntry<'a> {
    Regular(RegularEntry),
    Irregular(&'a IrregularEntry),
}

#[bitfield(bits = 64)]
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct IrregularEntryUnsafeAddress {
    pub common_info: CommonEntryInfo,
    /// A non-zero heap memory address holding `IrregularEntry` (i.e. 8-byte aligned struct) down-shifted by 3 bits
    pub shifted_address: B61,
}

pub struct CompactEntryHolder {
    raw_atomic: AtomicU64,
}

impl CompactEntryHolder {
    pub fn from_regular(regular: RegularEntry) -> Self {
        Self::from_unsafe_holder(UnsafeEntryHolder::from_regular(regular))
    }

    pub fn from_irregular(irregular: IrregularEntry) -> Self {
        Self::from_unsafe_holder(UnsafeEntryHolder::from_irregular(irregular))
    }

    pub fn as_fixed_entry(&self) -> FixedKindIndexEntry {
        self.make_unsafe_holder().into_fixed_entry()
    }

    pub fn turn_to_irregular(&self) -> FixedKindIndexEntry {
        let mut current_raw = self.raw_atomic.load(Ordering::Acquire);
        loop {
            let current_holder = UnsafeEntryHolder::from_raw(current_raw);
            match current_holder.into_fixed_entry() {
                FixedKindIndexEntry::Regular(regular) => {
                    let new_holder =
                        UnsafeEntryHolder::from_irregular(IrregularEntry::from_regular(regular));
                    match self.raw_atomic.compare_exchange(
                        current_raw,
                        unsafe { new_holder.raw },
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => return new_holder.into_fixed_entry(),
                        Err(different_raw) => {
                            // entry that we created and tried to store will not be used, so deallocate it
                            new_holder.deallocate_irregular();
                            // continue loop to re-check if the entry is still regular
                            current_raw = different_raw
                        }
                    }
                }
                e @ FixedKindIndexEntry::Irregular(_) => return e,
            }
        }
    }

    fn make_unsafe_holder(&self) -> UnsafeEntryHolder {
        UnsafeEntryHolder::from_raw(self.raw_atomic.load(Ordering::Relaxed))
    }

    fn from_unsafe_holder(holder: UnsafeEntryHolder) -> Self {
        Self {
            raw_atomic: AtomicU64::new(unsafe { holder.raw }),
        }
    }
}

impl Drop for CompactEntryHolder {
    fn drop(&mut self) {
        self.make_unsafe_holder().deallocate_irregular();
    }
}

union UnsafeEntryHolder {
    raw: u64,
    regular: RegularEntry,
    irregular: IrregularEntryUnsafeAddress,
}

impl UnsafeEntryHolder {
    const ALIGNED_PTR_SHIFT: u64 = 3;

    pub fn from_raw(raw: u64) -> Self {
        Self { raw }
    }

    pub fn from_regular(regular: RegularEntry) -> Self {
        assert!(regular.common_info().is_regular());
        Self { regular }
    }

    pub fn from_irregular(irregular: IrregularEntry) -> Self {
        let irregular_ptr = Box::into_raw(Box::new(irregular));
        let irregular = IrregularEntryUnsafeAddress::new()
            .with_common_info(CommonEntryInfo::new().with_is_regular(false))
            .with_shifted_address(Self::make_ptr_shifted_address(irregular_ptr));
        Self { irregular }
    }

    pub fn is_regular(&self) -> bool {
        unsafe { self.regular.common_info().is_regular() }
    }

    pub fn into_fixed_entry<'a>(self) -> FixedKindIndexEntry<'a> {
        unsafe {
            if self.is_regular() {
                FixedKindIndexEntry::Regular(self.regular)
            } else {
                let ptr = self.get_irregular_ptr();
                FixedKindIndexEntry::Irregular(ptr.as_ref().unwrap())
            }
        }
    }

    fn deallocate_irregular(self) {
        if !self.is_regular() {
            unsafe {
                let ptr = self.get_irregular_mut_ptr();
                drop(Box::from_raw(ptr));
            }
        }
    }

    fn make_ptr_shifted_address(ptr: *mut IrregularEntry) -> u64 {
        (ptr as u64) >> Self::ALIGNED_PTR_SHIFT
    }

    fn get_irregular_ptr(&self) -> *const IrregularEntry {
        unsafe {
            assert!(!self.is_regular());
            let addr = self.irregular.shifted_address() << Self::ALIGNED_PTR_SHIFT;
            addr as *const IrregularEntry
        }
    }

    fn get_irregular_mut_ptr(&self) -> *mut IrregularEntry {
        unsafe {
            assert!(!self.is_regular());
            let addr = self.irregular.shifted_address() << Self::ALIGNED_PTR_SHIFT;
            addr as *mut IrregularEntry
        }
    }
}

#[derive(Default)]
pub struct EntryMap {
    pub map: HashMap<Pubkey, CompactEntryHolder>,
}

#[cfg(test)]
mod test {
    use {super::*, crate::append_vec::MAXIMUM_APPEND_VEC_FILE_SIZE};

    #[test]
    fn test_new() {
        let regular = RegularEntry::new()
            .with_common_info(CommonEntryInfo::new().with_is_regular(true))
            .with_file_id(4534)
            .with_offset_reduced(99)
            .with_is_zero_lamports(true);
        let uni_e = CompactEntryHolder::from_regular(regular);

        let irregular = IrregularEntry {
            foo: 42,
            bar: 45,
            baz: 13,
        };
        let _uni_p = CompactEntryHolder::from_irregular(irregular);
        // assert_eq!(
        //     format!(
        //         "e={:?} p={:?}",
        //         uni_e.as_fixed_entry(),
        //         uni_p.as_fixed_entry()
        //     ),
        //     ""
        // );
        assert_eq!(format!("t={:?}", uni_e.turn_to_irregular()), "");

        let _map = EntryMap::default();
    }

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
    fn test_illegal_offset() {
        let offset = (MAXIMUM_APPEND_VEC_FILE_SIZE - (ALIGN_BOUNDARY_OFFSET as u64)) as Offset;
        AccountInfo::new(StorageLocation::AppendVec(0, offset), true);
    }

    #[test]
    #[should_panic(expected = "illegal offset")]
    fn test_alignment() {
        let offset = 1; // not aligned
        AccountInfo::new(StorageLocation::AppendVec(0, offset), true);
    }
}
