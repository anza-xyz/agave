#![cfg(feature = "agave-unstable-api")]
#![no_std]

#[repr(C, align(4))]
pub struct Aligned<Bytes: ?Sized> {
    pub _align: u8,
    pub bytes: Bytes,
}

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
macro_rules! program {
    () => {
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/agave-xdp-prog"))
    };
}

#[cfg(all(target_os = "linux", not(target_arch = "bpf")))]
#[unsafe(no_mangle)]
pub static AGAVE_XDP_EBPF_PROGRAM: &Aligned<[u8; program!().len()]> = &Aligned {
    _align: 0,
    bytes: *program!(),
};
