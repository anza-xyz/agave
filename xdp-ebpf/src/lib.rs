#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![no_std]

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
#[unsafe(no_mangle)]
pub static AGAVE_XDP_EBPF_PROGRAM: [u8; aya::include_bytes_aligned!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/agave-xdp-prog"
))
.len()] = unsafe {
    core::ptr::read(
        aya::include_bytes_aligned!(concat!(env!("CARGO_MANIFEST_DIR"), "/agave-xdp-prog"))
            .as_ptr()
            .cast(),
    )
};
