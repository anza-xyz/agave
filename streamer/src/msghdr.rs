#![cfg(target_os = "linux")]

use {
    libc::{iovec, msghdr, sockaddr_in, sockaddr_in6, socklen_t},
    std::{
        mem::{MaybeUninit, zeroed},
        ptr,
    },
};

/// An IPv4 or IPv6 socket address, the only kinds UDP sockets send to and receive
/// from: 28 bytes, against 128 for `sockaddr_storage`, which fits every address family.
#[derive(Clone, Copy)]
#[repr(C)]
pub(crate) union SockAddrInet {
    pub(crate) v4: sockaddr_in,
    pub(crate) v6: sockaddr_in6,
}

pub(crate) fn create_msghdr(
    msg_name: &mut MaybeUninit<SockAddrInet>,
    msg_namelen: socklen_t,
    iov: &mut MaybeUninit<iovec>,
) -> msghdr {
    // Cannot construct msghdr directly on musl
    // See https://github.com/rust-lang/libc/issues/2344 for more info
    let mut msg_hdr: msghdr = unsafe { zeroed() };
    msg_hdr.msg_name = msg_name.as_mut_ptr() as *mut _;
    msg_hdr.msg_namelen = msg_namelen;
    msg_hdr.msg_iov = iov.as_mut_ptr();
    msg_hdr.msg_iovlen = 1;
    msg_hdr.msg_control = ptr::null::<libc::c_void>() as *mut _;
    msg_hdr.msg_controllen = 0;
    msg_hdr.msg_flags = 0;
    msg_hdr
}
