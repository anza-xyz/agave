use std::sync::{
    atomic::{AtomicU64, Ordering},
    RwLock, RwLockReadGuard, RwLockWriteGuard,
};

use modular_bitfield::{bitfield, prelude::*};

#[derive(Debug)]
pub enum CaterpillarEntryView<'a> {
    Inlined(InlinedEntry),
    HeapRead(RwLockReadGuard<'a, ExpandedEntry>),
    HeapWrite(RwLockWriteGuard<'a, ExpandedEntry>),
}

pub struct X {
    pub foo: std::sync::Arc<u64>,
}

/// Entry that can turn its payload between inlined and expanded states.
pub struct CaterpillarEntry {
    /// Raw value - either an inline (small) payload or address of a boxed full value
    raw_atomic: AtomicU64,
}

impl CaterpillarEntry {
    pub fn from_inlined(inlined: InlinedEntry) -> Self {
        Self::from_unsafe_holder(UnsafeCaterpillarHolder::from_inlined(inlined))
    }

    pub fn from_expanded(expanded: ExpandedEntry) -> Self {
        Self::from_unsafe_holder(UnsafeCaterpillarHolder::from_expanded(expanded))
    }

    fn from_unsafe_holder(holder: UnsafeCaterpillarHolder) -> Self {
        Self {
            raw_atomic: AtomicU64::new(holder.raw()),
        }
    }

    pub fn as_view(&self) -> CaterpillarEntryView {
        UnsafeCaterpillarHolder::from_raw(self.raw_atomic.load(Ordering::Relaxed))
            .into_fixed_entry()
    }

    pub fn as_cocoon(&self) -> CaterpillarCocoon {
        let x = UnsafeCaterpillarHolder::from_raw(self.raw_atomic.load(Ordering::Relaxed))
            .into_fixed_entry();
        CaterpillarCocoon {
            entry: self,
            kind: x,
        }
    }

    pub fn turn_to_expanded(&self) -> CaterpillarEntryView {
        let mut current_raw = self.raw_atomic.load(Ordering::Acquire);
        loop {
            let current_holder = UnsafeCaterpillarHolder::from_raw(current_raw);
            match current_holder.into_fixed_entry() {
                CaterpillarEntryView::Inlined(inlined) => {
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
                e @ CaterpillarEntryView::HeapRead(_) => return e,
                e @ CaterpillarEntryView::HeapWrite(_) => return e,
            }
        }
    }

    pub fn turn_to_inlined(&mut self) -> CaterpillarEntryView {
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
}

impl Drop for CaterpillarEntry {
    fn drop(&mut self) {
        let previous_raw = self.raw_atomic.swap(0, Ordering::Relaxed);
        if previous_raw != 0 {
            UnsafeCaterpillarHolder::from_raw(previous_raw).deallocate_box_when_expanded();
        }
    }
}

pub struct CaterpillarCocoon<'a> {
    pub entry: &'a CaterpillarEntry,
    pub kind: CaterpillarEntryView<'a>,
}

impl Drop for CaterpillarCocoon<'_> {
    fn drop(&mut self) {
        match std::mem::replace(
            &mut self.kind,
            CaterpillarEntryView::Inlined(InlinedEntry::default()),
        ) {
            CaterpillarEntryView::Inlined(_) => {}
            CaterpillarEntryView::HeapRead(_) => {}
            CaterpillarEntryView::HeapWrite(mut write) => {
                if write.ref_count == 1 && write.slot_list.len() == 1 {
                    self.entry.raw_atomic.store(
                        UnsafeCaterpillarHolder::from_inlined(
                            write.slot_list.drain(..).next().unwrap(),
                        )
                        .raw(),
                        Ordering::Release,
                    );
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct ExpandedEntry {
    pub ref_count: u32,
    pub slot_list: Vec<InlinedEntry>,
}

impl From<InlinedEntry> for ExpandedEntry {
    fn from(inlined: InlinedEntry) -> Self {
        Self {
            ref_count: 1,
            slot_list: vec![inlined],
        }
    }
}

impl Into<InlinedEntry> for ExpandedEntry {
    fn into(self) -> InlinedEntry {
        InlinedEntry::new()
            .with_common_info(CommonEntryInfo::new().with_is_inlined(true))
            .with_file_id(self.slot_list[0].file_id())
            .with_offset_reduced(self.slot_list[0].offset_reduced())
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
    inlined: InlinedEntry,
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

    fn from_inlined(inlined: InlinedEntry) -> Self {
        assert!(inlined.common_info().is_inlined());
        Self { inlined: inlined }
    }

    fn from_expanded(expanded: ExpandedEntry) -> Self {
        let expanded_ptr = Box::into_raw(Box::new(RwLock::new(expanded)));
        let expanded = HeapEntryUnsafeAddress::new()
            .with_common_info(CommonEntryInfo::new().with_is_inlined(false))
            .with_shifted_address(Self::make_ptr_shifted_address(expanded_ptr));
        Self { expanded }
    }

    fn raw(&self) -> u64 {
        unsafe { self.raw }
    }

    fn is_inlined(&self) -> bool {
        unsafe { self.inlined.common_info().is_inlined() }
    }

    fn into_fixed_entry<'a>(self) -> CaterpillarEntryView<'a> {
        unsafe {
            if self.is_inlined() {
                CaterpillarEntryView::Inlined(self.inlined)
            } else {
                let ptr = self.get_expanded_ptr();
                CaterpillarEntryView::HeapRead(ptr.as_ref().unwrap().read().unwrap())
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
                Some(Box::from_raw(ptr).into_inner().unwrap())
            }
        } else {
            None
        }
    }

    fn make_ptr_shifted_address(ptr: *mut RwLock<ExpandedEntry>) -> u64 {
        (ptr as u64) >> Self::ALIGNED_PTR_SHIFT
    }

    fn get_expanded_ptr(&self) -> *const RwLock<ExpandedEntry> {
        unsafe {
            assert!(!self.is_inlined());
            let addr = self.expanded.shifted_address() << Self::ALIGNED_PTR_SHIFT;
            addr as *const RwLock<ExpandedEntry>
        }
    }

    fn get_expanded_mut_ptr(&self) -> *mut RwLock<ExpandedEntry> {
        unsafe {
            assert!(!self.is_inlined());
            let addr = self.expanded.shifted_address() << Self::ALIGNED_PTR_SHIFT;
            addr as *mut RwLock<ExpandedEntry>
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entry_turning() {
        assert_eq!(size_of::<X>(), 8);
        let inlined = InlinedEntry::new()
            .with_common_info(CommonEntryInfo::new().with_is_inlined(true))
            .with_file_id(4534)
            .with_offset_reduced(99)
            .with_is_zero_lamports(true);
        let inlined_centry = CaterpillarEntry::from_inlined(inlined);
        let CaterpillarEntryView::Inlined(inlined) = inlined_centry.as_view() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(inlined.file_id(), 4534);
        assert_eq!(inlined.offset_reduced(), 99);

        let expanded = ExpandedEntry {
            ref_count: 2,
            slot_list: vec![inlined],
        };
        let expanded_centry = CaterpillarEntry::from_expanded(expanded);
        let CaterpillarEntryView::HeapRead(expanded) = expanded_centry.as_view() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(expanded.ref_count, 2);
        assert_eq!(expanded.slot_list[0].file_id(), 4534);
        assert_eq!(expanded.slot_list[0].offset_reduced(), 99);

        let turning_centry = inlined_centry;
        let CaterpillarEntryView::HeapRead(expanded) = turning_centry.turn_to_expanded() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(expanded.ref_count, 1);
        assert_eq!(expanded.slot_list[0].file_id(), 4534);
        assert_eq!(expanded.slot_list[0].offset_reduced(), 99);
        drop(expanded);

        let mut mut_turning_centry = turning_centry;
        mut_turning_centry.turn_to_inlined();
        let CaterpillarEntryView::Inlined(inlined) = mut_turning_centry.as_view() else {
            panic!("Unexpected entry type")
        };
        assert_eq!(inlined.file_id(), 4534);
        assert_eq!(inlined.offset_reduced(), 99);
    }
}
