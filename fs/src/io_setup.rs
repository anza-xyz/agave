use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

#[cfg(target_os = "linux")]
const SQPOLL_IDLE_WAIT_TIME: u32 = 50;

// This value reflects recommended memory lock limit documented in the validator's
// setup instructions at docs/src/operations/guides/validator-start.md allowing use of
// several io_uring instances with fixed buffers for large disk IO operations.
pub const DEFAULT_MEMLOCK_BUDGET_SIZE: usize = 2_000_000_000;
// Linux distributions often have some small memory lock limit (e.g. 8MB) that we can tap into.
pub const MEMLOCK_BUDGET_SIZE_FOR_TESTS: usize = 4_000_000;

/// State used by IO utilities for managing shared resources and configuration during setup.
///
/// This may include io_uring file descriptors, lockable memory budget to register in kernel,
/// and other resources that need to be accessed by multiple functions performing IO operations
/// such that they can efficiently cooperate with each other.
///
/// This is achieved by creating `IoSetupState` at the beginning of the processing setup and
/// passing it or its clones to IO utilities that need sharing.
///
/// Note: the state needs to live only during creation of the IO utilities, not during their usage,
/// so it's generally advisable to only create clones as needed and not hold the original instance
/// after setup is done.
#[derive(Clone)]
pub struct IoSetupState {
    inner: Arc<IoSetupStateInner>,
}

impl IoSetupState {
    /// Creates a new `IoSetupState` with the specified memlock budget.
    ///
    /// The memlock budget is used to limit the amount of memory that can be locked by the kernel
    /// through io_uring utilities, such that buffer sizes can be adjusted for available `ulimit`
    /// with fallback to sync IO if necessary.
    pub fn new_with_memlock_budget(memlock_budget: usize) -> Self {
        Self {
            inner: Arc::new(IoSetupStateInner::new_with_memlock_budget(memlock_budget)),
        }
    }

    /// Enables shared io-uring worker pool and sqpoll based kernel thread.
    ///
    /// The sqpoll thread will drain submission queues from all io-uring instances created
    /// through builder obtained from `create_io_uring_builder()` after this call.
    pub fn with_shared_sqpoll(self) -> io::Result<Self> {
        let inner = Arc::into_inner(self.inner)
            .expect("shared_sqpoll must be enabled before cloning IoSetupState");
        Ok(Self {
            inner: Arc::new(inner.with_shared_sqpoll()?),
        })
    }

    /// Attempt to acquire `desired_size` (but at least `min_size`) bytes from the memlock budget.
    ///
    /// If available available budget is below `min_size` then `Err` is returned, otherwise
    /// `Ok(acquired_size)` is returned where `min_size <= acquired_size <= desired_size`.
    pub fn acquire_memlock_budget(
        &self,
        min_size: usize,
        desired_size: usize,
    ) -> io::Result<usize> {
        let calc_budget_after_acquire = |available_budget: usize| {
            if available_budget < min_size {
                None
            } else {
                Some(available_budget.saturating_sub(desired_size.max(min_size)))
            }
        };
        match self.inner.memlock_budget.fetch_update(
            Ordering::Release,
            Ordering::Acquire,
            calc_budget_after_acquire,
        ) {
            Ok(commited_budget) => {
                Ok(commited_budget
                    .saturating_sub(calc_budget_after_acquire(commited_budget).unwrap()))
            }
            Err(_) => Err(io::Error::new(
                io::ErrorKind::QuotaExceeded,
                "failed to acquire memlock budget",
            )),
        }
    }

    /// Return new io-uring builder that is attached shared worker pool (if configured).
    #[cfg(target_os = "linux")]
    pub fn create_io_uring_builder(&self) -> io_uring::Builder {
        use std::os::fd::AsRawFd;

        let mut builder = io_uring::IoUring::builder();
        if let Some(io_uring_backend) = &self.inner.shared_sqpoll_io_uring {
            builder
                .setup_attach_wq(io_uring_backend.as_raw_fd())
                .setup_sqpoll(SQPOLL_IDLE_WAIT_TIME);
        }
        builder
    }
}

impl Default for IoSetupState {
    fn default() -> Self {
        let inner = Arc::new(IoSetupStateInner::default());
        Self { inner }
    }
}

#[derive(Default)]
struct IoSetupStateInner {
    memlock_budget: AtomicUsize,

    #[cfg(target_os = "linux")]
    shared_sqpoll_io_uring: Option<io_uring::IoUring>,
}

impl IoSetupStateInner {
    fn new_with_memlock_budget(memlock_budget: usize) -> Self {
        Self {
            memlock_budget: AtomicUsize::new(memlock_budget),
            ..Default::default()
        }
    }

    fn with_shared_sqpoll(self) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let sqpoll_io_uring = io_uring::IoUring::builder()
                .setup_sqpoll(SQPOLL_IDLE_WAIT_TIME)
                .build(1)?;
            Ok(Self {
                shared_sqpoll_io_uring: Some(sqpoll_io_uring),
                ..self
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(self)
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use {
        super::*,
        crate::{
            file_io::FileCreator,
            io_uring::{
                file_creator::IoUringFileCreator, sequential_file_reader::SequentialFileReader,
            },
        },
        rand::RngCore,
        std::{
            fs::File,
            io::{Cursor, Read},
            sync::{Arc, RwLock},
        },
    };

    #[test]
    fn test_shared_sqpoll_read_and_create() {
        let io_setup = IoSetupState::default().with_shared_sqpoll().unwrap();

        let read_bytes = RwLock::new(vec![]);
        let read_bytes_ref = &read_bytes;
        let mut file_creator =
            IoUringFileCreator::with_buffer_capacity(1 << 20, io_setup.clone(), move |path| {
                let mut reader =
                    SequentialFileReader::with_capacity(1 << 20, path, io_setup.clone()).unwrap();
                reader
                    .read_to_end(read_bytes_ref.write().unwrap().as_mut())
                    .unwrap();
            })
            .unwrap();

        let temp_dir = tempfile::tempdir().unwrap();
        let dir_handle = Arc::new(File::open(temp_dir.path()).unwrap());
        let mut write_bytes = vec![0; 2 << 20];
        rand::thread_rng().fill_bytes(&mut write_bytes);

        let file_path1 = temp_dir.path().join("test-1.txt");
        let file_path2 = temp_dir.path().join("test-2.txt");
        for path in [file_path1, file_path2] {
            let dir_handle = dir_handle.clone();
            file_creator
                .schedule_create_at_dir(path, 0o644, dir_handle, &mut Cursor::new(&write_bytes))
                .unwrap();
        }
        file_creator.drain().unwrap();
        drop(file_creator);

        // After drain all the callbacks that read data into `read_bytes` should be done.
        let read_bytes = read_bytes.into_inner().unwrap();
        // Expect atomically appended two copies, each from different file.
        assert_eq!(&read_bytes[..write_bytes.len()], &write_bytes);
        assert_eq!(&read_bytes[write_bytes.len()..], &write_bytes);
    }
}
