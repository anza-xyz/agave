#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![cfg(target_os = "linux")]
mod ring;
mod slab;
use {
    io_uring::IoUring,
    std::{io, sync::Once},
};
pub use {ring::*, slab::FixedSlab};

pub fn io_uring_supported() -> bool {
    static mut IO_URING_SUPPORTED: bool = false;
    static IO_URING_SUPPORTED_ONCE: Once = Once::new();
    IO_URING_SUPPORTED_ONCE.call_once(|| {
        fn check() -> io::Result<()> {
            let ring = IoUring::new(1)?;
            if !ring.params().is_feature_nodrop() {
                return Err(io::Error::other("no IORING_FEAT_NODROP"));
            }
            if !ring.params().is_feature_sqpoll_nonfixed() {
                return Err(io::Error::other("no IORING_FEAT_SQPOLL_NONFIXED"));
            }
            Ok(())
        }
        unsafe {
            IO_URING_SUPPORTED = match check() {
                Ok(_) => {
                    log::info!("io_uring supported");
                    true
                }
                Err(e) => {
                    log::info!("io_uring NOT supported: {e}");
                    false
                }
            };
        }
    });
    unsafe { IO_URING_SUPPORTED }
}
