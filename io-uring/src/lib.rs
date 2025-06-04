#![cfg(target_os = "linux")]
mod ring;
mod slab;
pub use ring::*;
use {
    io_uring::IoUring,
    std::{io, sync::Once},
};

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
                    log::info!("io_uring NOT supported: {}", e);
                    false
                }
            };
        }
    });
    unsafe { IO_URING_SUPPORTED }
}
