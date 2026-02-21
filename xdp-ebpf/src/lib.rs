#![cfg(feature = "agave-unstable-api")]
// Activate some of the Rust 2024 lints to make the future migration easier.
#![warn(if_let_rescope)]
#![warn(keyword_idents_2024)]
#![warn(rust_2024_incompatible_pat)]
#![warn(tail_expr_drop_order)]
#![warn(unsafe_attr_outside_unsafe)]
#![warn(unsafe_op_in_unsafe_fn)]
#![no_std]

#[repr(C, align(32))]
pub struct AlignedBytes<const N: usize>(pub [u8; N]);

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
#[unsafe(no_mangle)]
pub static AGAVE_XDP_EBPF_PROGRAM: AlignedBytes<
    { include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/agave-xdp-prog")).len() },
> = AlignedBytes(*include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/agave-xdp-prog"
)));
