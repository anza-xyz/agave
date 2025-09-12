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

    pub fn turn_to_expanded(&self) -> FixedKindEntry {
        let mut current_raw = self.raw_atomic.load(Ordering::Acquire);
        loop {
            let current_holder = UnsafeCaterpillarHolder::from_raw(current_raw);
            match current_holder.into_fixed_entry() {
                FixedKindEntry::Inlined(inlined) => {
                    let new_holder =
                        UnsafeCaterpillarHolder::from_expanded(ExpandedEntry::from(inlined));
                    match self.raw_atomic.compare_exchange(
                        current_raw,
                        new_holder.raw(),
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => return new_holder.into_fixed_entry(),
                        Err(different_raw) => {
                            // entry that we created and tried to store will not be used, so deallocate it
                            new_holder.deallocate_box_when_expanded();
                            // continue loop to re-check if the entry is still inlined
                            current_raw = different_raw
                        }
                    }
                }
                e @ FixedKindEntry::Heap(_) => return e,
            }
        }
    }

    pub fn turn_to_inlined(&mut self) -> FixedKindEntry {
        let current_raw = self.raw_atomic.load(Ordering::Acquire);
        let current_holder = UnsafeCaterpillarHolder::from_raw(current_raw);
        if current_holder.is_inlined() {
            return current_holder.into_fixed_entry();
        }
        let expanded = current_holder
            .deallocate_box_when_expanded()
            .expect("entry should be of expanded kind");
        let new_holder = UnsafeCaterpillarHolder::from_inlined(expanded.into());
        // We have exclusive (&mut) access to the entry, so no other code is reading or updating
        // the raw atomic. This means we can just store new value without conflict.
        self.raw_atomic.store(new_holder.raw(), Ordering::Release);
        new_holder.into_fixed_entry()
    }

    fn make_unsafe_holder(&self) -> UnsafeCaterpillarHolder {
        UnsafeCaterpillarHolder::from_raw(self.raw_atomic.load(Ordering::Relaxed))
    }

    fn from_unsafe_holder(holder: UnsafeCaterpillarHolder) -> Self {
        Self {
            raw_atomic: AtomicU64::new(holder.raw()),
        }
    }
}

impl Drop for CaterpillarEntry {
    fn drop(&mut self) {
        let previous_raw = self.raw_atomic.swap(0, Ordering::Relaxed);
        if previous_raw != 0 {
            UnsafeCaterpillarHolder::from_raw(previous_raw).deallocate_box_when_expanded();
        }
    }
}

#[derive(Debug)]
pub struct ExpandedEntry {
    pub foo: u64,
    pub bar: u32,
    pub baz: u64,
}

impl From<InlinedEntry> for ExpandedEntry {
    fn from(inlined: InlinedEntry) -> Self {
        Self {
            foo: inlined.file_id() as u64,
            bar: inlined.offset_reduced() as u32,
            baz: 1,
        }
    }
}

impl Into<InlinedEntry> for ExpandedEntry {
    fn into(self) -> InlinedEntry {
        InlinedEntry::new()
            .with_common_info(CommonEntryInfo::new().with_is_inlined(true))
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
    /// Whether the payload is inlined in the entry itself (otherwise entry stores address to heap memory)
    pub is_inlined: bool,
    pub reserved: B2,
}

#[bitfield(bits = 64)]
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, Eq, PartialEq)]
pub struct HeapEntryUnsafeAddress {
    pub common_info: CommonEntryInfo,
    /// A non-zero heap memory address of `ExpandedEntry` (i.e. 8-byte aligned struct) down-shifted by 3 bits
    pub shifted_address: B61,
}

union UnsafeCaterpillarHolder {
    raw: u64,
    regular: InlinedEntry,
    expanded: HeapEntryUnsafeAddress,
}

impl UnsafeCaterpillarHolder {
    const ALIGNED_PTR_SHIFT: u64 = 3;

    fn from_raw(raw: u64) -> Self {
        assert_ne!(
            raw, 0,
            "0 is not a valid value, i.e. neither it is inlined nor holds a valid heap address"
        );
        Self { raw }
    }

    fn from_inlined(regular: InlinedEntry) -> Self {
        assert!(regular.common_info().is_inlined());
        Self { regular }
    }

    fn from_expanded(expanded: ExpandedEntry) -> Self {
        let expanded_ptr = Box::into_raw(Box::new(expanded));
        let expanded = HeapEntryUnsafeAddress::new()
            .with_common_info(CommonEntryInfo::new().with_is_inlined(false))
            .with_shifted_address(Self::make_ptr_shifted_address(expanded_ptr));
        Self { expanded }
    }

    fn raw(&self) -> u64 {
        unsafe { self.raw }
    }

    fn is_inlined(&self) -> bool {
        unsafe { self.regular.common_info().is_inlined() }
    }

    fn into_fixed_entry<'a>(self) -> FixedKindEntry<'a> {
        unsafe {
            if self.is_inlined() {
                FixedKindEntry::Inlined(self.regular)
            } else {
                let ptr = self.get_expanded_ptr();
                FixedKindEntry::Heap(ptr.as_ref().unwrap())
            }
        }
    }

    /// If entry is expanded, converts the saved heap address to the entry itself
    ///
    /// Note: this will deallocate the entry from the heap
    fn deallocate_box_when_expanded(self) -> Option<ExpandedEntry> {
        if !self.is_inlined() {
            unsafe {
                let ptr = self.get_expanded_mut_ptr();
                Some(*Box::from_raw(ptr))
            }
        } else {
            None
        }
    }

    fn make_ptr_shifted_address(ptr: *mut ExpandedEntry) -> u64 {
        (ptr as u64) >> Self::ALIGNED_PTR_SHIFT
    }

    fn get_expanded_ptr(&self) -> *const ExpandedEntry {
        unsafe {
            assert!(!self.is_inlined());
            let addr = self.expanded.shifted_address() << Self::ALIGNED_PTR_SHIFT;
            addr as *const ExpandedEntry
        }
    }

    fn get_expanded_mut_ptr(&self) -> *mut ExpandedEntry {
        unsafe {
            assert!(!self.is_inlined());
            let addr = self.expanded.shifted_address() << Self::ALIGNED_PTR_SHIFT;
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
            .with_common_info(CommonEntryInfo::new().with_is_inlined(true))
            .with_file_id(4534)
            .with_offset_reduced(99)
            .with_is_zero_lamports(true);
        let regular_centry = CaterpillarEntry::from_inlined(regular);
        let FixedKindEntry::Inlined(regular) = regular_centry.as_fixed_entry() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(regular.file_id(), 4534);
        assert_eq!(regular.offset_reduced(), 99);

        let expanded = ExpandedEntry {
            foo: 42,
            bar: 45,
            baz: 13,
        };
        let expanded_centry = CaterpillarEntry::from_expanded(expanded);
        let FixedKindEntry::Heap(expanded) = expanded_centry.as_fixed_entry() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(expanded.bar, 45);
        assert_eq!(expanded.foo, 42);
        assert_eq!(expanded.baz, 13);

        let turning_centry = regular_centry;
        let FixedKindEntry::Heap(expanded) = turning_centry.turn_to_expanded() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(expanded.bar, 99);
        assert_eq!(expanded.foo, 4534);
        assert_eq!(expanded.baz, 1);

        let mut mut_turning_centry = turning_centry;
        mut_turning_centry.turn_to_inlined();
        let FixedKindEntry::Inlined(regular) = mut_turning_centry.as_fixed_entry() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(regular.file_id(), 4534);
        assert_eq!(regular.offset_reduced(), 99);
    }
}
