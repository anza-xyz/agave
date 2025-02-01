#![feature(test)]
extern crate test;

use {
    libc::{msghdr, socklen_t},
    std::{hint::black_box, ptr},
    test::Bencher
};

#[bench]
fn unsafe_constructor(b: &mut Bencher) {
    b.iter(|| {
        for i in 0..100_000 {
            black_box(|| {
                let mut msg_hdr: msghdr = unsafe { std::mem::zeroed() };
                msg_hdr.msg_name = ptr::null::<u8>() as *mut _;
                msg_hdr.msg_namelen = i as socklen_t;
                msg_hdr.msg_iov = ptr::null::<libc::iovec>() as *mut _;
                msg_hdr.msg_iovlen = 1;
                msg_hdr.msg_control = ptr::null::<libc::c_void>() as *mut _;
                msg_hdr.msg_controllen = 0;
                msg_hdr.msg_flags = 0;
                msg_hdr
            });
        }
    });
}

#[bench]
fn safe_constructor(b: &mut Bencher) {
    b.iter(|| {
        for i in 0..100_000 {
            black_box(
                msghdr {
                    msg_name: ptr::null::<u8>() as *mut _,
                    msg_namelen: i as socklen_t,
                    msg_iov: ptr::null::<libc::iovec>() as *mut _,
                    msg_iovlen: 1,
                    msg_control: ptr::null::<libc::c_void>() as *mut _,
                    msg_controllen: 0,
                    msg_flags: 0,
                }
            );
        }
    });
}
