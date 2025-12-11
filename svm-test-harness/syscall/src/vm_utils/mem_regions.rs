//! Memory region utilities for syscall harness.

use {
    solana_sbpf::{
        ebpf,
        memory_region::{MemoryMapping, MemoryRegion},
    },
    solana_svm_test_harness_fixture::syscall_effects::InputDataRegion,
};

/// Extract input data regions from a MemoryMapping and convert them into InputDataRegions.
pub fn extract_input_data_regions(mapping: &MemoryMapping<'_>) -> Vec<InputDataRegion> {
    match mapping {
        MemoryMapping::Aligned(_mapping) => mapping
            .get_regions()
            .iter()
            .skip_while(|region| region.vm_addr < ebpf::MM_INPUT_START)
            .map(mem_region_to_input_data_region)
            .collect(),
        MemoryMapping::Unaligned(_mapping) => {
            let mut input_regions: Vec<InputDataRegion> = mapping
                .get_regions()
                .iter()
                .filter(|region| region.vm_addr >= ebpf::MM_INPUT_START)
                .map(mem_region_to_input_data_region)
                .collect();
            input_regions.sort_by_key(|region| region.offset);
            input_regions
        }
        _ => vec![],
    }
}

/// Copy a prefix of bytes from src to dst.
pub fn copy_memory_prefix(dst: &mut [u8], src: &[u8]) {
    let size = dst.len().min(src.len());
    dst[..size].copy_from_slice(&src[..size]);
}

fn mem_region_to_input_data_region(region: &MemoryRegion) -> InputDataRegion {
    InputDataRegion {
        content: unsafe {
            std::slice::from_raw_parts(region.host_addr as *const u8, region.len as usize).to_vec()
        },
        offset: region.vm_addr.saturating_sub(ebpf::MM_INPUT_START),
        is_writable: region.writable,
    }
}
