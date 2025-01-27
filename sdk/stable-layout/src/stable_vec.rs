//! `Vec`, with a stable memory layout

use std::{
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
};

/// `Vec`, with a stable memory layout
///
/// This container is used within the runtime to ensure memory mapping and memory accesses are
/// valid.  We rely on known addresses and offsets within the runtime, and since `Vec`'s layout
/// is allowed to change, we must provide a way to lock down the memory layout.  `StableVec`
/// reimplements the bare minimum of `Vec`'s API sufficient only for the runtime's needs.
///
/// To ensure memory allocation and deallocation is handled correctly, it is only possible to
/// create a new `StableVec` from an existing `Vec`.  This way we ensure all Rust invariants are
/// upheld.
///
/// # Examples
///
/// Creating a `StableVec` from a `Vec`
///
/// ```
/// # use solana_stable_layout::stable_vec::StableVec;
/// let vec = vec!["meow", "woof", "moo"];
/// let vec = StableVec::from(vec);
/// ```
#[repr(C)]
pub struct StableVec<T> {
    pub addr: u64,
    pub cap: u64,
    pub len: u64,
    _marker: PhantomData<T>,
}

// We shadow these slice methods of the same name to avoid going through
// `deref`, which creates an intermediate reference.
impl<T> StableVec<T> {
    #[inline]
    pub fn as_vaddr(&self) -> u64 {
        self.addr
    }

    #[inline]
    pub fn len(&self) -> u64 {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<T> AsRef<[T]> for StableVec<T> {
    fn as_ref(&self) -> &[T] {
        self.deref()
    }
}

impl<T> AsMut<[T]> for StableVec<T> {
    fn as_mut(&mut self) -> &mut [T] {
        self.deref_mut()
    }
}

impl<T> std::ops::Deref for StableVec<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        unsafe { core::slice::from_raw_parts(self.addr as usize as *mut T, self.len as usize) }
    }
}

impl<T> std::ops::DerefMut for StableVec<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        unsafe { core::slice::from_raw_parts_mut(self.addr as usize as *mut T, self.len as usize) }
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for StableVec<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&**self, f)
    }
}

macro_rules! impl_partial_eq {
    ([$($vars:tt)*] $lhs:ty, $rhs:ty) => {
        impl<T, U, $($vars)*> PartialEq<$rhs> for $lhs
        where
            T: PartialEq<U>,
        {
            #[inline]
            fn eq(&self, other: &$rhs) -> bool { self[..] == other[..] }
        }
    }
}
impl_partial_eq! { [] StableVec<T>, StableVec<U> }
impl_partial_eq! { [] StableVec<T>, Vec<U> }
impl_partial_eq! { [] Vec<T>, StableVec<U> }
impl_partial_eq! { [] StableVec<T>, &[U] }
impl_partial_eq! { [] StableVec<T>, &mut [U] }
impl_partial_eq! { [] &[T], StableVec<U> }
impl_partial_eq! { [] &mut [T], StableVec<U> }
impl_partial_eq! { [] StableVec<T>, [U] }
impl_partial_eq! { [] [T], StableVec<U> }
impl_partial_eq! { [const N: usize] StableVec<T>, [U; N] }
impl_partial_eq! { [const N: usize] StableVec<T>, &[U; N] }

impl<T> From<Vec<T>> for StableVec<T> {
    fn from(other: Vec<T>) -> Self {
        // NOTE: This impl is basically copied from `Vec::into_raw_parts()`.  Once that fn is
        // stabilized, use it here.
        //
        // We are going to pilfer `other`'s guts, and we don't want it to be dropped when it goes
        // out of scope.
        let mut other = ManuallyDrop::new(other);
        Self {
            // SAFETY: We have a valid Vec, so its ptr is non-null.
            addr: other.as_mut_ptr() as u64, // Problematic if other is in 32-bit physical address space
            cap: other.capacity() as u64,
            len: other.len() as u64,
            _marker: PhantomData,
        }
    }
}

impl<T> From<StableVec<T>> for Vec<T> {
    fn from(other: StableVec<T>) -> Self {
        // We are going to pilfer `other`'s guts, and we don't want it to be dropped when it goes
        // out of scope.
        let other = ManuallyDrop::new(other);
        // SAFETY: We have a valid StableVec, which we can only get from a Vec.  Therefore it is
        // safe to convert back to Vec. Assuming we're not starting with a vector in 64-bit virtual
        // address space while building the app in 32-bit, and this vector is in that 32-bit physical
        // space.
        unsafe {
            Vec::from_raw_parts(
                other.addr as usize as *mut T,
                other.len as usize,
                other.cap as usize,
            )
        }
    }
}

impl<T> Drop for StableVec<T> {
    fn drop(&mut self) {
        // We only allow creating a StableVec through creating a Vec.  To ensure we are dropped
        // correctly, convert ourselves back to a Vec and let Vec's drop handling take over.
        //
        // SAFETY: We have a valid StableVec, which we can only get from a Vec.  Therefore it is
        // safe to convert back to Vec.
        let _vec = unsafe {
            Vec::from_raw_parts(
                self.addr as usize as *mut T,
                self.len as usize,
                self.cap as usize,
            )
        };
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        memoffset::offset_of,
        std::mem::{align_of, size_of},
    };

    #[test]
    fn test_memory_layout() {
        assert_eq!(offset_of!(StableVec<i32>, addr), 0);
        assert_eq!(offset_of!(StableVec<i32>, cap), 8);
        assert_eq!(offset_of!(StableVec<i32>, len), 16);
        assert_eq!(align_of::<StableVec<i32>>(), 8);
        assert_eq!(size_of::<StableVec<i32>>(), 8 + 8 + 8);

        // create a vec with different values for cap and len
        let vec = {
            let mut vec = Vec::with_capacity(3);
            vec.push(11);
            vec.push(22);
            vec
        };
        let vec = StableVec::from(vec);

        let addr_vec = &vec as *const _ as usize;
        let addr_ptr = addr_vec;
        let addr_cap = addr_vec + 8;
        let addr_len = addr_vec + 16;
        assert_eq!(unsafe { *(addr_cap as *const usize) }, 3);
        assert_eq!(unsafe { *(addr_len as *const usize) }, 2);

        let ptr_data = addr_ptr as *const &[i32; 2];
        assert_eq!(unsafe { *ptr_data }, &[11, 22]);
    }
}
