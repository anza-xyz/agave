#![cfg(feature = "agave-unstable-api")]
#![no_std]

#[repr(C, align(4))]
pub struct Aligned<Bytes: ?Sized>(Bytes);

impl<Bytes: ?Sized> core::ops::Deref for Aligned<Bytes> {
    type Target = Bytes;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
#[unsafe(no_mangle)]
pub static AGAVE_XDP_EBPF_PROGRAM: &Aligned<[u8]> = &Aligned(*include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/agave-xdp-prog"
)));
