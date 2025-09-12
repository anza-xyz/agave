use std::{
    collections::HashMap,
    hash::Hash,
    sync::atomic::{AtomicU64, Ordering},
};

use modular_bitfield::{bitfield, prelude::*};

/// Map that can transition entries without locking between small inlined payload into (boxed) values
#[derive(Default)]
pub struct CaterpillarMap<K> {
    map: HashMap<K, CaterpillarEntry>,
}

impl<K> CaterpillarMap<K>
where
    K: Eq + Hash,
{
    pub fn get_fixed(&self, pubkey: &K) -> Option<FixedKindEntry> {
        self.map.get(pubkey).map(|entry| entry.as_fixed_entry())
    }

    pub fn get(&self, pubkey: &K) -> Option<&CaterpillarEntry> {
        self.map.get(pubkey)
    }

    pub fn insert_inlined(&mut self, pubkey: K, inlined: InlinedEntry) {
        self.map
            .insert(pubkey, CaterpillarEntry::from_inlined(inlined));
    }

    pub fn insert_expanded(&mut self, pubkey: K, expanded: ExpandedEntry) {
        self.map
            .insert(pubkey, CaterpillarEntry::from_expanded(expanded));
    }
}

#[derive(Debug)]
pub enum FixedKindEntry<'a> {
    Inlined(InlinedEntry),
    Heap(&'a ExpandedEntry),
}

/// Entry in the caterpillar map that can turn its payload between inlined and expanded states.
pub struct CaterpillarEntry {
    /// Raw value - either an inline (small) payload or address of a boxed full value
    raw_atomic: AtomicU64,
}

impl CaterpillarEntry {
    fn from_inlined(inlined: InlinedEntry) -> Self {
        Self::from_unsafe_holder(UnsafeCaterpillarHolder::from_inlined(inlined))
    }

    fn from_expanded(expanded: ExpandedEntry) -> Self {
        Self::from_unsafe_holder(UnsafeCaterpillarHolder::from_expanded(expanded))
    }

    pub fn as_fixed_entry(&self) -> FixedKindEntry {
        self.make_unsafe_holder().into_fixed_entry()
    }

    pub fn turn_to_irregular(&self) -> FixedKindEntry {
        let mut current_raw = self.raw_atomic.load(Ordering::Acquire);
        loop {
            let current_holder = UnsafeCaterpillarHolder::from_raw(current_raw);
            match current_holder.into_fixed_entry() {
                FixedKindEntry::Inlined(regular) => {
                    let new_holder = UnsafeCaterpillarHolder::from_expanded(
                        ExpandedEntry::from_regular(regular),
                    );
                    match self.raw_atomic.compare_exchange(
                        current_raw,
                        new_holder.raw(),
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
                e @ FixedKindEntry::Heap(_) => return e,
            }
        }
    }

    pub fn turn_to_regular(&mut self) -> FixedKindEntry {
        let current_raw = self.raw_atomic.load(Ordering::Acquire);
        let current_holder = UnsafeCaterpillarHolder::from_raw(current_raw);
        match current_holder.into_fixed_entry() {
            e @ FixedKindEntry::Inlined(_) => e,
            FixedKindEntry::Heap(irregular) => {
                let new_holder = UnsafeCaterpillarHolder::from_inlined(irregular.into_regular());
                // We have exclusive (&mut) access to the entry, so no other code is reading or updating
                // the raw atomic. This means we can just store new value without conflict.
                self.raw_atomic.store(new_holder.raw(), Ordering::Release);
                new_holder.into_fixed_entry()
            }
        }
    }

    fn make_unsafe_holder(&self) -> UnsafeCaterpillarHolder {
        UnsafeCaterpillarHolder::from_raw(self.raw_atomic.load(Ordering::Relaxed))
    }

    fn from_unsafe_holder(holder: UnsafeCaterpillarHolder) -> Self {
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
pub struct ExpandedEntry {
    pub foo: u64,
    pub bar: u32,
    pub baz: u64,
}

impl ExpandedEntry {
    pub fn from_regular(regular: InlinedEntry) -> Self {
        Self {
            foo: regular.file_id() as u64,
            bar: regular.offset_reduced() as u32,
            baz: 1,
        }
    }

    fn into_regular(&self) -> InlinedEntry {
        InlinedEntry::new()
            .with_common_info(CommonEntryInfo::new().with_is_regular(true))
            .with_file_id(self.foo as u32)
            .with_offset_reduced(self.bar as u32)
    }
}

#[bitfield(bits = 64)]
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct InlinedEntry {
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

#[bitfield(bits = 64)]
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct HeapEntryUnsafeAddress {
    pub common_info: CommonEntryInfo,
    /// A non-zero heap memory address of `IrregularEntry` (i.e. 8-byte aligned struct) down-shifted by 3 bits
    pub shifted_address: B61,
}

union UnsafeCaterpillarHolder {
    raw: u64,
    regular: InlinedEntry,
    irregular: HeapEntryUnsafeAddress,
}

impl UnsafeCaterpillarHolder {
    const ALIGNED_PTR_SHIFT: u64 = 3;

    fn from_raw(raw: u64) -> Self {
        Self { raw }
    }

    fn from_inlined(regular: InlinedEntry) -> Self {
        assert!(regular.common_info().is_regular());
        Self { regular }
    }

    fn from_expanded(irregular: ExpandedEntry) -> Self {
        let irregular_ptr = Box::into_raw(Box::new(irregular));
        let irregular = HeapEntryUnsafeAddress::new()
            .with_common_info(CommonEntryInfo::new().with_is_regular(false))
            .with_shifted_address(Self::make_ptr_shifted_address(irregular_ptr));
        Self { irregular }
    }

    fn raw(&self) -> u64 {
        unsafe { self.raw }
    }

    fn is_regular(&self) -> bool {
        unsafe { self.regular.common_info().is_regular() }
    }

    fn into_fixed_entry<'a>(self) -> FixedKindEntry<'a> {
        unsafe {
            if self.is_regular() {
                FixedKindEntry::Inlined(self.regular)
            } else {
                let ptr = self.get_irregular_ptr();
                FixedKindEntry::Heap(ptr.as_ref().unwrap())
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

    fn make_ptr_shifted_address(ptr: *mut ExpandedEntry) -> u64 {
        (ptr as u64) >> Self::ALIGNED_PTR_SHIFT
    }

    fn get_irregular_ptr(&self) -> *const ExpandedEntry {
        unsafe {
            assert!(!self.is_regular());
            let addr = self.irregular.shifted_address() << Self::ALIGNED_PTR_SHIFT;
            addr as *const ExpandedEntry
        }
    }

    fn get_irregular_mut_ptr(&self) -> *mut ExpandedEntry {
        unsafe {
            assert!(!self.is_regular());
            let addr = self.irregular.shifted_address() << Self::ALIGNED_PTR_SHIFT;
            addr as *mut ExpandedEntry
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entry_turning() {
        let regular = InlinedEntry::new()
            .with_common_info(CommonEntryInfo::new().with_is_regular(true))
            .with_file_id(4534)
            .with_offset_reduced(99)
            .with_is_zero_lamports(true);
        let regular_centry = CaterpillarEntry::from_inlined(regular);
        let FixedKindEntry::Inlined(regular) = regular_centry.as_fixed_entry() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(regular.file_id(), 4534);
        assert_eq!(regular.offset_reduced(), 99);

        let irregular = ExpandedEntry {
            foo: 42,
            bar: 45,
            baz: 13,
        };
        let irregular_centry = CaterpillarEntry::from_expanded(irregular);
        let FixedKindEntry::Heap(irregular) = irregular_centry.as_fixed_entry() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(irregular.bar, 45);
        assert_eq!(irregular.foo, 42);
        assert_eq!(irregular.baz, 13);

        let turning_centry = regular_centry;
        let FixedKindEntry::Heap(irregular) = turning_centry.turn_to_irregular() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(irregular.bar, 99);
        assert_eq!(irregular.foo, 4534);
        assert_eq!(irregular.baz, 1);

        let mut mut_turning_centry = turning_centry;
        mut_turning_centry.turn_to_regular();
        let FixedKindEntry::Inlined(regular) = mut_turning_centry.as_fixed_entry() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(regular.file_id(), 4534);
        assert_eq!(regular.offset_reduced(), 99);
    }
}
