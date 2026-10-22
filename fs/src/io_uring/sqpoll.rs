use std::{
    io,
    os::fd::{AsFd, AsRawFd as _, BorrowedFd},
    time::Duration,
};

const SQPOLL_IDLE_WAIT_TIME: Duration = Duration::from_millis(50);

/// Mechanism for sharing a kernel worker pool and submission queue kernel polling thread
///
/// A single `SharedSqPoll` instance should be created and file descriptor obtained through
/// `as_fd()` can be then passed to io-uring utilities that should create io_uring builder using
/// `io_uring_builder_with(shared_sqpoll_fd)`.
pub struct SharedSqPoll {
    /// The io_uring instance used as root defining the kernel worker pool such that
    /// other io_uring instances can attach to its file descriptor.
    sqpoll_io_uring: io_uring::IoUring,
}

impl SharedSqPoll {
    pub fn new() -> io::Result<Self> {
        let shared_sqpoll_io_uring = io_uring::IoUring::builder()
            .setup_sqpoll(SQPOLL_IDLE_WAIT_TIME.as_millis() as u32)
            .build(1)?;
        Ok(Self {
            sqpoll_io_uring: shared_sqpoll_io_uring,
        })
    }
}

impl AsFd for SharedSqPoll {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.sqpoll_io_uring.as_fd()
    }
}

/// Return new io-uring builder that is attached to shared worker pool (if provided).
pub fn io_uring_builder_with(shared_sqpoll_fd: Option<BorrowedFd>) -> io_uring::Builder {
    let mut builder = io_uring::IoUring::builder();
    if let Some(fd) = shared_sqpoll_fd {
        builder
            .setup_attach_wq(fd.as_raw_fd())
            .setup_sqpoll(SQPOLL_IDLE_WAIT_TIME.as_millis() as u32);
    }
    builder
}
