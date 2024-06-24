// Copyright 2022 Solana Maintainers <maintainers@solana.com>
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

#![cfg_attr(target_os = "windows", allow(dead_code))]

use crate::error::EbpfError;

#[cfg(not(target_os = "windows"))]
extern crate libc;
#[cfg(not(target_os = "windows"))]
use libc::c_void;

#[cfg(target_os = "windows")]
use winapi::{
    ctypes::c_void,
    shared::minwindef,
    um::{
        errhandlingapi::GetLastError,
        memoryapi::{VirtualAlloc, VirtualFree, VirtualProtect},
        sysinfoapi::{GetSystemInfo, SYSTEM_INFO},
        winnt,
    },
};

#[cfg(not(target_os = "windows"))]
macro_rules! libc_error_guard {
    (succeeded?, mmap, $addr:expr, $($arg:expr),*) => {{
        *$addr = libc::mmap(*$addr, $($arg),*);
        *$addr != libc::MAP_FAILED
    }};
    (succeeded?, $function:ident, $($arg:expr),*) => {
        libc::$function($($arg),*) == 0
    };
    ($function:ident, $($arg:expr),* $(,)?) => {{
        const RETRY_COUNT: usize = 3;
        for i in 0..RETRY_COUNT {
            if libc_error_guard!(succeeded?, $function, $($arg),*) {
                break;
            } else if i.saturating_add(1) == RETRY_COUNT {
                let args = vec![$(format!("{:?}", $arg)),*];
                #[cfg(any(target_os = "freebsd", target_os = "ios", target_os = "macos"))]
                let errno = *libc::__error();
                #[cfg(any(target_os = "android", target_os = "netbsd", target_os = "openbsd"))]
                let errno = *libc::__errno();
                #[cfg(target_os = "linux")]
                let errno = *libc::__errno_location();
                return Err(EbpfError::LibcInvocationFailed(stringify!($function), args, errno));
            }
        }
    }};
}

#[cfg(target_os = "windows")]
macro_rules! winapi_error_guard {
    (succeeded?, VirtualAlloc, $addr:expr, $($arg:expr),*) => {{
        *$addr = VirtualAlloc(*$addr, $($arg),*);
        !(*$addr).is_null()
    }};
    (succeeded?, $function:ident, $($arg:expr),*) => {
        $function($($arg),*) != 0
    };
    ($function:ident, $($arg:expr),* $(,)?) => {{
        if !winapi_error_guard!(succeeded?, $function, $($arg),*) {
            let args = vec![$(format!("{:?}", $arg)),*];
            let errno = GetLastError();
            return Err(EbpfError::LibcInvocationFailed(stringify!($function), args, errno as i32));
        }
    }};
}

pub fn get_system_page_size() -> usize {
    #[cfg(not(target_os = "windows"))]
    unsafe {
        libc::sysconf(libc::_SC_PAGESIZE) as usize
    }
    #[cfg(target_os = "windows")]
    unsafe {
        let mut system_info: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut system_info);
        system_info.dwPageSize as usize
    }
}

pub fn round_to_page_size(value: usize, page_size: usize) -> usize {
    value
        .saturating_add(page_size)
        .saturating_sub(1)
        .checked_div(page_size)
        .unwrap()
        .saturating_mul(page_size)
}

pub unsafe fn allocate_pages(size_in_bytes: usize) -> Result<*mut u8, EbpfError> {
    let mut raw: *mut c_void = std::ptr::null_mut();
    #[cfg(not(target_os = "windows"))]
    libc_error_guard!(
        mmap,
        &mut raw,
        size_in_bytes,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
        -1,
        0,
    );
    #[cfg(target_os = "windows")]
    winapi_error_guard!(
        VirtualAlloc,
        &mut raw,
        size_in_bytes,
        winnt::MEM_RESERVE | winnt::MEM_COMMIT,
        winnt::PAGE_READWRITE,
    );
    Ok(raw.cast::<u8>())
}

pub unsafe fn free_pages(raw: *mut u8, size_in_bytes: usize) -> Result<(), EbpfError> {
    #[cfg(not(target_os = "windows"))]
    libc_error_guard!(munmap, raw.cast::<c_void>(), size_in_bytes);
    #[cfg(target_os = "windows")]
    winapi_error_guard!(
        VirtualFree,
        raw.cast::<c_void>(),
        size_in_bytes,
        winnt::MEM_RELEASE, // winnt::MEM_DECOMMIT
    );
    Ok(())
}

pub unsafe fn protect_pages(
    raw: *mut u8,
    size_in_bytes: usize,
    executable_flag: bool,
) -> Result<(), EbpfError> {
    #[cfg(not(target_os = "windows"))]
    {
        libc_error_guard!(
            mprotect,
            raw.cast::<c_void>(),
            size_in_bytes,
            if executable_flag {
                libc::PROT_EXEC | libc::PROT_READ
            } else {
                libc::PROT_READ
            },
        );
    }
    #[cfg(target_os = "windows")]
    {
        let mut old: minwindef::DWORD = 0;
        let ptr_old: *mut minwindef::DWORD = &mut old;
        winapi_error_guard!(
            VirtualProtect,
            raw.cast::<c_void>(),
            size_in_bytes,
            if executable_flag {
                winnt::PAGE_EXECUTE_READ
            } else {
                winnt::PAGE_READONLY
            },
            ptr_old,
        );
    }
    Ok(())
}
