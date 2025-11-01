use std::io;

#[cfg(target_os = "linux")]
const SQPOLL_IDLE_WAIT_TIME: u32 = 50;

/// State used by IO utilities for managing shared resources and configuration during setup.
///
/// Holds objects that need to be accessed by multiple functions performing IO operations
/// such that they can efficiently cooperate with each other.
///
/// This may include io_uring file descriptors, lockable memory budget to register in kernel, etc.
#[derive(Default)]
pub struct IoSetupState {
    #[cfg(target_os = "linux")]
    shared_sqpoll_io_uring: Option<io_uring::IoUring>,
}

impl IoSetupState {
    /// Enables shared io-uring worker pool and sqpoll based kernel thread.
    ///
    /// The sqpoll thread will drain submission queues from all io-uring instances created
    /// through builder obtained from `create_io_uring_builder()` after this call.
    pub fn with_shared_sqpoll(self) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let sqpoll_io_uring = io_uring::IoUring::builder()
                .setup_sqpoll(SQPOLL_IDLE_WAIT_TIME)
                .build(1)?;
            Ok(Self {
                shared_sqpoll_io_uring: Some(sqpoll_io_uring),
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(Self {})
        }
    }

    /// Return new io-uring builder that is attached shared worker pool (if configured).
    #[cfg(target_os = "linux")]
    pub fn create_io_uring_builder(&self) -> io_uring::Builder {
        use std::os::fd::AsRawFd;

        let mut builder = io_uring::IoUring::builder();
        if let Some(io_uring_backend) = &self.shared_sqpoll_io_uring {
            builder
                .setup_attach_wq(io_uring_backend.as_raw_fd())
                .setup_sqpoll(SQPOLL_IDLE_WAIT_TIME);
        }
        builder
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
        let io_backend = IoSetupState::default().with_shared_sqpoll().unwrap();

        let read_bytes = RwLock::new(vec![]);
        let mut file_creator =
            IoUringFileCreator::with_buffer_capacity(1 << 20, &io_backend, |path| {
                let mut reader =
                    SequentialFileReader::with_capacity(1 << 20, path, &io_backend).unwrap();
                reader
                    .read_to_end(read_bytes.write().unwrap().as_mut())
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
