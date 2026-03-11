//! Memory translation utilities.

use {
    solana_sbpf::memory_region::{AccessType, MemoryMapping},
    solana_transaction_context::vm_slice::VmSlice,
    std::{mem::align_of, slice::from_raw_parts_mut},
};

/// Error types for memory translation operations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MemoryTranslationError {
    #[error("Unaligned pointer")]
    UnalignedPointer,
    #[error("Invalid length")]
    InvalidLength,
}

pub fn address_is_aligned<T>(address: u64) -> bool {
    (address as *mut T as usize)
        .checked_rem(align_of::<T>())
        .map(|rem| rem == 0)
        .expect("T to be non-zero aligned")
}

// Do not use this directly
#[macro_export]
macro_rules! translate_inner {
    ($memory_mapping:expr, $map:ident, $access_type:expr, $vm_addr:expr, $len:expr $(,)?) => {
        Result::<u64, Box<dyn std::error::Error>>::from(
            $memory_mapping
                .$map($access_type, $vm_addr, $len)
                .map_err(|err| err.into()),
        )
    };
}

// Do not use this directly
#[macro_export]
macro_rules! translate_type_inner {
    ($memory_mapping:expr, $access_type:expr, $vm_addr:expr, $T:ty, $check_aligned:expr $(,)?) => {{
        let host_addr = $crate::translate_inner!(
            $memory_mapping,
            map,
            $access_type,
            $vm_addr,
            size_of::<$T>() as u64
        )?;
        if !$check_aligned {
            Ok(unsafe { std::mem::transmute::<u64, &mut $T>(host_addr) })
        } else if !$crate::memory::address_is_aligned::<$T>(host_addr) {
            Err($crate::memory::MemoryTranslationError::UnalignedPointer.into())
        } else {
            Ok(unsafe { &mut *(host_addr as *mut $T) })
        }
    }};
}

// Do not use this directly
#[macro_export]
macro_rules! translate_slice_inner {
    ($memory_mapping:expr, $access_type:expr, $vm_addr:expr, $len:expr, $T:ty, $check_aligned:expr $(,)?) => {{
        if $len == 0 {
            return Ok(&mut []);
        }
        let total_size = $len.saturating_mul(size_of::<$T>() as u64);
        if isize::try_from(total_size).is_err() {
            return Err($crate::memory::MemoryTranslationError::InvalidLength.into());
        }
        let host_addr =
            $crate::translate_inner!($memory_mapping, map, $access_type, $vm_addr, total_size)?;
        if $check_aligned && !$crate::memory::address_is_aligned::<$T>(host_addr) {
            return Err($crate::memory::MemoryTranslationError::UnalignedPointer.into());
        }
        Ok(unsafe { from_raw_parts_mut(host_addr as *mut $T, $len as usize) })
    }};
}

pub fn translate_type<'a, T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    check_aligned: bool,
) -> Result<&'a T, Box<dyn std::error::Error>> {
    translate_type_inner!(memory_mapping, AccessType::Load, vm_addr, T, check_aligned)
        .map(|value| &*value)
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
    .map(|value| &*value)
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
pub fn translate_slice_mut_for_cpi<'a, T>(
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
}

pub fn translate_vm_slice<'a, T>(
    slice: &VmSlice<T>,
    memory_mapping: &'a MemoryMapping,
    check_aligned: bool,
) -> Result<&'a [T], Box<dyn std::error::Error>> {
    translate_slice::<T>(memory_mapping, slice.ptr(), slice.len(), check_aligned)
}
