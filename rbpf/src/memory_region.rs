//! This module defines memory regions

use crate::{
    aligned_memory::Pod,
    ebpf,
    error::{EbpfError, ProgramResult},
    program::SBPFVersion,
    vm::Config,
};
use std::{
    array,
    cell::{Cell, UnsafeCell},
    fmt, mem,
    ops::Range,
    ptr::{self, copy_nonoverlapping},
};

/* Explaination of the Gapped Memory

    The MemoryMapping supports a special mapping mode which is used for the stack MemoryRegion.
    In this mode the backing address space of the host is sliced in power-of-two aligned frames.
    The exponent of this alignment is specified in vm_gap_shift. Then the virtual address space
    of the guest is spread out in a way which leaves gapes, the same size as the frames, in
    between the frames. This effectively doubles the size of the guests virtual address space.
    But the acutual mapped memory stays the same, as the gaps are not mapped and accessing them
    results in an AccessViolation.

    Guest: frame 0 | gap 0 | frame 1 | gap 1 | frame 2 | gap 2 | ...
              |                /                 /
              |          *----*    *------------*
              |         /         /
    Host:  frame 0 | frame 1 | frame 2 | ...
*/

/// The state of a memory region.
#[derive(Debug, Copy, Clone, Default, Eq, PartialEq)]
pub enum MemoryState {
    /// The memory region is readable
    #[default]
    Readable,
    /// The memory region is writable
    Writable,
    /// The memory region is writable but must be copied before writing. The
    /// carried data can be used to uniquely identify the region.
    Cow(u64),
}

/// Callback executed when a CoW memory region is written to
pub type MemoryCowCallback = Box<dyn Fn(u64) -> Result<u64, ()>>;

/// Memory region for bounds checking and address translation
#[derive(Default, Eq, PartialEq)]
#[repr(C, align(32))]
pub struct MemoryRegion {
    /// start host address
    pub host_addr: Cell<u64>,
    /// start virtual address
    pub vm_addr: u64,
    /// end virtual address
    pub vm_addr_end: u64,
    /// Length in bytes
    pub len: u64,
    /// Size of regular gaps as bit shift (63 means this region is continuous)
    pub vm_gap_shift: u8,
    /// Whether the region is readonly, writable or must be copied before writing
    pub state: Cell<MemoryState>,
}

impl MemoryRegion {
    fn new(slice: &[u8], vm_addr: u64, vm_gap_size: u64, state: MemoryState) -> Self {
        let mut vm_addr_end = vm_addr.saturating_add(slice.len() as u64);
        let mut vm_gap_shift = (std::mem::size_of::<u64>() as u8)
            .saturating_mul(8)
            .saturating_sub(1);
        if vm_gap_size > 0 {
            vm_addr_end = vm_addr_end.saturating_add(slice.len() as u64);
            vm_gap_shift = vm_gap_shift.saturating_sub(vm_gap_size.leading_zeros() as u8);
            debug_assert_eq!(Some(vm_gap_size), 1_u64.checked_shl(vm_gap_shift as u32));
        };
        MemoryRegion {
            host_addr: Cell::new(slice.as_ptr() as u64),
            vm_addr,
            vm_addr_end,
            len: slice.len() as u64,
            vm_gap_shift,
            state: Cell::new(state),
        }
    }

    /// Only to be used in tests and benches
    pub fn new_for_testing(
        slice: &[u8],
        vm_addr: u64,
        vm_gap_size: u64,
        state: MemoryState,
    ) -> Self {
        Self::new(slice, vm_addr, vm_gap_size, state)
    }

    /// Creates a new readonly MemoryRegion from a slice
    pub fn new_readonly(slice: &[u8], vm_addr: u64) -> Self {
        Self::new(slice, vm_addr, 0, MemoryState::Readable)
    }

    /// Creates a new writable MemoryRegion from a mutable slice
    pub fn new_writable(slice: &mut [u8], vm_addr: u64) -> Self {
        Self::new(&*slice, vm_addr, 0, MemoryState::Writable)
    }

    /// Creates a new copy on write MemoryRegion.
    ///
    /// The region is made writable
    pub fn new_cow(slice: &[u8], vm_addr: u64, cow_id: u64) -> Self {
        Self::new(slice, vm_addr, 0, MemoryState::Cow(cow_id))
    }

    /// Creates a new writable gapped MemoryRegion from a mutable slice
    pub fn new_writable_gapped(slice: &mut [u8], vm_addr: u64, vm_gap_size: u64) -> Self {
        Self::new(&*slice, vm_addr, vm_gap_size, MemoryState::Writable)
    }

    /// Convert a virtual machine address into a host address
    pub fn vm_to_host(&self, vm_addr: u64, len: u64) -> ProgramResult {
        // This can happen if a region starts at an offset from the base region
        // address, eg with rodata regions if config.optimize_rodata = true, see
        // Elf::get_ro_region.
        if vm_addr < self.vm_addr {
            return ProgramResult::Err(EbpfError::InvalidVirtualAddress(vm_addr));
        }

        let begin_offset = vm_addr.saturating_sub(self.vm_addr);
        let is_in_gap = (begin_offset
            .checked_shr(self.vm_gap_shift as u32)
            .unwrap_or(0)
            & 1)
            == 1;
        let gap_mask = (-1i64).checked_shl(self.vm_gap_shift as u32).unwrap_or(0) as u64;
        let gapped_offset =
            (begin_offset & gap_mask).checked_shr(1).unwrap_or(0) | (begin_offset & !gap_mask);
        if let Some(end_offset) = gapped_offset.checked_add(len) {
            if end_offset <= self.len && !is_in_gap {
                return ProgramResult::Ok(self.host_addr.get().saturating_add(gapped_offset));
            }
        }
        ProgramResult::Err(EbpfError::InvalidVirtualAddress(vm_addr))
    }
}

impl fmt::Debug for MemoryRegion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "host_addr: {:#x?}-{:#x?}, vm_addr: {:#x?}-{:#x?}, len: {}",
            self.host_addr,
            self.host_addr.get().saturating_add(self.len),
            self.vm_addr,
            self.vm_addr_end,
            self.len
        )
    }
}
impl std::cmp::PartialOrd for MemoryRegion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl std::cmp::Ord for MemoryRegion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.vm_addr.cmp(&other.vm_addr)
    }
}

/// Type of memory access
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AccessType {
    /// Read
    Load,
    /// Write
    Store,
}

/// Memory mapping based on eytzinger search.
pub struct UnalignedMemoryMapping<'a> {
    /// Mapped memory regions
    regions: Box<[MemoryRegion]>,
    /// Copy of the regions vm_addr fields to improve cache density
    region_addresses: Box<[u64]>,
    /// Cache of the last `MappingCache::SIZE` vm_addr => region_index lookups
    cache: UnsafeCell<MappingCache>,
    /// VM configuration
    config: &'a Config,
    /// Executable sbpf_version
    sbpf_version: &'a SBPFVersion,
    /// CoW callback
    cow_cb: Option<MemoryCowCallback>,
}

impl<'a> fmt::Debug for UnalignedMemoryMapping<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnalignedMemoryMapping")
            .field("regions", &self.regions)
            .field("region_addresses", &self.region_addresses)
            .field("cache", &self.cache)
            .field("config", &self.config)
            .field(
                "cow_cb",
                &self
                    .cow_cb
                    .as_ref()
                    .map(|cb| format!("Some({:p})", &cb))
                    .unwrap_or_else(|| "None".to_string()),
            )
            .finish()
    }
}

impl<'a> UnalignedMemoryMapping<'a> {
    fn construct_eytzinger_order(
        &mut self,
        ascending_regions: &mut [MemoryRegion],
        mut in_index: usize,
        out_index: usize,
    ) -> usize {
        if out_index >= self.regions.len() {
            return in_index;
        }
        in_index = self.construct_eytzinger_order(
            ascending_regions,
            in_index,
            out_index.saturating_mul(2).saturating_add(1),
        );
        self.regions[out_index] = mem::take(&mut ascending_regions[in_index]);
        self.region_addresses[out_index] = self.regions[out_index].vm_addr;
        self.construct_eytzinger_order(
            ascending_regions,
            in_index.saturating_add(1),
            out_index.saturating_mul(2).saturating_add(2),
        )
    }

    fn new_internal(
        mut regions: Vec<MemoryRegion>,
        cow_cb: Option<MemoryCowCallback>,
        config: &'a Config,
        sbpf_version: &'a SBPFVersion,
    ) -> Result<Self, EbpfError> {
        regions.sort();
        for index in 1..regions.len() {
            let first = &regions[index.saturating_sub(1)];
            let second = &regions[index];
            if first.vm_addr_end > second.vm_addr {
                return Err(EbpfError::InvalidMemoryRegion(index));
            }
        }

        let mut result = Self {
            regions: (0..regions.len())
                .map(|_| MemoryRegion::default())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            region_addresses: vec![0; regions.len()].into_boxed_slice(),
            cache: UnsafeCell::new(MappingCache::new()),
            config,
            sbpf_version,
            cow_cb,
        };
        result.construct_eytzinger_order(&mut regions, 0, 0);
        Ok(result)
    }

    /// Creates a new UnalignedMemoryMapping structure from the given regions
    pub fn new(
        regions: Vec<MemoryRegion>,
        config: &'a Config,
        sbpf_version: &'a SBPFVersion,
    ) -> Result<Self, EbpfError> {
        Self::new_internal(regions, None, config, sbpf_version)
    }

    /// Creates a new UnalignedMemoryMapping from the given regions.
    ///
    /// `cow_cb` is used to copy CoW regions on the first write access.
    pub fn new_with_cow(
        regions: Vec<MemoryRegion>,
        cow_cb: MemoryCowCallback,
        config: &'a Config,
        sbpf_version: &'a SBPFVersion,
    ) -> Result<Self, EbpfError> {
        Self::new_internal(regions, Some(cow_cb), config, sbpf_version)
    }

    #[allow(clippy::arithmetic_side_effects)]
    fn find_region(&self, cache: &mut MappingCache, vm_addr: u64) -> Option<&MemoryRegion> {
        if let Some(index) = cache.find(vm_addr) {
            // Safety:
            // Cached index, we validated it before caching it. See the corresponding safety section
            // in the miss branch.
            Some(unsafe { self.regions.get_unchecked(index - 1) })
        } else {
            let mut index = 1;
            while index <= self.region_addresses.len() {
                // Safety:
                // we start the search at index=1 and in the loop condition check
                // for index <= len, so bound checks can be avoided
                index = (index << 1)
                    + unsafe { *self.region_addresses.get_unchecked(index - 1) <= vm_addr }
                        as usize;
            }
            index >>= index.trailing_zeros() + 1;
            if index == 0 {
                return None;
            }
            // Safety:
            // we check for index==0 above, and by construction if we get here index
            // must be contained in region
            let region = unsafe { self.regions.get_unchecked(index - 1) };
            cache.insert(region.vm_addr..region.vm_addr_end, index);
            Some(region)
        }
    }

    /// Given a list of regions translate from virtual machine to host address
    pub fn map(&self, access_type: AccessType, vm_addr: u64, len: u64) -> ProgramResult {
        // Safety:
        // &mut references to the mapping cache are only created internally from methods that do not
        // invoke each other. UnalignedMemoryMapping is !Sync, so the cache reference below is
        // guaranteed to be unique.
        let cache = unsafe { &mut *self.cache.get() };

        let region = match self.find_region(cache, vm_addr) {
            Some(res) => res,
            None => {
                return generate_access_violation(
                    self.config,
                    self.sbpf_version,
                    access_type,
                    vm_addr,
                    len,
                )
            }
        };

        if access_type == AccessType::Load || ensure_writable_region(region, &self.cow_cb) {
            if let ProgramResult::Ok(host_addr) = region.vm_to_host(vm_addr, len) {
                return ProgramResult::Ok(host_addr);
            }
        }

        generate_access_violation(self.config, self.sbpf_version, access_type, vm_addr, len)
    }

    /// Loads `size_of::<T>()` bytes from the given address.
    ///
    /// See [MemoryMapping::load].
    #[inline(always)]
    pub fn load<T: Pod + Into<u64>>(&self, mut vm_addr: u64) -> ProgramResult {
        let mut len = mem::size_of::<T>() as u64;
        debug_assert!(len <= mem::size_of::<u64>() as u64);

        // Safety:
        // &mut references to the mapping cache are only created internally from methods that do not
        // invoke each other. UnalignedMemoryMapping is !Sync, so the cache reference below is
        // guaranteed to be unique.
        let cache = unsafe { &mut *self.cache.get() };

        let mut region = match self.find_region(cache, vm_addr) {
            Some(region) => {
                if let ProgramResult::Ok(host_addr) = region.vm_to_host(vm_addr, len) {
                    // fast path
                    return ProgramResult::Ok(unsafe {
                        ptr::read_unaligned::<T>(host_addr as *const _).into()
                    });
                }

                region
            }
            None => {
                return generate_access_violation(
                    self.config,
                    self.sbpf_version,
                    AccessType::Load,
                    vm_addr,
                    len,
                )
            }
        };

        // slow path
        let initial_len = len;
        let initial_vm_addr = vm_addr;
        let mut value = 0u64;
        let mut ptr = std::ptr::addr_of_mut!(value).cast::<u8>();

        while len > 0 {
            let load_len = len.min(region.vm_addr_end.saturating_sub(vm_addr));
            if load_len == 0 {
                break;
            }
            if let ProgramResult::Ok(host_addr) = region.vm_to_host(vm_addr, load_len) {
                // Safety:
                // we debug_assert!(len <= mem::size_of::<u64>()) so we never
                // overflow &value
                unsafe {
                    copy_nonoverlapping(host_addr as *const _, ptr, load_len as usize);
                    ptr = ptr.add(load_len as usize);
                };
                len = len.saturating_sub(load_len);
                if len == 0 {
                    return ProgramResult::Ok(value);
                }
                vm_addr = vm_addr.saturating_add(load_len);
                region = match self.find_region(cache, vm_addr) {
                    Some(region) => region,
                    None => break,
                };
            } else {
                break;
            }
        }

        generate_access_violation(
            self.config,
            self.sbpf_version,
            AccessType::Load,
            initial_vm_addr,
            initial_len,
        )
    }

    /// Store `value` at the given address.
    ///
    /// See [MemoryMapping::store].
    #[inline]
    pub fn store<T: Pod>(&self, value: T, mut vm_addr: u64) -> ProgramResult {
        let mut len = mem::size_of::<T>() as u64;

        // Safety:
        // &mut references to the mapping cache are only created internally from methods that do not
        // invoke each other. UnalignedMemoryMapping is !Sync, so the cache reference below is
        // guaranteed to be unique.
        let cache = unsafe { &mut *self.cache.get() };

        let mut src = std::ptr::addr_of!(value).cast::<u8>();

        let mut region = match self.find_region(cache, vm_addr) {
            Some(region) if ensure_writable_region(region, &self.cow_cb) => {
                // fast path
                if let ProgramResult::Ok(host_addr) = region.vm_to_host(vm_addr, len) {
                    // Safety:
                    // vm_to_host() succeeded so we know there's enough space to
                    // store `value`
                    unsafe { ptr::write_unaligned(host_addr as *mut _, value) };
                    return ProgramResult::Ok(host_addr);
                }
                region
            }
            _ => {
                return generate_access_violation(
                    self.config,
                    self.sbpf_version,
                    AccessType::Store,
                    vm_addr,
                    len,
                )
            }
        };

        // slow path
        let initial_len = len;
        let initial_vm_addr = vm_addr;

        while len > 0 {
            if !ensure_writable_region(region, &self.cow_cb) {
                break;
            }

            let write_len = len.min(region.vm_addr_end.saturating_sub(vm_addr));
            if write_len == 0 {
                break;
            }
            if let ProgramResult::Ok(host_addr) = region.vm_to_host(vm_addr, write_len) {
                // Safety:
                // vm_to_host() succeeded so we have enough space for write_len
                unsafe { copy_nonoverlapping(src, host_addr as *mut _, write_len as usize) };
                len = len.saturating_sub(write_len);
                if len == 0 {
                    return ProgramResult::Ok(host_addr);
                }
                src = unsafe { src.add(write_len as usize) };
                vm_addr = vm_addr.saturating_add(write_len);
                region = match self.find_region(cache, vm_addr) {
                    Some(region) => region,
                    None => break,
                };
            } else {
                break;
            }
        }

        generate_access_violation(
            self.config,
            self.sbpf_version,
            AccessType::Store,
            initial_vm_addr,
            initial_len,
        )
    }

    /// Returns the `MemoryRegion` corresponding to the given address.
    pub fn region(
        &self,
        access_type: AccessType,
        vm_addr: u64,
    ) -> Result<&MemoryRegion, EbpfError> {
        // Safety:
        // &mut references to the mapping cache are only created internally from methods that do not
        // invoke each other. UnalignedMemoryMapping is !Sync, so the cache reference below is
        // guaranteed to be unique.
        let cache = unsafe { &mut *self.cache.get() };
        if let Some(region) = self.find_region(cache, vm_addr) {
            if (region.vm_addr..region.vm_addr_end).contains(&vm_addr)
                && (access_type == AccessType::Load || ensure_writable_region(region, &self.cow_cb))
            {
                return Ok(region);
            }
        }
        Err(
            generate_access_violation(self.config, self.sbpf_version, access_type, vm_addr, 0)
                .unwrap_err(),
        )
    }

    /// Returns the `MemoryRegion`s in this mapping
    pub fn get_regions(&self) -> &[MemoryRegion] {
        &self.regions
    }

    /// Replaces the `MemoryRegion` at the given index
    pub fn replace_region(&mut self, index: usize, region: MemoryRegion) -> Result<(), EbpfError> {
        if index >= self.regions.len() || self.regions[index].vm_addr != region.vm_addr {
            return Err(EbpfError::InvalidMemoryRegion(index));
        }
        self.regions[index] = region;
        self.cache.get_mut().flush();
        Ok(())
    }
}

/// Memory mapping that uses the upper half of an address to identify the
/// underlying memory region.
pub struct AlignedMemoryMapping<'a> {
    /// Mapped memory regions
    regions: Box<[MemoryRegion]>,
    /// VM configuration
    config: &'a Config,
    /// Executable sbpf_version
    sbpf_version: &'a SBPFVersion,
    /// CoW callback
    cow_cb: Option<MemoryCowCallback>,
}

impl<'a> fmt::Debug for AlignedMemoryMapping<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AlignedMemoryMapping")
            .field("regions", &self.regions)
            .field("config", &self.config)
            .field(
                "cow_cb",
                &self
                    .cow_cb
                    .as_ref()
                    .map(|cb| format!("Some({:p})", &cb))
                    .unwrap_or_else(|| "None".to_string()),
            )
            .finish()
    }
}

impl<'a> AlignedMemoryMapping<'a> {
    fn new_internal(
        mut regions: Vec<MemoryRegion>,
        cow_cb: Option<MemoryCowCallback>,
        config: &'a Config,
        sbpf_version: &'a SBPFVersion,
    ) -> Result<Self, EbpfError> {
        regions.insert(0, MemoryRegion::new_readonly(&[], 0));
        regions.sort();
        for (index, region) in regions.iter().enumerate() {
            if region
                .vm_addr
                .checked_shr(ebpf::VIRTUAL_ADDRESS_BITS as u32)
                .unwrap_or(0)
                != index as u64
            {
                return Err(EbpfError::InvalidMemoryRegion(index));
            }
        }
        Ok(Self {
            regions: regions.into_boxed_slice(),
            config,
            sbpf_version,
            cow_cb,
        })
    }

    /// Creates a new MemoryMapping structure from the given regions
    pub fn new(
        regions: Vec<MemoryRegion>,
        config: &'a Config,
        sbpf_version: &'a SBPFVersion,
    ) -> Result<Self, EbpfError> {
        Self::new_internal(regions, None, config, sbpf_version)
    }

    /// Creates a new MemoryMapping structure from the given regions.
    ///
    /// `cow_cb` is used to copy CoW regions on the first write access.
    pub fn new_with_cow(
        regions: Vec<MemoryRegion>,
        cow_cb: MemoryCowCallback,
        config: &'a Config,
        sbpf_version: &'a SBPFVersion,
    ) -> Result<Self, EbpfError> {
        Self::new_internal(regions, Some(cow_cb), config, sbpf_version)
    }

    /// Given a list of regions translate from virtual machine to host address
    pub fn map(&self, access_type: AccessType, vm_addr: u64, len: u64) -> ProgramResult {
        let index = vm_addr
            .checked_shr(ebpf::VIRTUAL_ADDRESS_BITS as u32)
            .unwrap_or(0) as usize;
        if (1..self.regions.len()).contains(&index) {
            let region = &self.regions[index];
            if access_type == AccessType::Load || ensure_writable_region(region, &self.cow_cb) {
                if let ProgramResult::Ok(host_addr) = region.vm_to_host(vm_addr, len) {
                    return ProgramResult::Ok(host_addr);
                }
            }
        }
        generate_access_violation(self.config, self.sbpf_version, access_type, vm_addr, len)
    }

    /// Loads `size_of::<T>()` bytes from the given address.
    ///
    /// See [MemoryMapping::load].
    #[inline]
    pub fn load<T: Pod + Into<u64>>(&self, vm_addr: u64) -> ProgramResult {
        let len = mem::size_of::<T>() as u64;
        match self.map(AccessType::Load, vm_addr, len) {
            ProgramResult::Ok(host_addr) => {
                ProgramResult::Ok(unsafe { ptr::read_unaligned::<T>(host_addr as *const _) }.into())
            }
            err => err,
        }
    }

    /// Store `value` at the given address.
    ///
    /// See [MemoryMapping::store].
    #[inline]
    pub fn store<T: Pod>(&self, value: T, vm_addr: u64) -> ProgramResult {
        let len = mem::size_of::<T>() as u64;
        debug_assert!(len <= mem::size_of::<u64>() as u64);

        match self.map(AccessType::Store, vm_addr, len) {
            ProgramResult::Ok(host_addr) => {
                // Safety:
                // map succeeded so we can write at least `len` bytes
                unsafe {
                    ptr::write_unaligned(host_addr as *mut T, value);
                }
                ProgramResult::Ok(host_addr)
            }

            err => err,
        }
    }

    /// Returns the `MemoryRegion` corresponding to the given address.
    pub fn region(
        &self,
        access_type: AccessType,
        vm_addr: u64,
    ) -> Result<&MemoryRegion, EbpfError> {
        let index = vm_addr
            .checked_shr(ebpf::VIRTUAL_ADDRESS_BITS as u32)
            .unwrap_or(0) as usize;
        if (1..self.regions.len()).contains(&index) {
            let region = &self.regions[index];
            if (region.vm_addr..region.vm_addr_end).contains(&vm_addr)
                && (access_type == AccessType::Load || ensure_writable_region(region, &self.cow_cb))
            {
                return Ok(region);
            }
        }
        Err(
            generate_access_violation(self.config, self.sbpf_version, access_type, vm_addr, 0)
                .unwrap_err(),
        )
    }

    /// Returns the `MemoryRegion`s in this mapping
    pub fn get_regions(&self) -> &[MemoryRegion] {
        &self.regions
    }

    /// Replaces the `MemoryRegion` at the given index
    pub fn replace_region(&mut self, index: usize, region: MemoryRegion) -> Result<(), EbpfError> {
        if index >= self.regions.len() {
            return Err(EbpfError::InvalidMemoryRegion(index));
        }
        let begin_index = region
            .vm_addr
            .checked_shr(ebpf::VIRTUAL_ADDRESS_BITS as u32)
            .unwrap_or(0) as usize;
        let end_index = region
            .vm_addr
            .saturating_add(region.len.saturating_sub(1))
            .checked_shr(ebpf::VIRTUAL_ADDRESS_BITS as u32)
            .unwrap_or(0) as usize;
        if begin_index != index || end_index != index {
            return Err(EbpfError::InvalidMemoryRegion(index));
        }
        self.regions[index] = region;
        Ok(())
    }
}

/// Maps virtual memory to host memory.
#[derive(Debug)]
pub enum MemoryMapping<'a> {
    /// Used when address translation is disabled
    Identity,
    /// Aligned memory mapping which uses the upper half of an address to
    /// identify the underlying memory region.
    Aligned(AlignedMemoryMapping<'a>),
    /// Memory mapping that allows mapping unaligned memory regions.
    Unaligned(UnalignedMemoryMapping<'a>),
}

impl<'a> MemoryMapping<'a> {
    pub(crate) fn new_identity() -> Self {
        MemoryMapping::Identity
    }

    /// Creates a new memory mapping.
    ///
    /// Uses aligned or unaligned memory mapping depending on the value of
    /// `config.aligned_memory_mapping=true`.
    pub fn new(
        regions: Vec<MemoryRegion>,
        config: &'a Config,
        sbpf_version: &'a SBPFVersion,
    ) -> Result<Self, EbpfError> {
        if config.aligned_memory_mapping {
            AlignedMemoryMapping::new(regions, config, sbpf_version).map(MemoryMapping::Aligned)
        } else {
            UnalignedMemoryMapping::new(regions, config, sbpf_version).map(MemoryMapping::Unaligned)
        }
    }

    /// Creates a new memory mapping.
    ///
    /// Uses aligned or unaligned memory mapping depending on the value of
    /// `config.aligned_memory_mapping=true`. `cow_cb` is used to copy CoW memory regions.
    pub fn new_with_cow(
        regions: Vec<MemoryRegion>,
        cow_cb: MemoryCowCallback,
        config: &'a Config,
        sbpf_version: &'a SBPFVersion,
    ) -> Result<Self, EbpfError> {
        if config.aligned_memory_mapping {
            AlignedMemoryMapping::new_with_cow(regions, cow_cb, config, sbpf_version)
                .map(MemoryMapping::Aligned)
        } else {
            UnalignedMemoryMapping::new_with_cow(regions, cow_cb, config, sbpf_version)
                .map(MemoryMapping::Unaligned)
        }
    }

    /// Map virtual memory to host memory.
    pub fn map(&self, access_type: AccessType, vm_addr: u64, len: u64) -> ProgramResult {
        match self {
            MemoryMapping::Identity => ProgramResult::Ok(vm_addr),
            MemoryMapping::Aligned(m) => m.map(access_type, vm_addr, len),
            MemoryMapping::Unaligned(m) => m.map(access_type, vm_addr, len),
        }
    }

    /// Loads `size_of::<T>()` bytes from the given address.
    ///
    /// Works across memory region boundaries.
    #[inline]
    pub fn load<T: Pod + Into<u64>>(&self, vm_addr: u64) -> ProgramResult {
        match self {
            MemoryMapping::Identity => unsafe {
                ProgramResult::Ok(ptr::read_unaligned(vm_addr as *const T).into())
            },
            MemoryMapping::Aligned(m) => m.load::<T>(vm_addr),
            MemoryMapping::Unaligned(m) => m.load::<T>(vm_addr),
        }
    }

    /// Store `value` at the given address.
    ///
    /// Works across memory region boundaries if `len` does not fit within a single region.
    #[inline]
    pub fn store<T: Pod>(&self, value: T, vm_addr: u64) -> ProgramResult {
        match self {
            MemoryMapping::Identity => unsafe {
                ptr::write_unaligned(vm_addr as *mut T, value);
                ProgramResult::Ok(0)
            },
            MemoryMapping::Aligned(m) => m.store(value, vm_addr),
            MemoryMapping::Unaligned(m) => m.store(value, vm_addr),
        }
    }

    /// Returns the `MemoryRegion` corresponding to the given address.
    pub fn region(
        &self,
        access_type: AccessType,
        vm_addr: u64,
    ) -> Result<&MemoryRegion, EbpfError> {
        match self {
            MemoryMapping::Identity => Err(EbpfError::InvalidMemoryRegion(0)),
            MemoryMapping::Aligned(m) => m.region(access_type, vm_addr),
            MemoryMapping::Unaligned(m) => m.region(access_type, vm_addr),
        }
    }

    /// Returns the `MemoryRegion`s in this mapping.
    pub fn get_regions(&self) -> &[MemoryRegion] {
        match self {
            MemoryMapping::Identity => &[],
            MemoryMapping::Aligned(m) => m.get_regions(),
            MemoryMapping::Unaligned(m) => m.get_regions(),
        }
    }

    /// Replaces the `MemoryRegion` at the given index
    pub fn replace_region(&mut self, index: usize, region: MemoryRegion) -> Result<(), EbpfError> {
        match self {
            MemoryMapping::Identity => Err(EbpfError::InvalidMemoryRegion(index)),
            MemoryMapping::Aligned(m) => m.replace_region(index, region),
            MemoryMapping::Unaligned(m) => m.replace_region(index, region),
        }
    }
}

// Ensure that the given region is writable.
//
// If the region is CoW, cow_cb is called to execute the CoW operation.
fn ensure_writable_region(region: &MemoryRegion, cow_cb: &Option<MemoryCowCallback>) -> bool {
    match (region.state.get(), cow_cb) {
        (MemoryState::Writable, _) => true,
        (MemoryState::Cow(cow_id), Some(cb)) => match cb(cow_id) {
            Ok(host_addr) => {
                region.host_addr.replace(host_addr);
                region.state.replace(MemoryState::Writable);
                true
            }
            Err(_) => false,
        },
        _ => false,
    }
}

/// Helper for map to generate errors
fn generate_access_violation(
    config: &Config,
    sbpf_version: &SBPFVersion,
    access_type: AccessType,
    vm_addr: u64,
    len: u64,
) -> ProgramResult {
    let stack_frame = (vm_addr as i64)
        .saturating_sub(ebpf::MM_STACK_START as i64)
        .checked_div(config.stack_frame_size as i64)
        .unwrap_or(0);
    if !sbpf_version.dynamic_stack_frames()
        && (-1..(config.max_call_depth as i64).saturating_add(1)).contains(&stack_frame)
    {
        ProgramResult::Err(EbpfError::StackAccessViolation(
            access_type,
            vm_addr,
            len,
            stack_frame,
        ))
    } else {
        let region_name = match vm_addr & (!ebpf::MM_PROGRAM_START.saturating_sub(1)) {
            ebpf::MM_PROGRAM_START => "program",
            ebpf::MM_STACK_START => "stack",
            ebpf::MM_HEAP_START => "heap",
            ebpf::MM_INPUT_START => "input",
            _ => "unknown",
        };
        ProgramResult::Err(EbpfError::AccessViolation(
            access_type,
            vm_addr,
            len,
            region_name,
        ))
    }
}

/// Fast, small linear cache used to speed up unaligned memory mapping.
#[derive(Debug)]
struct MappingCache {
    // The cached entries.
    entries: [(Range<u64>, usize); MappingCache::SIZE as usize],
    // Index of the last accessed memory region.
    //
    // New entries are written backwards, so that find() can always scan
    // forward which is faster.
    head: isize,
}

impl MappingCache {
    const SIZE: isize = 4;

    fn new() -> MappingCache {
        MappingCache {
            entries: array::from_fn(|_| (0..0, 0)),
            head: 0,
        }
    }

    #[allow(clippy::arithmetic_side_effects)]
    #[inline]
    fn find(&self, vm_addr: u64) -> Option<usize> {
        for i in 0..Self::SIZE {
            let index = (self.head + i) % Self::SIZE;
            // Safety:
            // index is guaranteed to be between 0..Self::SIZE
            let (vm_range, region_index) = unsafe { self.entries.get_unchecked(index as usize) };
            if vm_range.contains(&vm_addr) {
                return Some(*region_index);
            }
        }

        None
    }

    #[allow(clippy::arithmetic_side_effects)]
    #[inline]
    fn insert(&mut self, vm_range: Range<u64>, region_index: usize) {
        self.head = (self.head - 1).rem_euclid(Self::SIZE);
        // Safety:
        // self.head is guaranteed to be between 0..Self::SIZE
        unsafe { *self.entries.get_unchecked_mut(self.head as usize) = (vm_range, region_index) };
    }

    #[inline]
    fn flush(&mut self) {
        self.entries = array::from_fn(|_| (0..0, 0));
        self.head = 0;
    }
}

#[cfg(test)]
mod test {
    use std::{cell::RefCell, rc::Rc};
    use test_utils::assert_error;

    use super::*;

    #[test]
    fn test_mapping_cache() {
        let mut cache = MappingCache::new();
        assert_eq!(cache.find(0), None);

        let mut ranges = vec![10u64..20, 20..30, 30..40, 40..50];
        for (region, range) in ranges.iter().cloned().enumerate() {
            cache.insert(range, region);
        }
        for (region, range) in ranges.iter().enumerate() {
            if region > 0 {
                assert_eq!(cache.find(range.start - 1), Some(region - 1));
            } else {
                assert_eq!(cache.find(range.start - 1), None);
            }
            assert_eq!(cache.find(range.start), Some(region));
            assert_eq!(cache.find(range.start + 1), Some(region));
            assert_eq!(cache.find(range.end - 1), Some(region));
            if region < 3 {
                assert_eq!(cache.find(range.end), Some(region + 1));
            } else {
                assert_eq!(cache.find(range.end), None);
            }
        }

        cache.insert(50..60, 4);
        ranges.push(50..60);
        for (region, range) in ranges.iter().enumerate() {
            if region == 0 {
                assert_eq!(cache.find(range.start), None);
                continue;
            }
            if region > 1 {
                assert_eq!(cache.find(range.start - 1), Some(region - 1));
            } else {
                assert_eq!(cache.find(range.start - 1), None);
            }
            assert_eq!(cache.find(range.start), Some(region));
            assert_eq!(cache.find(range.start + 1), Some(region));
            assert_eq!(cache.find(range.end - 1), Some(region));
            if region < 4 {
                assert_eq!(cache.find(range.end), Some(region + 1));
            } else {
                assert_eq!(cache.find(range.end), None);
            }
        }
    }

    #[test]
    fn test_mapping_cache_flush() {
        let mut cache = MappingCache::new();
        assert_eq!(cache.find(0), None);
        cache.insert(0..10, 0);
        assert_eq!(cache.find(0), Some(0));
        cache.flush();
        assert_eq!(cache.find(0), None);
    }

    #[test]
    fn test_map_empty() {
        let config = Config::default();
        let m = UnalignedMemoryMapping::new(vec![], &config, &SBPFVersion::V2).unwrap();
        assert_error!(
            m.map(AccessType::Load, ebpf::MM_INPUT_START, 8),
            "AccessViolation"
        );

        let m = AlignedMemoryMapping::new(vec![], &config, &SBPFVersion::V2).unwrap();
        assert_error!(
            m.map(AccessType::Load, ebpf::MM_INPUT_START, 8),
            "AccessViolation"
        );
    }

    #[test]
    fn test_gapped_map() {
        for aligned_memory_mapping in [false, true] {
            let config = Config {
                aligned_memory_mapping,
                ..Config::default()
            };
            let mut mem1 = vec![0xff; 8];
            let m = MemoryMapping::new(
                vec![
                    MemoryRegion::new_readonly(&[0; 8], ebpf::MM_PROGRAM_START),
                    MemoryRegion::new_writable_gapped(&mut mem1, ebpf::MM_STACK_START, 2),
                ],
                &config,
                &SBPFVersion::V2,
            )
            .unwrap();
            for frame in 0..4 {
                let address = ebpf::MM_STACK_START + frame * 4;
                assert!(m.region(AccessType::Load, address).is_ok());
                assert!(m.map(AccessType::Load, address, 2).is_ok());
                assert_error!(m.map(AccessType::Load, address + 2, 2), "AccessViolation");
                assert_eq!(m.load::<u16>(address).unwrap(), 0xFFFF);
                assert_error!(m.load::<u16>(address + 2), "AccessViolation");
                assert!(m.store::<u16>(0xFFFF, address).is_ok());
                assert_error!(m.store::<u16>(0xFFFF, address + 2), "AccessViolation");
            }
        }
    }

    #[test]
    fn test_unaligned_map_overlap() {
        let config = Config::default();
        let mem1 = [1, 2, 3, 4];
        let mem2 = [5, 6];
        assert_error!(
            UnalignedMemoryMapping::new(
                vec![
                    MemoryRegion::new_readonly(&mem1, ebpf::MM_INPUT_START),
                    MemoryRegion::new_readonly(&mem2, ebpf::MM_INPUT_START + mem1.len() as u64 - 1),
                ],
                &config,
                &SBPFVersion::V2,
            ),
            "InvalidMemoryRegion(1)"
        );
        assert!(UnalignedMemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&mem1, ebpf::MM_INPUT_START),
                MemoryRegion::new_readonly(&mem2, ebpf::MM_INPUT_START + mem1.len() as u64),
            ],
            &config,
            &SBPFVersion::V2,
        )
        .is_ok());
    }

    #[test]
    fn test_unaligned_map() {
        let config = Config::default();
        let mut mem1 = [11];
        let mem2 = [22, 22];
        let mem3 = [33];
        let mem4 = [44, 44];
        let m = UnalignedMemoryMapping::new(
            vec![
                MemoryRegion::new_writable(&mut mem1, ebpf::MM_INPUT_START),
                MemoryRegion::new_readonly(&mem2, ebpf::MM_INPUT_START + mem1.len() as u64),
                MemoryRegion::new_readonly(
                    &mem3,
                    ebpf::MM_INPUT_START + (mem1.len() + mem2.len()) as u64,
                ),
                MemoryRegion::new_readonly(
                    &mem4,
                    ebpf::MM_INPUT_START + (mem1.len() + mem2.len() + mem3.len()) as u64,
                ),
            ],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();

        assert_eq!(
            m.map(AccessType::Load, ebpf::MM_INPUT_START, 1).unwrap(),
            mem1.as_ptr() as u64
        );

        assert_eq!(
            m.map(AccessType::Store, ebpf::MM_INPUT_START, 1).unwrap(),
            mem1.as_ptr() as u64
        );

        assert_error!(
            m.map(AccessType::Load, ebpf::MM_INPUT_START, 2),
            "AccessViolation"
        );

        assert_eq!(
            m.map(
                AccessType::Load,
                ebpf::MM_INPUT_START + mem1.len() as u64,
                1,
            )
            .unwrap(),
            mem2.as_ptr() as u64
        );

        assert_eq!(
            m.map(
                AccessType::Load,
                ebpf::MM_INPUT_START + (mem1.len() + mem2.len()) as u64,
                1,
            )
            .unwrap(),
            mem3.as_ptr() as u64
        );

        assert_eq!(
            m.map(
                AccessType::Load,
                ebpf::MM_INPUT_START + (mem1.len() + mem2.len() + mem3.len()) as u64,
                1,
            )
            .unwrap(),
            mem4.as_ptr() as u64
        );

        assert_error!(
            m.map(
                AccessType::Load,
                ebpf::MM_INPUT_START + (mem1.len() + mem2.len() + mem3.len() + mem4.len()) as u64,
                1,
            ),
            "AccessViolation"
        );
    }

    #[test]
    fn test_unaligned_region() {
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };

        let mut mem1 = vec![0xFF; 4];
        let mem2 = vec![0xDD; 4];
        let m = MemoryMapping::new(
            vec![
                MemoryRegion::new_writable(&mut mem1, ebpf::MM_INPUT_START),
                MemoryRegion::new_readonly(&mem2, ebpf::MM_INPUT_START + 4),
            ],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();
        assert_error!(
            m.region(AccessType::Load, ebpf::MM_INPUT_START - 1),
            "AccessViolation"
        );
        assert_eq!(
            m.region(AccessType::Load, ebpf::MM_INPUT_START)
                .unwrap()
                .host_addr
                .get(),
            mem1.as_ptr() as u64
        );
        assert_eq!(
            m.region(AccessType::Load, ebpf::MM_INPUT_START + 3)
                .unwrap()
                .host_addr
                .get(),
            mem1.as_ptr() as u64
        );
        assert_error!(
            m.region(AccessType::Store, ebpf::MM_INPUT_START + 4),
            "AccessViolation"
        );
        assert_eq!(
            m.region(AccessType::Load, ebpf::MM_INPUT_START + 4)
                .unwrap()
                .host_addr
                .get(),
            mem2.as_ptr() as u64
        );
        assert_eq!(
            m.region(AccessType::Load, ebpf::MM_INPUT_START + 7)
                .unwrap()
                .host_addr
                .get(),
            mem2.as_ptr() as u64
        );
        assert_error!(
            m.region(AccessType::Load, ebpf::MM_INPUT_START + 8),
            "AccessViolation"
        );
    }

    #[test]
    fn test_aligned_region() {
        let config = Config {
            aligned_memory_mapping: true,
            ..Config::default()
        };

        let mut mem1 = vec![0xFF; 4];
        let mem2 = vec![0xDD; 4];
        let m = MemoryMapping::new(
            vec![
                MemoryRegion::new_writable(&mut mem1, ebpf::MM_PROGRAM_START),
                MemoryRegion::new_readonly(&mem2, ebpf::MM_STACK_START),
            ],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();
        assert_error!(
            m.region(AccessType::Load, ebpf::MM_PROGRAM_START - 1),
            "AccessViolation"
        );
        assert_eq!(
            m.region(AccessType::Load, ebpf::MM_PROGRAM_START)
                .unwrap()
                .host_addr
                .get(),
            mem1.as_ptr() as u64
        );
        assert_eq!(
            m.region(AccessType::Load, ebpf::MM_PROGRAM_START + 3)
                .unwrap()
                .host_addr
                .get(),
            mem1.as_ptr() as u64
        );
        assert_error!(
            m.region(AccessType::Load, ebpf::MM_PROGRAM_START + 4),
            "AccessViolation"
        );

        assert_error!(
            m.region(AccessType::Store, ebpf::MM_STACK_START),
            "AccessViolation"
        );
        assert_eq!(
            m.region(AccessType::Load, ebpf::MM_STACK_START)
                .unwrap()
                .host_addr
                .get(),
            mem2.as_ptr() as u64
        );
        assert_eq!(
            m.region(AccessType::Load, ebpf::MM_STACK_START + 3)
                .unwrap()
                .host_addr
                .get(),
            mem2.as_ptr() as u64
        );
        assert_error!(
            m.region(AccessType::Load, ebpf::MM_INPUT_START + 4),
            "AccessViolation"
        );
    }

    #[test]
    fn test_unaligned_map_load() {
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let mem1 = [0x11, 0x22];
        let mem2 = [0x33];
        let mem3 = [0x44, 0x55, 0x66];
        let mem4 = [0x77, 0x88, 0x99];
        let m = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&mem1, ebpf::MM_INPUT_START),
                MemoryRegion::new_readonly(&mem2, ebpf::MM_INPUT_START + mem1.len() as u64),
                MemoryRegion::new_readonly(
                    &mem3,
                    ebpf::MM_INPUT_START + (mem1.len() + mem2.len()) as u64,
                ),
                MemoryRegion::new_readonly(
                    &mem4,
                    ebpf::MM_INPUT_START + (mem1.len() + mem2.len() + mem3.len()) as u64,
                ),
            ],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();

        assert_eq!(m.load::<u16>(ebpf::MM_INPUT_START).unwrap(), 0x2211);
        assert_eq!(m.load::<u32>(ebpf::MM_INPUT_START).unwrap(), 0x44332211);
        assert_eq!(
            m.load::<u64>(ebpf::MM_INPUT_START).unwrap(),
            0x8877665544332211
        );
        assert_eq!(m.load::<u16>(ebpf::MM_INPUT_START + 1).unwrap(), 0x3322);
        assert_eq!(m.load::<u32>(ebpf::MM_INPUT_START + 1).unwrap(), 0x55443322);
        assert_eq!(
            m.load::<u64>(ebpf::MM_INPUT_START + 1).unwrap(),
            0x9988776655443322
        );
    }

    #[test]
    fn test_unaligned_map_store() {
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let mut mem1 = vec![0xff, 0xff];
        let mut mem2 = vec![0xff];
        let mut mem3 = vec![0xff, 0xff, 0xff];
        let mut mem4 = vec![0xff, 0xff];
        let m = MemoryMapping::new(
            vec![
                MemoryRegion::new_writable(&mut mem1, ebpf::MM_INPUT_START),
                MemoryRegion::new_writable(&mut mem2, ebpf::MM_INPUT_START + mem1.len() as u64),
                MemoryRegion::new_writable(
                    &mut mem3,
                    ebpf::MM_INPUT_START + (mem1.len() + mem2.len()) as u64,
                ),
                MemoryRegion::new_writable(
                    &mut mem4,
                    ebpf::MM_INPUT_START + (mem1.len() + mem2.len() + mem3.len()) as u64,
                ),
            ],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();
        m.store(0x1122u16, ebpf::MM_INPUT_START).unwrap();
        assert_eq!(m.load::<u16>(ebpf::MM_INPUT_START).unwrap(), 0x1122);

        m.store(0x33445566u32, ebpf::MM_INPUT_START).unwrap();
        assert_eq!(m.load::<u32>(ebpf::MM_INPUT_START).unwrap(), 0x33445566);

        m.store(0x778899AABBCCDDEEu64, ebpf::MM_INPUT_START)
            .unwrap();
        assert_eq!(
            m.load::<u64>(ebpf::MM_INPUT_START).unwrap(),
            0x778899AABBCCDDEE
        );
    }

    #[test]
    fn test_unaligned_map_load_store_fast_paths() {
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let mut mem1 = vec![0xff; 8];
        let m = MemoryMapping::new(
            vec![MemoryRegion::new_writable(&mut mem1, ebpf::MM_INPUT_START)],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();

        m.store(0x1122334455667788u64, ebpf::MM_INPUT_START)
            .unwrap();
        assert_eq!(
            m.load::<u64>(ebpf::MM_INPUT_START).unwrap(),
            0x1122334455667788
        );
        m.store(0x22334455u32, ebpf::MM_INPUT_START).unwrap();
        assert_eq!(m.load::<u32>(ebpf::MM_INPUT_START).unwrap(), 0x22334455);

        m.store(0x3344u16, ebpf::MM_INPUT_START).unwrap();
        assert_eq!(m.load::<u16>(ebpf::MM_INPUT_START).unwrap(), 0x3344);

        m.store(0x55u8, ebpf::MM_INPUT_START).unwrap();
        assert_eq!(m.load::<u8>(ebpf::MM_INPUT_START).unwrap(), 0x55);
    }

    #[test]
    fn test_unaligned_map_load_store_slow_paths() {
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let mut mem1 = vec![0xff; 7];
        let mut mem2 = vec![0xff];
        let m = MemoryMapping::new(
            vec![
                MemoryRegion::new_writable(&mut mem1, ebpf::MM_INPUT_START),
                MemoryRegion::new_writable(&mut mem2, ebpf::MM_INPUT_START + 7),
            ],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();

        m.store(0x1122334455667788u64, ebpf::MM_INPUT_START)
            .unwrap();
        assert_eq!(
            m.load::<u64>(ebpf::MM_INPUT_START).unwrap(),
            0x1122334455667788
        );
        m.store(0xAABBCCDDu32, ebpf::MM_INPUT_START + 4).unwrap();
        assert_eq!(m.load::<u32>(ebpf::MM_INPUT_START + 4).unwrap(), 0xAABBCCDD);

        m.store(0xEEFFu16, ebpf::MM_INPUT_START + 6).unwrap();
        assert_eq!(m.load::<u16>(ebpf::MM_INPUT_START + 6).unwrap(), 0xEEFF);
    }

    #[test]
    fn test_unaligned_map_store_out_of_bounds() {
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };

        let mut mem1 = vec![0xFF];
        let m = MemoryMapping::new(
            vec![MemoryRegion::new_writable(&mut mem1, ebpf::MM_INPUT_START)],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();
        m.store(0x11u8, ebpf::MM_INPUT_START).unwrap();
        assert_error!(m.store(0x11u8, ebpf::MM_INPUT_START - 1), "AccessViolation");
        assert_error!(m.store(0x11u8, ebpf::MM_INPUT_START + 1), "AccessViolation");
        // this gets us line coverage for the case where we're completely
        // outside the address space (the case above is just on the edge)
        assert_error!(m.store(0x11u8, ebpf::MM_INPUT_START + 2), "AccessViolation");

        let mut mem1 = vec![0xFF; 4];
        let mut mem2 = vec![0xDD; 4];
        let m = MemoryMapping::new(
            vec![
                MemoryRegion::new_writable(&mut mem1, ebpf::MM_INPUT_START),
                MemoryRegion::new_writable(&mut mem2, ebpf::MM_INPUT_START + 4),
            ],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();
        m.store(0x1122334455667788u64, ebpf::MM_INPUT_START)
            .unwrap();
        assert_eq!(
            m.load::<u64>(ebpf::MM_INPUT_START).unwrap(),
            0x1122334455667788u64
        );
        assert_error!(
            m.store(0x1122334455667788u64, ebpf::MM_INPUT_START + 1),
            "AccessViolation"
        );
    }

    #[test]
    fn test_unaligned_map_load_out_of_bounds() {
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };

        let mem1 = vec![0xff];
        let m = MemoryMapping::new(
            vec![MemoryRegion::new_readonly(&mem1, ebpf::MM_INPUT_START)],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();
        assert_eq!(m.load::<u8>(ebpf::MM_INPUT_START).unwrap(), 0xff);
        assert_error!(m.load::<u8>(ebpf::MM_INPUT_START - 1), "AccessViolation");
        assert_error!(m.load::<u8>(ebpf::MM_INPUT_START + 1), "AccessViolation");
        assert_error!(m.load::<u8>(ebpf::MM_INPUT_START + 2), "AccessViolation");

        let mem1 = vec![0xFF; 4];
        let mem2 = vec![0xDD; 4];
        let m = MemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&mem1, ebpf::MM_INPUT_START),
                MemoryRegion::new_readonly(&mem2, ebpf::MM_INPUT_START + 4),
            ],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();
        assert_eq!(
            m.load::<u64>(ebpf::MM_INPUT_START).unwrap(),
            0xDDDDDDDDFFFFFFFF
        );
        assert_error!(m.load::<u64>(ebpf::MM_INPUT_START + 1), "AccessViolation");
    }

    #[test]
    #[should_panic(expected = "AccessViolation")]
    fn test_store_readonly() {
        let config = Config {
            aligned_memory_mapping: false,
            ..Config::default()
        };
        let mut mem1 = vec![0xff, 0xff];
        let mem2 = vec![0xff, 0xff];
        let m = MemoryMapping::new(
            vec![
                MemoryRegion::new_writable(&mut mem1, ebpf::MM_INPUT_START),
                MemoryRegion::new_readonly(&mem2, ebpf::MM_INPUT_START + mem1.len() as u64),
            ],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();
        m.store(0x11223344, ebpf::MM_INPUT_START).unwrap();
    }

    #[test]
    fn test_unaligned_map_replace_region() {
        let config = Config::default();
        let mem1 = [11];
        let mem2 = [22, 22];
        let mem3 = [33];
        let mut m = UnalignedMemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&mem1, ebpf::MM_INPUT_START),
                MemoryRegion::new_readonly(&mem2, ebpf::MM_INPUT_START + mem1.len() as u64),
            ],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();

        assert_eq!(
            m.map(AccessType::Load, ebpf::MM_INPUT_START, 1).unwrap(),
            mem1.as_ptr() as u64
        );

        assert_eq!(
            m.map(
                AccessType::Load,
                ebpf::MM_INPUT_START + mem1.len() as u64,
                1,
            )
            .unwrap(),
            mem2.as_ptr() as u64
        );

        assert_error!(
            m.replace_region(
                2,
                MemoryRegion::new_readonly(&mem3, ebpf::MM_INPUT_START + mem1.len() as u64)
            ),
            "InvalidMemoryRegion(2)"
        );

        let region_index = m
            .get_regions()
            .iter()
            .position(|mem| mem.vm_addr == ebpf::MM_INPUT_START + mem1.len() as u64)
            .unwrap();

        // old.vm_addr != new.vm_addr
        assert_error!(
            m.replace_region(
                region_index,
                MemoryRegion::new_readonly(&mem3, ebpf::MM_INPUT_START + mem1.len() as u64 + 1)
            ),
            "InvalidMemoryRegion({})",
            region_index
        );

        m.replace_region(
            region_index,
            MemoryRegion::new_readonly(&mem3, ebpf::MM_INPUT_START + mem1.len() as u64),
        )
        .unwrap();

        assert_eq!(
            m.map(
                AccessType::Load,
                ebpf::MM_INPUT_START + mem1.len() as u64,
                1,
            )
            .unwrap(),
            mem3.as_ptr() as u64
        );
    }

    #[test]
    fn test_aligned_map_replace_region() {
        let config = Config::default();
        let mem1 = [11];
        let mem2 = [22, 22];
        let mem3 = [33, 33];
        let mut m = AlignedMemoryMapping::new(
            vec![
                MemoryRegion::new_readonly(&mem1, ebpf::MM_PROGRAM_START),
                MemoryRegion::new_readonly(&mem2, ebpf::MM_STACK_START),
            ],
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();

        assert_eq!(
            m.map(AccessType::Load, ebpf::MM_STACK_START, 1).unwrap(),
            mem2.as_ptr() as u64
        );

        // index > regions.len()
        assert_error!(
            m.replace_region(3, MemoryRegion::new_readonly(&mem3, ebpf::MM_STACK_START)),
            "InvalidMemoryRegion(3)"
        );

        // index != addr >> VIRTUAL_ADDRESS_BITS
        assert_error!(
            m.replace_region(2, MemoryRegion::new_readonly(&mem3, ebpf::MM_HEAP_START)),
            "InvalidMemoryRegion(2)"
        );

        // index + len != addr >> VIRTUAL_ADDRESS_BITS
        assert_error!(
            m.replace_region(
                2,
                MemoryRegion::new_readonly(&mem3, ebpf::MM_HEAP_START - 1)
            ),
            "InvalidMemoryRegion(2)"
        );

        m.replace_region(2, MemoryRegion::new_readonly(&mem3, ebpf::MM_STACK_START))
            .unwrap();

        assert_eq!(
            m.map(AccessType::Load, ebpf::MM_STACK_START, 1).unwrap(),
            mem3.as_ptr() as u64
        );
    }

    #[test]
    fn test_cow_map() {
        for aligned_memory_mapping in [true, false] {
            let config = Config {
                aligned_memory_mapping,
                ..Config::default()
            };
            let original = [11, 22];
            let copied = Rc::new(RefCell::new(Vec::new()));

            let c = Rc::clone(&copied);
            let m = MemoryMapping::new_with_cow(
                vec![MemoryRegion::new_cow(&original, ebpf::MM_PROGRAM_START, 42)],
                Box::new(move |_| {
                    c.borrow_mut().extend_from_slice(&original);
                    Ok(c.borrow().as_slice().as_ptr() as u64)
                }),
                &config,
                &SBPFVersion::V2,
            )
            .unwrap();

            assert_eq!(
                m.map(AccessType::Load, ebpf::MM_PROGRAM_START, 1).unwrap(),
                original.as_ptr() as u64
            );
            assert_eq!(
                m.map(AccessType::Store, ebpf::MM_PROGRAM_START, 1).unwrap(),
                copied.borrow().as_ptr() as u64
            );
        }
    }

    #[test]
    fn test_cow_load_store() {
        for aligned_memory_mapping in [true, false] {
            let config = Config {
                aligned_memory_mapping,
                ..Config::default()
            };
            let original = [11, 22];
            let copied = Rc::new(RefCell::new(Vec::new()));

            let c = Rc::clone(&copied);
            let m = MemoryMapping::new_with_cow(
                vec![MemoryRegion::new_cow(&original, ebpf::MM_PROGRAM_START, 42)],
                Box::new(move |_| {
                    c.borrow_mut().extend_from_slice(&original);
                    Ok(c.borrow().as_slice().as_ptr() as u64)
                }),
                &config,
                &SBPFVersion::V2,
            )
            .unwrap();

            assert_eq!(
                m.map(AccessType::Load, ebpf::MM_PROGRAM_START, 1).unwrap(),
                original.as_ptr() as u64
            );

            assert_eq!(m.load::<u8>(ebpf::MM_PROGRAM_START).unwrap(), 11);
            assert_eq!(m.load::<u8>(ebpf::MM_PROGRAM_START + 1).unwrap(), 22);
            assert!(copied.borrow().is_empty());

            m.store(33u8, ebpf::MM_PROGRAM_START).unwrap();
            assert_eq!(original[0], 11);
            assert_eq!(m.load::<u8>(ebpf::MM_PROGRAM_START).unwrap(), 33);
            assert_eq!(m.load::<u8>(ebpf::MM_PROGRAM_START + 1).unwrap(), 22);
        }
    }

    #[test]
    fn test_cow_region_id() {
        for aligned_memory_mapping in [true, false] {
            let config = Config {
                aligned_memory_mapping,
                ..Config::default()
            };
            let original1 = [11, 22];
            let original2 = [33, 44];
            let copied = Rc::new(RefCell::new(Vec::new()));

            let c = Rc::clone(&copied);
            let m = MemoryMapping::new_with_cow(
                vec![
                    MemoryRegion::new_cow(&original1, ebpf::MM_PROGRAM_START, 42),
                    MemoryRegion::new_cow(&original2, ebpf::MM_PROGRAM_START + 0x100000000, 24),
                ],
                Box::new(move |id| {
                    // check that the argument passed to MemoryRegion::new_cow is then passed to the
                    // callback
                    assert_eq!(id, 42);
                    c.borrow_mut().extend_from_slice(&original1);
                    Ok(c.borrow().as_slice().as_ptr() as u64)
                }),
                &config,
                &SBPFVersion::V2,
            )
            .unwrap();

            m.store(55u8, ebpf::MM_PROGRAM_START).unwrap();
            assert_eq!(original1[0], 11);
            assert_eq!(m.load::<u8>(ebpf::MM_PROGRAM_START).unwrap(), 55);
        }
    }

    #[test]
    #[should_panic(expected = "AccessViolation")]
    fn test_map_cow_error() {
        let config = Config::default();
        let original = [11, 22];

        let m = MemoryMapping::new_with_cow(
            vec![MemoryRegion::new_cow(&original, ebpf::MM_PROGRAM_START, 42)],
            Box::new(|_| Err(())),
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();

        m.map(AccessType::Store, ebpf::MM_PROGRAM_START, 1).unwrap();
    }

    #[test]
    #[should_panic(expected = "AccessViolation")]
    fn test_store_cow_error() {
        let config = Config::default();
        let original = [11, 22];

        let m = MemoryMapping::new_with_cow(
            vec![MemoryRegion::new_cow(&original, ebpf::MM_PROGRAM_START, 42)],
            Box::new(|_| Err(())),
            &config,
            &SBPFVersion::V2,
        )
        .unwrap();

        m.store(33u8, ebpf::MM_PROGRAM_START).unwrap();
    }
}
