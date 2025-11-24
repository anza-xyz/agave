// The VmSlice class is used for cases when you need a slice that is stored in the BPF
// interpreter's virtual address space. Because this source code can be compiled with
// addresses of different bit depths, we cannot assume that the 64-bit BPF interpreter's
// pointer sizes can be mapped to physical pointer sizes. In particular, if you need a
// slice-of-slices in the virtual space, the inner slices will be different sizes in a
// 32-bit app build than in the 64-bit virtual space. Therefore instead of a slice-of-slices,
// you should implement a slice-of-VmSlices, which can then use VmSlice::translate() to
// map to the physical address.
// This class must consist only of 16 bytes: a u64 ptr and a u64 len, to match the 64-bit
// implementation of a slice in Rust. The PhantomData entry takes up 0 bytes.

use std::marker::PhantomData;

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct VmSlice<T> {
    ptr: u64,
    len: u64,
    resource_type: PhantomData<T>,
}

impl<T> VmSlice<T> {
    pub fn new(ptr: u64, len: u64) -> Self {
        VmSlice {
            ptr,
            len,
            resource_type: PhantomData,
        }
    }

    pub fn ptr(&self) -> u64 {
        self.ptr
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn end(&self) -> u64 {
        self.ptr()
            .saturating_add(self.len().saturating_mul(size_of::<T>() as u64))
    }

    /// # Safety
    /// Set a new length for the mapped account.
    /// This function is not safe to use if not coupled with the respective change in
    /// the underlying vector.
    pub unsafe fn set_len(&mut self, new_len: u64) {
        self.len = new_len;
    }
}
