#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::too_many_arguments)]
// Copyright 2015 Big Switch Networks, Inc
//      (Algorithms for uBPF syscalls, originally in C)
// Copyright 2016 6WIND S.A. <quentin.monnet@6wind.com>
//      (Translation to Rust, other syscalls)
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! This module implements some built-in syscalls that can be called from within an eBPF program.
//!
//! These syscalls may originate from several places:
//!
//! * Some of them mimic the syscalls available in the Linux kernel.
//! * Some of them were proposed as example syscalls in uBPF and they were adapted here.
//! * Other syscalls may be specific to rbpf.
//!
//! The prototype for syscalls is always the same: five `u64` as arguments, and a `u64` as a return
//! value. Hence some syscalls have unused arguments, or return a 0 value in all cases, in order to
//! respect this convention.

use crate::{
    declare_builtin_function,
    error::EbpfError,
    memory_region::{AccessType, MemoryMapping},
    vm::TestContextObject,
};
use std::{slice::from_raw_parts, str::from_utf8};

declare_builtin_function!(
    /// Prints its **last three** arguments to standard output. The **first two** arguments are
    /// **unused**. Returns the number of bytes written.
    SyscallTracePrintf,
    fn rust(
        _context_object: &mut TestContextObject,
        _arg1: u64,
        _arg2: u64,
        arg3: u64,
        arg4: u64,
        arg5: u64,
        _memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        println!("bpf_trace_printf: {arg3:#x}, {arg4:#x}, {arg5:#x}");
        let size_arg = |x| {
            if x == 0 {
                1
            } else {
                (x as f64).log(16.0).floor() as u64 + 1
            }
        };
        Ok("bpf_trace_printf: 0x, 0x, 0x\n".len() as u64
            + size_arg(arg3)
            + size_arg(arg4)
            + size_arg(arg5))
    }
);

declare_builtin_function!(
    /// The idea is to assemble five bytes into a single `u64`. For compatibility with the syscalls API,
    /// each argument must be a `u64`.
    SyscallGatherBytes,
    fn rust(
        _context_object: &mut TestContextObject,
        arg1: u64,
        arg2: u64,
        arg3: u64,
        arg4: u64,
        arg5: u64,
        _memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        Ok(arg1.wrapping_shl(32)
            | arg2.wrapping_shl(24)
            | arg3.wrapping_shl(16)
            | arg4.wrapping_shl(8)
            | arg5)
    }
);

declare_builtin_function!(
    /// Same as `void *memfrob(void *s, size_t n);` in `string.h` in C. See the GNU manual page (in
    /// section 3) for `memfrob`. The memory is directly modified, and the syscall returns 0 in all
    /// cases. Arguments 3 to 5 are unused.
    SyscallMemFrob,
    fn rust(
        _context_object: &mut TestContextObject,
        vm_addr: u64,
        len: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let host_addr: Result<u64, EbpfError> =
            memory_mapping.map(AccessType::Store, vm_addr, len).into();
        let host_addr = host_addr?;
        for i in 0..len {
            unsafe {
                let p = (host_addr + i) as *mut u8;
                *p ^= 0b101010;
            }
        }
        Ok(0)
    }
);

declare_builtin_function!(
    /// C-like `strcmp`, return 0 if the strings are equal, and a non-null value otherwise.
    SyscallStrCmp,
    fn rust(
        _context_object: &mut TestContextObject,
        arg1: u64,
        arg2: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        // C-like strcmp, maybe shorter than converting the bytes to string and comparing?
        if arg1 == 0 || arg2 == 0 {
            return Ok(u64::MAX);
        }
        let a: Result<u64, EbpfError> = memory_mapping.map(AccessType::Load, arg1, 1).into();
        let mut a = a?;
        let b: Result<u64, EbpfError> = memory_mapping.map(AccessType::Load, arg2, 1).into();
        let mut b = b?;
        unsafe {
            let mut a_val = *(a as *const u8);
            let mut b_val = *(b as *const u8);
            while a_val == b_val && a_val != 0 && b_val != 0 {
                a += 1;
                b += 1;
                a_val = *(a as *const u8);
                b_val = *(b as *const u8);
            }
            if a_val >= b_val {
                Ok((a_val - b_val) as u64)
            } else {
                Ok((b_val - a_val) as u64)
            }
        }
    }
);

declare_builtin_function!(
    /// Prints a NULL-terminated UTF-8 string.
    SyscallString,
    fn rust(
        _context_object: &mut TestContextObject,
        vm_addr: u64,
        len: u64,
        _arg3: u64,
        _arg4: u64,
        _arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        let host_addr: Result<u64, EbpfError> =
            memory_mapping.map(AccessType::Load, vm_addr, len).into();
        let host_addr = host_addr?;
        unsafe {
            let c_buf = from_raw_parts(host_addr as *const u8, len as usize);
            let len = c_buf.iter().position(|c| *c == 0).unwrap_or(len as usize);
            let message = from_utf8(&c_buf[0..len]).unwrap_or("Invalid UTF-8 String");
            println!("log: {message}");
        }
        Ok(0)
    }
);

declare_builtin_function!(
    /// Prints the five arguments formated as u64 in decimal.
    SyscallU64,
    fn rust(
        _context_object: &mut TestContextObject,
        arg1: u64,
        arg2: u64,
        arg3: u64,
        arg4: u64,
        arg5: u64,
        memory_mapping: &mut MemoryMapping,
    ) -> Result<u64, Box<dyn std::error::Error>> {
        println!(
            "dump_64: {:#x}, {:#x}, {:#x}, {:#x}, {:#x}, {:?}",
            arg1, arg2, arg3, arg4, arg5, memory_mapping as *const _
        );
        Ok(0)
    }
);
