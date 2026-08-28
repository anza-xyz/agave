use std::{ptr, slice};

/// Marker trait to indicate a type T is valid to access as bytes.
///
/// # Safety
///
/// Caller must ensure all bytes of type T are valid to access.
/// E.g. T is POD and has no uninitialized/padding bytes.
pub unsafe trait AsBytesRef {}

/// Marker trait to indicate a type T is valid to access as mutable bytes.
///
/// # Safety
///
/// Caller must ensure all bytes of type T are valid to access.
/// E.g. T is POD and has no uninitialized/padding bytes.
pub unsafe trait AsBytesMut {}

/// Returns a byte-slice  of `x`.
pub fn as_bytes_ref<T: AsBytesRef>(x: &T) -> &[u8] {
    debug_assert!(size_of::<T>() <= isize::MAX as usize);
    let ptr = ptr::from_ref(x).cast::<u8>();
    // SAFETY:
    // * ptr is non-null
    // * caller guarantees all bytes of T are valid to access
    // * data is properly aligned for T
    // * data is a single allocation
    // * size of T is less than isize::MAX
    unsafe { slice::from_raw_parts(ptr, size_of::<T>()) }
}

/// Returns a mutable byte-slice  of `x`.
pub fn as_bytes_mut<T: AsBytesMut>(x: &mut T) -> &mut [u8] {
    debug_assert!(size_of::<T>() <= isize::MAX as usize);
    let ptr = ptr::from_mut(x).cast::<u8>();
    // SAFETY:
    // * ptr is non-null
    // * caller guarantees all bytes of T are valid to access
    // * data is properly aligned for T
    // * data is a single allocation
    // * size of T is less than isize::MAX
    unsafe { slice::from_raw_parts_mut(ptr, size_of::<T>()) }
}
