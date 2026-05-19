use std::marker::PhantomData;

/// A guest-side representation of a slice of memory.
///
/// `VmSlice` serves as a stable layout for memory shared between the guest and the host.
/// It is also the layout for `SolSignerSeedC` and `SolSignerSeedsC`. `VmSlice` is used anytime you
/// need a slice that is stored in the BPF interpreter's virtual address space. Because this source
/// code can be compiled with addresses of different bit depths, we cannot assume that the 64-bit
/// BPF interpreter's pointer sizes can be mapped to physical pointer sizes (`usize`, `*const u8`,
/// etc.)
///
/// In particular, if you need a slice-of-slices in the virtual space, the inner slices will
/// be different sizes in a 32-bit app build than in the 64-bit virtual space. Therefore instead of
/// a slice-of-slices, you should implement a slice-of-VmSlices, which can then use
/// [`VmSlice::translate`] to map to the physical address.
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

    pub fn set_len(&mut self, new_len: u64) {
        self.len = new_len;
    }

    #[cfg(any(target_arch = "bpf", target_arch = "sbf"))]
    pub fn deref(&self) -> &[T] {
        unsafe { core::slice::from_raw_parts(self.ptr as *const T, self.len as usize) }
    }

    #[cfg(any(target_arch = "bpf", target_arch = "sbf"))]
    pub fn deref_mut(&mut self) -> &mut [T] {
        unsafe { core::slice::from_raw_parts_mut(self.ptr as *mut T, self.len as usize) }
    }
}

const _: () = assert!(size_of::<VmSlice<u8>>() == 16);
const _: () = assert!(size_of::<VmSlice<u64>>() == 16);
const _: () = assert!(size_of::<VmSlice<VmSlice<u8>>>() == 16);
