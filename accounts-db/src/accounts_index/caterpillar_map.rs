use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

use modular_bitfield::{bitfield, prelude::*};
use solana_pubkey::Pubkey;

/// Map that can transition entries without locking between small inlined payload into (boxed) values
#[derive(Default)]
pub struct CaterpillarMap {
    pub map: HashMap<Pubkey, CaterpillarEntry>,
}

pub struct CaterpillarEntry {
    /// Raw value - either an inline (small) payload or address of a boxed full value
    raw_atomic: AtomicU64,
}

impl CaterpillarEntry {
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

    pub fn turn_to_regular(&mut self) -> FixedKindIndexEntry {
        let current_raw = self.raw_atomic.load(Ordering::Acquire);
        let current_holder = UnsafeEntryHolder::from_raw(current_raw);
        match current_holder.into_fixed_entry() {
            e @ FixedKindIndexEntry::Regular(_) => e,
            FixedKindIndexEntry::Irregular(irregular) => {
                let new_holder = UnsafeEntryHolder::from_regular(irregular.into_regular());
                // We have exclusive (&mut) access to the entry, so no other code is reading or updating
                // the raw atomic. This means we can just store new value without conflict.
                self.raw_atomic
                    .store(unsafe { new_holder.raw }, Ordering::Release);
                new_holder.into_fixed_entry()
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

impl Drop for CaterpillarEntry {
    fn drop(&mut self) {
        self.make_unsafe_holder().deallocate_irregular();
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

    fn into_regular(&self) -> RegularEntry {
        RegularEntry::new()
            .with_common_info(CommonEntryInfo::new().with_is_regular(true))
            .with_file_id(self.foo as u32)
            .with_offset_reduced(self.bar as u32)
    }
}

#[bitfield(bits = 64)]
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct RegularEntry {
    pub common_info: CommonEntryInfo,
    /// Storage id, capped at 2^29-1
    pub file_id: B29,
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
    /// A non-zero heap memory address of `IrregularEntry` (i.e. 8-byte aligned struct) down-shifted by 3 bits
    pub shifted_address: B61,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entry_turning() {
        let regular = RegularEntry::new()
            .with_common_info(CommonEntryInfo::new().with_is_regular(true))
            .with_file_id(4534)
            .with_offset_reduced(99)
            .with_is_zero_lamports(true);
        let regular_centry = CaterpillarEntry::from_regular(regular);
        let FixedKindIndexEntry::Regular(regular) = regular_centry.as_fixed_entry() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(regular.file_id(), 4534);
        assert_eq!(regular.offset_reduced(), 99);

        let irregular = IrregularEntry {
            foo: 42,
            bar: 45,
            baz: 13,
        };
        let irregular_centry = CaterpillarEntry::from_irregular(irregular);
        let FixedKindIndexEntry::Irregular(irregular) = irregular_centry.as_fixed_entry() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(irregular.bar, 45);
        assert_eq!(irregular.foo, 42);
        assert_eq!(irregular.baz, 13);

        let turning_centry = regular_centry;
        let FixedKindIndexEntry::Irregular(irregular) = turning_centry.turn_to_irregular() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(irregular.bar, 99);
        assert_eq!(irregular.foo, 4534);
        assert_eq!(irregular.baz, 1);
    }
}
