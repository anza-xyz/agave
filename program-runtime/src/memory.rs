//! Memory translation utilities.

use {solana_sbpf::memory_region::MemoryMapping, solana_transaction_context::vm_slice::VmSlice};

/// Error types for memory translation operations.
#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum MemoryTranslationError {
    #[error("Unaligned pointer")]
    UnalignedPointer,
    #[error("InvalidLength")]
    InvalidLength,
}

// Do not use this directly
#[macro_export]
macro_rules! translate_inner {
    ($memory_mapping:expr, $map:ident, $access_type:expr, $vm_addr:expr, $len:expr $(,)?) => {
        Result::<$crate::solana_sbpf::memory_region::HostBuffer, Box<dyn std::error::Error>>::from(
            $memory_mapping
                .$map($access_type, $vm_addr, $len)
                .map_err(|err| err.into()),
        )
    };
}

// Do not use this directly
#[macro_export]
macro_rules! translate_type_inner {
    ($memory_mapping:expr, AccessType::Store, $vm_addr:expr, $T:ty, $check_aligned:expr $(,)?) => {{
        let host_addr = $crate::translate_inner!(
            $memory_mapping,
            map,
            $crate::solana_sbpf::memory_region::AccessType::Store,
            $vm_addr,
            size_of::<$T>() as u64
        )?;
        let ptr = host_addr.ptr_mut().cast::<$T>();
        if $check_aligned && !ptr.is_aligned() {
            Err($crate::memory::MemoryTranslationError::UnalignedPointer.into())
        } else {
            Ok(unsafe { ptr.as_mut_unchecked() })
        }
    }};
    ($memory_mapping:expr, AccessType::Load, $vm_addr:expr, $T:ty, $check_aligned:expr $(,)?) => {{
        let host_addr = $crate::translate_inner!(
            $memory_mapping,
            map,
            $crate::solana_sbpf::memory_region::AccessType::Load,
            $vm_addr,
            size_of::<$T>() as u64
        )?;
        let ptr = host_addr.ptr().cast::<$T>();
        if $check_aligned && !ptr.is_aligned() {
            Err($crate::memory::MemoryTranslationError::UnalignedPointer.into())
        } else {
            Ok(unsafe { ptr.as_ref_unchecked() })
        }
    }};
}

// Do not use this directly
#[macro_export]
macro_rules! translate_slice_inner {
    ($memory_mapping:expr, AccessType::Store, $vm_addr:expr, $len:expr, $T:ty, $check_aligned:expr $(,)?) => {{
        if $len == 0 {
            Ok(std::ptr::slice_from_raw_parts_mut(
                std::ptr::dangling_mut::<$T>(),
                0,
            ))
        } else {
            let total_size = $len.saturating_mul(size_of::<$T>() as u64);
            if isize::try_from(total_size).is_err() {
                Err($crate::memory::MemoryTranslationError::InvalidLength.into())
            } else {
                match $crate::translate_inner!(
                    $memory_mapping,
                    map,
                    $crate::solana_sbpf::memory_region::AccessType::Store,
                    $vm_addr,
                    total_size
                ) {
                    Err(e) => Err(e),
                    Ok(host_buf) if $check_aligned && !host_buf.ptr().cast::<$T>().is_aligned() => {
                        Err($crate::memory::MemoryTranslationError::UnalignedPointer.into())
                    }
                    Ok(host_buf) => Ok(std::ptr::slice_from_raw_parts_mut(
                        host_buf.ptr_mut().cast(),
                        $len as usize,
                    )),
                }
            }
        }
    }};
    ($memory_mapping:expr, AccessType::Load, $vm_addr:expr, $len:expr, $T:ty, $check_aligned:expr $(,)?) => {{
        if $len == 0 {
            Ok(std::ptr::slice_from_raw_parts(
                std::ptr::dangling_mut::<$T>(),
                0,
            ))
        } else {
            let total_size = $len.saturating_mul(size_of::<$T>() as u64);
            if isize::try_from(total_size).is_err() {
                Err($crate::memory::MemoryTranslationError::InvalidLength.into())
            } else {
                match $crate::translate_inner!(
                    $memory_mapping,
                    map,
                    $crate::solana_sbpf::memory_region::AccessType::Load,
                    $vm_addr,
                    total_size
                ) {
                    Err(e) => Err(e),
                    Ok(host_buf) if $check_aligned && !host_buf.ptr().cast::<$T>().is_aligned() => {
                        Err($crate::memory::MemoryTranslationError::UnalignedPointer.into())
                    }
                    Ok(host_buf) => Ok(std::ptr::slice_from_raw_parts(
                        host_buf.ptr().cast(),
                        $len as usize,
                    )),
                }
            }
        }
    }};
}

pub fn translate_type<'a, T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    check_aligned: bool,
) -> Result<&'a T, Box<dyn std::error::Error>> {
    translate_type_inner!(memory_mapping, AccessType::Load, vm_addr, T, check_aligned)
}

pub fn translate_slice<T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    len: u64,
    check_aligned: bool,
) -> Result<&[T], Box<dyn std::error::Error>> {
    translate_slice_inner!(
        memory_mapping,
        AccessType::Load,
        vm_addr,
        len,
        T,
        check_aligned,
    )
    .map(|value| unsafe {
        // SAFETY: `translate_slice_inner` is guaranteed to return a dereferenceable memory region.
        // This is producing a shared/read-only slice to the memory, so the uniqueness invariants
        // aren't relevant.
        &*value
    })
}

/// CPI-specific version with intentionally different lifetime signature.
/// This version is missing lifetime 'a of the return type in the parameter &MemoryMapping.
pub fn translate_type_mut_for_cpi<'a, T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    check_aligned: bool,
) -> Result<&'a mut T, Box<dyn std::error::Error>> {
    translate_type_inner!(memory_mapping, AccessType::Store, vm_addr, T, check_aligned)
}

/// CPI-specific version with intentionally different lifetime signature.
/// This version is missing lifetime 'a of the return type in the parameter &MemoryMapping.
///
/// # Safety
///
/// * The caller must ensure that this function does not violate mutable reference uniqueness
///   constraints;
/// * The caller must ensure that the lifetime of the returned slice does not outlive the backing
///   data.
pub unsafe fn translate_slice_mut_for_cpi<'a, T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    len: u64,
    check_aligned: bool,
) -> Result<&'a mut [T], Box<dyn std::error::Error>> {
    translate_slice_inner!(
        memory_mapping,
        AccessType::Store,
        vm_addr,
        len,
        T,
        check_aligned,
    )
    .map(|p| unsafe {
        // SAFETY: the invariants for producing mutable references delegated to the callers.
        &mut *p
    })
}

pub fn translate_vm_slice<'a, T>(
    slice: &VmSlice<T>,
    memory_mapping: &'a MemoryMapping,
    check_aligned: bool,
) -> Result<&'a [T], Box<dyn std::error::Error>> {
    translate_slice::<T>(memory_mapping, slice.ptr(), slice.len(), check_aligned)
}
