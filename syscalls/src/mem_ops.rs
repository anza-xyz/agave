use {super::*, crate::translate_mut};

fn mem_op_consume(invoke_context: &mut InvokeContext, n: u64) -> Result<(), Error> {
    let compute_cost = invoke_context.get_execution_cost();
    let cost = compute_cost.mem_op_base_cost.max(
        n.checked_div(compute_cost.cpi_bytes_per_unit)
            .unwrap_or(u64::MAX),
    );
    invoke_context.compute_meter.consume_checked(cost)
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
        invoke_context: &mut InvokeContext<'_, '_>,
        dst_addr: u64,
        src_addr: u64,
        n: u64,
        _arg4: u64,
        _arg5: u64,
    ) -> Result<u64, Error> {
        mem_op_consume(invoke_context, n)?;

        if !is_nonoverlapping(src_addr, n, dst_addr, n) {
            return Err(SyscallError::CopyOverlapping.into());
        }

        // host addresses can overlap so we always invoke memmove
        memmove(invoke_context, dst_addr, src_addr, n)
    }
);

declare_builtin_function!(
    /// memmove
    SyscallMemmove,
    fn rust(
        invoke_context: &mut InvokeContext<'_, '_>,
        dst_addr: u64,
        src_addr: u64,
        n: u64,
        _arg4: u64,
        _arg5: u64,
    ) -> Result<u64, Error> {
        mem_op_consume(invoke_context, n)?;
        memmove(invoke_context, dst_addr, src_addr, n)
    }
);

declare_builtin_function!(
    /// memcmp
    SyscallMemcmp,
    fn rust(
        invoke_context: &mut InvokeContext<'_, '_>,
        s1_addr: u64,
        s2_addr: u64,
        n: u64,
        cmp_result_addr: u64,
        _arg5: u64,
    ) -> Result<u64, Error> {
        mem_op_consume(invoke_context, n)?;
        let check_aligned = invoke_context.get_check_aligned();
        let memory_mapping = invoke_context.memory_contexts.memory_mapping_mut()?;

        let s1 = translate_slice::<u8>(
            memory_mapping,
            s1_addr,
            n,
            check_aligned,
        )?;
        let s2 = translate_slice::<u8>(
            memory_mapping,
            s2_addr,
            n,
            check_aligned,
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
            check_aligned,
            let cmp_result_ref_mut: (&mut std::mem::MaybeUninit<i32>) = map(cmp_result_addr)?;
        );
        cmp_result_ref_mut.write(result);

        Ok(0)
    }
);

declare_builtin_function!(
    /// memset
    SyscallMemset,
    fn rust(
        invoke_context: &mut InvokeContext<'_, '_>,
        dst_addr: u64,
        c: u64,
        n: u64,
        _arg4: u64,
        _arg5: u64,
    ) -> Result<u64, Error> {
        mem_op_consume(invoke_context, n)?;

        let check_aligned = invoke_context.get_check_aligned();
        let memory_mapping = invoke_context.memory_contexts.memory_mapping_mut()?;
        translate_mut!(
            memory_mapping,
            check_aligned,
            let s: (&mut [MaybeUninit<u8>]) = map(dst_addr, n)?;
        );
        s.fill(MaybeUninit::new(c as u8));
        Ok(0)
    }
);

fn memmove(
    invoke_context: &mut InvokeContext,
    dst_addr: u64,
    src_addr: u64,
    n: u64,
) -> Result<u64, Error> {
    let check_aligned = invoke_context.get_check_aligned();
    let memory_mapping = invoke_context.memory_contexts.memory_mapping_mut()?;
    // In a rare exception to the rule we manually translate addresses to a raw pointer rather than
    // using `translate_mut!` because the src and dst memory regions may overlap for this syscall.
    touch_slice_mut::<MaybeUninit<u8>>(memory_mapping, dst_addr, n)?;
    let slice = translate_slice_inner!(
        memory_mapping,
        AccessType::Store,
        dst_addr,
        n,
        MaybeUninit<u8>,
        check_aligned,
    )?;
    let src_ptr = translate_slice::<u8>(memory_mapping, src_addr, n, check_aligned)?.as_ptr();
    unsafe { std::ptr::copy(src_ptr.cast(), slice as *mut MaybeUninit<u8>, n as usize) };
    Ok(0)
}

// Marked unsafe since it assumes that the slices are at least `n` bytes long.
unsafe fn memcmp(s1: &[u8], s2: &[u8], n: usize) -> i32 {
    let (s1pre, s1mid, s1end) = unsafe {
        // SAFETY: Caller is required to guarantee both slices are at least n-long.
        s1.get_unchecked(..n).align_to::<u128>()
    };
    let mut s2ptr = s2.as_ptr();
    for s1pre_byte in s1pre.iter().copied() {
        unsafe {
            // SAFETY: we are guaranteed to stay in bounds of a slice `s2` by virtue of both slices
            // containing at least `n` bytes (caller precondition.)
            let s2pre_byte = *s2ptr;
            if s1pre_byte != s2pre_byte {
                return i32::from(s1pre_byte).wrapping_sub(s2pre_byte.into());
            }
            s2ptr = s2ptr.add(1);
        }
    }
    for s1mid_value in s1mid.iter().copied() {
        let s2mid_value = unsafe {
            // SAFETY: Caller is required to guarantee both slices are at least n-long.
            // SAFETY: Pointer is guaranteed to be dereferenceable by virtue of being derived from
            // `s2` slice.
            s2ptr.cast::<u128>().read_unaligned().to_le()
        };
        if s1mid_value != s2mid_value {
            // It would seem that we could work with u128s directly here and leave it to LLVM to
            // figure out how to split up the operations to u64s, but it seems to produce notably
            // worse code than splitting the u64s out manually (even _when_ these splits result in
            // the values being re-read from "memory").
            let (s1_word, s2_word) = if s1mid_value as u64 != s2mid_value as u64 {
                let w1 = s1mid_value as u64;
                let w2 = s2mid_value as u64;
                (w1, w2)
            } else {
                let w1 = (s1mid_value >> 64) as u64;
                let w2 = (s2mid_value >> 64) as u64;
                (w1, w2)
            };
            let shift = (s1_word ^ s2_word).trailing_zeros() & !7;
            let b1 = (s1_word >> shift) as u8;
            let b2 = (s2_word >> shift) as u8;
            return i32::from(b1).wrapping_sub(b2.into());
        }
        unsafe {
            // SAFETY: we are guaranteed to stay in bounds of a slice `s2` by virtue of both slices
            // containing at least `n` bytes (caller precondition.)
            s2ptr = s2ptr.add(std::mem::size_of::<u128>());
        }
    }
    for s1end_byte in s1end.iter().copied() {
        unsafe {
            // This is the same as the `pre` slice loop above.
            let s2end_byte = *s2ptr;
            if s1end_byte != s2end_byte {
                return i32::from(s1end_byte).wrapping_sub(s2end_byte.into());
            }
            s2ptr = s2ptr.add(1);
        }
    }
    0
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
#[allow(clippy::arithmetic_side_effects)]
mod tests {
    use super::*;

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
