use {
    super::*,
    crate::translate_mut,
    solana_program_runtime::invoke_context::SerializedAccountMetadata,
    solana_sbpf::{error::EbpfError, memory_region::MemoryRegion},
    std::slice,
};

fn mem_op_consume(invoke_context: &mut InvokeContext, n: u64) -> Result<(), Error> {
    let compute_cost = invoke_context.get_execution_cost();
    let cost = compute_cost.mem_op_base_cost.max(
        n.checked_div(compute_cost.cpi_bytes_per_unit)
            .unwrap_or(u64::MAX),
    );
    consume_compute_meter(invoke_context, cost)
}

/// Check that two regions do not overlap.
pub(crate) fn is_nonoverlapping<N>(src: N, src_len: N, dst: N, dst_len: N) -> bool
where
    N: Ord + num_traits::SaturatingSub,
{
    // If the absolute distance between the ptrs is at least as big as the size of the other,
    // they do not overlap.
    if src > dst {
        src.saturating_sub(&dst) >= dst_len
    } else {
        dst.saturating_sub(&src) >= src_len
    }
}

declare_builtin_function!(
    /// memcpy
    SyscallMemcpy,
    fn rust(
        invoke_context: &mut InvokeContext,
        dst_addr: u64,
        src_addr: u64,
        n: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        mem_op_consume(invoke_context, n)?;

        if !is_nonoverlapping(src_addr, n, dst_addr, n) {
            return Err(SyscallError::CopyOverlapping.into());
        }

        // host addresses can overlap so we always invoke memmove
        memmove(invoke_context, dst_addr, src_addr, n, memory_mapping)
    }
);

declare_builtin_function!(
    /// memmove
    SyscallMemmove,
    fn rust(
        invoke_context: &mut InvokeContext,
        dst_addr: u64,
        src_addr: u64,
        n: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        mem_op_consume(invoke_context, n)?;

        memmove(invoke_context, dst_addr, src_addr, n, memory_mapping)
    }
);

declare_builtin_function!(
    /// memcmp
    SyscallMemcmp,
    fn rust(
        invoke_context: &mut InvokeContext,
        s1_addr: u64,
        s2_addr: u64,
        n: u64,
        cmp_result_addr: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        mem_op_consume(invoke_context, n)?;

        let s1 = translate_slice::<u8>(
            memory_mapping,
            s1_addr,
            n,
            invoke_context.get_check_aligned(),
        )?;
        let s2 = translate_slice::<u8>(
            memory_mapping,
            s2_addr,
            n,
            invoke_context.get_check_aligned(),
        )?;

        debug_assert_eq!(s1.len(), n as usize);
        debug_assert_eq!(s2.len(), n as usize);
        // Safety:
        // memcmp is marked unsafe since it assumes that the inputs are at least
        // `n` bytes long. `s1` and `s2` are guaranteed to be exactly `n` bytes
        // long because `translate_slice` would have failed otherwise.
        let result = unsafe { memcmp(s1, s2, n as usize) };

        translate_mut!(
            memory_mapping,
            invoke_context.get_check_aligned(),
            let cmp_result_ref_mut: &mut i32 = map(cmp_result_addr)?;
        );
        *cmp_result_ref_mut = result;

        Ok(0)
    }
);

declare_builtin_function!(
    /// memset
    SyscallMemset,
    fn rust(
        invoke_context: &mut InvokeContext,
        dst_addr: u64,
        c: u64,
        n: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Error> {
        mem_op_consume(invoke_context, n)?;

        translate_mut!(
            memory_mapping,
            invoke_context.get_check_aligned(),
            let s: &mut [u8] = map(dst_addr, n)?;
        );
        s.fill(c as u8);
        Ok(0)
    }
);

fn memmove(
    invoke_context: &mut InvokeContext,
    dst_addr: u64,
    src_addr: u64,
    n: u64,
    memory_mapping: &MemoryMapping,
) -> Result<u64, Error> {
    translate_mut!(
        memory_mapping,
        invoke_context.get_check_aligned(),
        let dst_ref_mut: &mut [u8] = map(dst_addr, n)?;
    );
    let dst_ptr = dst_ref_mut.as_mut_ptr();
    let src_ptr = translate_slice::<u8>(
        memory_mapping,
        src_addr,
        n,
        invoke_context.get_check_aligned(),
    )?
    .as_ptr();

    unsafe { std::ptr::copy(src_ptr, dst_ptr, n as usize) };
    Ok(0)
}

// Marked unsafe since it assumes that the slices are at least `n` bytes long.
unsafe fn memcmp(s1: &[u8], s2: &[u8], n: usize) -> i32 {
    for i in 0..n {
        let a = *s1.get_unchecked(i);
        let b = *s2.get_unchecked(i);
        if a != b {
            return (a as i32).saturating_sub(b as i32);
        };
    }

    0
}

#[derive(Debug)]
enum MemcmpError {
    Diff(i32),
}

impl std::fmt::Display for MemcmpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemcmpError::Diff(diff) => write!(f, "memcmp diff: {diff}"),
        }
    }
}

impl std::error::Error for MemcmpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MemcmpError::Diff(_) => None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn iter_memory_pair_chunks<T, F>(
    src_access: AccessType,
    src_addr: u64,
    dst_access: AccessType,
    dst_addr: u64,
    n_bytes: u64,
    accounts: &[SerializedAccountMetadata],
    memory_mapping: &mut MemoryMapping,
    reverse: bool,
    resize_area: bool,
    mut fun: F,
) -> Result<T, Error>
where
    T: Default,
    F: FnMut(*const u8, *const u8, usize) -> Result<T, Error>,
{
    let mut src_chunk_iter = MemoryChunkIterator::new(
        memory_mapping,
        accounts,
        src_access,
        src_addr,
        n_bytes,
        resize_area,
    )?;
    let mut dst_chunk_iter = MemoryChunkIterator::new(
        memory_mapping,
        accounts,
        dst_access,
        dst_addr,
        n_bytes,
        resize_area,
    )?;

    let mut src_chunk = None;
    let mut dst_chunk = None;

    macro_rules! memory_chunk {
        ($chunk_iter:ident, $chunk:ident) => {
            if let Some($chunk) = &mut $chunk {
                // Keep processing the current chunk
                $chunk
            } else {
                // This is either the first call or we've processed all the bytes in the current
                // chunk. Move to the next one.
                let chunk = match if reverse {
                    $chunk_iter.next_back()
                } else {
                    $chunk_iter.next()
                } {
                    Some(item) => item?,
                    None => break,
                };
                $chunk.insert(chunk)
            }
        };
    }

    loop {
        let (src_region, src_chunk_addr, src_remaining) = memory_chunk!(src_chunk_iter, src_chunk);
        let (dst_region, dst_chunk_addr, dst_remaining) = memory_chunk!(dst_chunk_iter, dst_chunk);

        // We always process same-length pairs
        let chunk_len = *src_remaining.min(dst_remaining);

        let (src_host_addr, dst_host_addr) = {
            let (src_addr, dst_addr) = if reverse {
                // When scanning backwards not only we want to scan regions from the end,
                // we want to process the memory within regions backwards as well.
                (
                    src_chunk_addr
                        .saturating_add(*src_remaining as u64)
                        .saturating_sub(chunk_len as u64),
                    dst_chunk_addr
                        .saturating_add(*dst_remaining as u64)
                        .saturating_sub(chunk_len as u64),
                )
            } else {
                (*src_chunk_addr, *dst_chunk_addr)
            };

            (
                src_region
                    .vm_to_host(src_access, src_addr, chunk_len as u64)
                    .ok_or_else(|| {
                        EbpfError::AccessViolation(src_access, src_addr, chunk_len as u64, "")
                    })?,
                dst_region
                    .vm_to_host(dst_access, dst_addr, chunk_len as u64)
                    .ok_or_else(|| {
                        EbpfError::AccessViolation(dst_access, dst_addr, chunk_len as u64, "")
                    })?,
            )
        };

        fun(
            src_host_addr as *const u8,
            dst_host_addr as *const u8,
            chunk_len,
        )?;

        // Update how many bytes we have left to scan in each chunk
        *src_remaining = src_remaining.saturating_sub(chunk_len);
        *dst_remaining = dst_remaining.saturating_sub(chunk_len);

        if !reverse {
            // We've scanned `chunk_len` bytes so we move the vm address forward. In reverse
            // mode we don't do this since we make progress by decreasing src_len and
            // dst_len.
            *src_chunk_addr = src_chunk_addr.saturating_add(chunk_len as u64);
            *dst_chunk_addr = dst_chunk_addr.saturating_add(chunk_len as u64);
        }

        if *src_remaining == 0 {
            src_chunk = None;
        }

        if *dst_remaining == 0 {
            dst_chunk = None;
        }
    }

    Ok(T::default())
}

struct MemoryChunkIterator<'a> {
    memory_mapping: &'a mut MemoryMapping<'a>,
    accounts: &'a [SerializedAccountMetadata],
    access_type: AccessType,
    initial_vm_addr: u64,
    vm_addr_start: u64,
    // exclusive end index (start + len, so one past the last valid address)
    vm_addr_end: u64,
    len: u64,
    account_index: Option<usize>,
    is_account: Option<bool>,
    resize_area: bool,
}

impl<'a> MemoryChunkIterator<'a> {
    fn new(
        memory_mapping: &'a mut MemoryMapping,
        accounts: &'a [SerializedAccountMetadata],
        access_type: AccessType,
        vm_addr: u64,
        len: u64,
        resize_area: bool,
    ) -> Result<MemoryChunkIterator<'a>, EbpfError> {
        let vm_addr_end = vm_addr.checked_add(len).ok_or(EbpfError::AccessViolation(
            access_type,
            vm_addr,
            len,
            "unknown",
        ))?;

        Ok(MemoryChunkIterator {
            memory_mapping,
            accounts,
            access_type,
            initial_vm_addr: vm_addr,
            len,
            vm_addr_start: vm_addr,
            vm_addr_end,
            account_index: None,
            is_account: None,
            resize_area,
        })
    }

    fn region(&mut self, vm_addr: u64) -> Result<&'a MemoryRegion, Error> {
        match self.memory_mapping.map(self.access_type, vm_addr, self.len) {
            solana_sbpf::error::ProgramResult::Ok(_) => {
                Ok(self.memory_mapping.find_region(vm_addr).unwrap().1)
            }
            solana_sbpf::error::ProgramResult::Err(error) => match error {
                EbpfError::AccessViolation(access_type, _vm_addr, _len, name) => Err(Box::new(
                    EbpfError::AccessViolation(access_type, self.initial_vm_addr, self.len, name),
                )),
                EbpfError::StackAccessViolation(access_type, _vm_addr, _len, frame) => {
                    Err(Box::new(EbpfError::StackAccessViolation(
                        access_type,
                        self.initial_vm_addr,
                        self.len,
                        frame,
                    )))
                }
                _ => Err(error.into()),
            },
        }
    }
}

impl<'a> Iterator for MemoryChunkIterator<'a> {
    type Item = Result<(&'a MemoryRegion, u64, usize), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.vm_addr_start == self.vm_addr_end {
            return None;
        }

        let region = match self.region(self.vm_addr_start) {
            Ok(region) => region,
            Err(e) => {
                self.vm_addr_start = self.vm_addr_end;
                return Some(Err(e));
            }
        };

        let region_is_account;

        let mut account_index = self.account_index.unwrap_or_default();
        self.account_index = Some(account_index);

        loop {
            if let Some(account) = self.accounts.get(account_index) {
                let account_addr = account.vm_data_addr;
                let resize_addr = account_addr.saturating_add(account.original_data_len as u64);

                if resize_addr < region.vm_addr {
                    // region is after this account, move on next one
                    account_index = account_index.saturating_add(1);
                    self.account_index = Some(account_index);
                } else {
                    region_is_account = (account.original_data_len != 0 && region.vm_addr == account_addr)
                        // unaligned programs do not have a resize area
                        || (self.resize_area && region.vm_addr == resize_addr);
                    break;
                }
            } else {
                // address is after all the accounts
                region_is_account = false;
                break;
            }
        }

        if let Some(is_account) = self.is_account {
            if is_account != region_is_account {
                return Some(Err(SyscallError::InvalidLength.into()));
            }
        } else {
            self.is_account = Some(region_is_account);
        }

        let vm_addr = self.vm_addr_start;
        let region_vm_addr_end = region.vm_addr_range().end;

        let chunk_len = if region_vm_addr_end <= self.vm_addr_end {
            // consume the whole region
            let len = region_vm_addr_end.saturating_sub(self.vm_addr_start);
            self.vm_addr_start = region_vm_addr_end;
            len
        } else {
            // consume part of the region
            let len = self.vm_addr_end.saturating_sub(self.vm_addr_start);
            self.vm_addr_start = self.vm_addr_end;
            len
        };

        Some(Ok((region, vm_addr, chunk_len as usize)))
    }
}

impl DoubleEndedIterator for MemoryChunkIterator<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.vm_addr_start == self.vm_addr_end {
            return None;
        }

        let region = match self.region(self.vm_addr_end.saturating_sub(1)) {
            Ok(region) => region,
            Err(e) => {
                self.vm_addr_start = self.vm_addr_end;
                return Some(Err(e));
            }
        };

        let region_is_account;

        let mut account_index = self
            .account_index
            .unwrap_or_else(|| self.accounts.len().saturating_sub(1));
        self.account_index = Some(account_index);

        loop {
            let Some(account) = self.accounts.get(account_index) else {
                // address is after all the accounts
                region_is_account = false;
                break;
            };

            let account_addr = account.vm_data_addr;
            let resize_addr = account_addr.saturating_add(account.original_data_len as u64);

            if account_index > 0 && account_addr > region.vm_addr {
                account_index = account_index.saturating_sub(1);

                self.account_index = Some(account_index);
            } else {
                region_is_account = (account.original_data_len != 0 && region.vm_addr == account_addr)
                    // unaligned programs do not have a resize area
                    || (self.resize_area && region.vm_addr == resize_addr);
                break;
            }
        }

        if let Some(is_account) = self.is_account {
            if is_account != region_is_account {
                return Some(Err(SyscallError::InvalidLength.into()));
            }
        } else {
            self.is_account = Some(region_is_account);
        }

        let chunk_len = if region.vm_addr >= self.vm_addr_start {
            // consume the whole region
            let len = self.vm_addr_end.saturating_sub(region.vm_addr);
            self.vm_addr_end = region.vm_addr;
            len
        } else {
            // consume part of the region
            let len = self.vm_addr_end.saturating_sub(self.vm_addr_start);
            self.vm_addr_end = self.vm_addr_start;
            len
        };

        Some(Ok((region, self.vm_addr_end, chunk_len as usize)))
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use {
        super::*,
        assert_matches::assert_matches,
        solana_sbpf::{ebpf::MM_RODATA_START, program::SBPFVersion},
        test_case::test_case,
    };

    fn to_chunk_vec<'a>(
        iter: impl Iterator<Item = Result<(&'a MemoryRegion, u64, usize), Error>>,
    ) -> Vec<(u64, usize)> {
        iter.flat_map(|res| res.map(|(_, vm_addr, len)| (vm_addr, len)))
            .collect::<Vec<_>>()
    }

    fn build_memory_mapping<'a>(
        regions: &[usize],
        config: &'a Config,
    ) -> (Vec<Vec<u8>>, MemoryMapping<'a>) {
        let mut regs = vec![];
        let mut mem = Vec::new();
        let mut offset = 0;
        for (i, region_len) in regions.iter().enumerate() {
            mem.push(
                (0..*region_len)
                    .map(|x| (i * 10 + x) as u8)
                    .collect::<Vec<_>>(),
            );
            regs.push(MemoryRegion::new_writable(
                &mut mem[i],
                MM_RODATA_START + offset as u64,
            ));
            offset += *region_len;
        }

        let mut memory_mapping = MemoryMapping::new(regs, config, SBPFVersion::V3).unwrap();

        (mem, memory_mapping)
    }

    fn flatten_memory(mem: &[Vec<u8>]) -> Vec<u8> {
        mem.iter().flatten().copied().collect()
    }

    #[test]
    fn test_is_nonoverlapping() {
        for dst in 0..8 {
            assert!(is_nonoverlapping(10, 3, dst, 3));
        }
        for dst in 8..13 {
            assert!(!is_nonoverlapping(10, 3, dst, 3));
        }
        for dst in 13..20 {
            assert!(is_nonoverlapping(10, 3, dst, 3));
        }
        assert!(is_nonoverlapping::<u8>(255, 3, 254, 1));
        assert!(!is_nonoverlapping::<u8>(255, 2, 254, 3));
    }
}
