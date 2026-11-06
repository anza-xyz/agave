//! Memory translation utilities.

use {
    crate::abi::ABI, solana_sbpf::memory_region::MemoryMapping,
    solana_transaction_context::vm_slice::VmSlice,
};

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
    ($memory_mapping:expr, AccessType::Store, $vm_addr:expr, $T:ty, $abi:expr $(,)?) => {{
        let host_addr = $crate::translate_inner!(
            $memory_mapping,
            map,
            $crate::solana_sbpf::memory_region::AccessType::Store,
            $vm_addr,
            size_of::<$T>() as u64
        )?;
        let ptr = host_addr.ptr_mut().cast::<$T>();
        if $abi == $crate::abi::ABI::V1 && !ptr.is_aligned() {
            Err($crate::memory::MemoryTranslationError::UnalignedPointer.into())
        } else {
            Ok(unsafe { ptr.as_mut_unchecked() })
        }
    }};
    ($memory_mapping:expr, AccessType::Load, $vm_addr:expr, $T:ty, $abi:expr $(,)?) => {{
        let host_addr = $crate::translate_inner!(
            $memory_mapping,
            map,
            $crate::solana_sbpf::memory_region::AccessType::Load,
            $vm_addr,
            size_of::<$T>() as u64
        )?;
        let ptr = host_addr.ptr().cast::<$T>();
        if $abi == $crate::abi::ABI::V1 && !ptr.is_aligned() {
            Err($crate::memory::MemoryTranslationError::UnalignedPointer.into())
        } else {
            Ok(unsafe { ptr.as_ref_unchecked() })
        }
    }};
}

// Do not use this directly
#[macro_export]
macro_rules! translate_slice_inner {
    ($memory_mapping:expr, AccessType::Store, $vm_addr:expr, $len:expr, $T:ty, $abi:expr $(,)?) => {{
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
                    Ok(host_buf)
                        if $abi == $crate::abi::ABI::V1
                            && !host_buf.ptr().cast::<$T>().is_aligned() =>
                    {
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
    ($memory_mapping:expr, AccessType::Load, $vm_addr:expr, $len:expr, $T:ty, $abi:expr $(,)?) => {{
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
                    Ok(host_buf)
                        if $abi == $crate::abi::ABI::V1
                            && !host_buf.ptr().cast::<$T>().is_aligned() =>
                    {
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
    abi: ABI,
) -> Result<&'a T, Box<dyn std::error::Error>> {
    translate_type_inner!(memory_mapping, AccessType::Load, vm_addr, T, abi)
}

pub fn translate_slice<T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    len: u64,
    abi: ABI,
) -> Result<&[T], Box<dyn std::error::Error>> {
    translate_slice_inner!(memory_mapping, AccessType::Load, vm_addr, len, T, abi).map(
        |value| unsafe {
            // SAFETY: `translate_slice_inner` is guaranteed to return a dereferenceable memory region.
            // This is producing a shared/read-only slice to the memory, so the uniqueness invariants
            // aren't relevant.
            &*value
        },
    )
}

/// CPI-specific version with intentionally different lifetime signature.
/// This version is missing lifetime 'a of the return type in the parameter &MemoryMapping.
pub fn translate_type_mut_for_cpi<'a, T>(
    memory_mapping: &MemoryMapping,
    vm_addr: u64,
    abi: ABI,
) -> Result<&'a mut T, Box<dyn std::error::Error>> {
    translate_type_inner!(memory_mapping, AccessType::Store, vm_addr, T, abi)
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
    abi: ABI,
) -> Result<&'a mut [T], Box<dyn std::error::Error>> {
    translate_slice_inner!(memory_mapping, AccessType::Store, vm_addr, len, T, abi).map(
        |p| unsafe {
            // SAFETY: the invariants for producing mutable references delegated to the callers.
            &mut *p
        },
    )
}

pub fn translate_vm_slice<'a, T>(
    slice: &VmSlice<T>,
    memory_mapping: &'a MemoryMapping,
    abi: ABI,
) -> Result<&'a [T], Box<dyn std::error::Error>> {
    translate_slice::<T>(memory_mapping, slice.ptr(), slice.len(), abi)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_sbpf::{
            aligned_memory::AlignedMemory, ebpf::HOST_ALIGN, memory_region::MemoryRegion,
            program::SBPFVersion, vm::Config,
        },
        test_case::test_case,
    };

    const VM_ADDR: u64 = 0x100000000;

    #[test_case(ABI::V0, true; "abi_v0_allows_unaligned")]
    #[test_case(ABI::V1, false; "abi_v1_rejects_unaligned")]
    fn test_translate_unaligned_pointer(abi: ABI, is_allowed: bool) {
        let mut memory = AlignedMemory::<HOST_ALIGN>::zero_filled(32);
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let regions = vec![MemoryRegion::new(&mut memory, VM_ADDR)];
        let mapping = unsafe { MemoryMapping::new(regions, &config, SBPFVersion::V3).unwrap() };
        let unaligned = VM_ADDR + 1;

        let results = [
            translate_slice::<u64>(&mapping, unaligned, 1, abi).map(|_| ()),
            unsafe { translate_slice_mut_for_cpi::<u64>(&mapping, unaligned, 1, abi) }.map(|_| ()),
        ];
        for result in results {
            assert_eq!(result.is_ok(), is_allowed);
        }

        // Aligned pointers translate under every ABI.
        assert!(translate_type::<u64>(&mapping, VM_ADDR, abi).is_ok());
        assert!(translate_type_mut_for_cpi::<u64>(&mapping, VM_ADDR, abi).is_ok());
        assert!(translate_slice::<u64>(&mapping, VM_ADDR, 1, abi).is_ok());
    }

    #[test]
    fn test_translate_type_rejects_unaligned_pointer_abi_v1() {
        let mut memory = AlignedMemory::<HOST_ALIGN>::zero_filled(32);
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let regions = vec![MemoryRegion::new(&mut memory, VM_ADDR)];
        let mapping = unsafe { MemoryMapping::new(regions, &config, SBPFVersion::V3).unwrap() };
        let unaligned = VM_ADDR + 1;

        assert!(translate_type::<u64>(&mapping, unaligned, ABI::V1).is_err());
        assert!(translate_type_mut_for_cpi::<u64>(&mapping, unaligned, ABI::V1).is_err());
    }
}
