use std::io;
#[cfg(target_os = "linux")]
use {
    crate::io_uring::sqpoll::SharedSqPoll,
    std::os::fd::{AsFd as _, BorrowedFd},
};

/// State used by IO utilities for managing shared resources and configuration during setup.
///
/// This may include io_uring file descriptors, flag whether to register memory buffers in kernel,
/// and other resources that need to be accessed by multiple functions performing IO operations
/// such that they can efficiently cooperate with each other.
///
/// This is achieved by creating `IoSetupState` at the beginning of the processing setup and
/// passing its reference to IO utilities that need sharing / customized options.
///
/// Note: the state needs to live only during creation of the IO utilities, not during their usage,
/// so it's generally advisable to drop it after setup is done such that e.g. init-only squeue is
/// released.
#[derive(Default)]
pub struct IoSetupState {
    #[cfg(target_os = "linux")]
    shared_sqpoll: Option<SharedSqPoll>,
}

impl IoSetupState {
    /// Enables shared io-uring worker pool and sqpoll based kernel thread.
    ///
    /// The sqpoll thread will drain submission queues from all io-uring instances created
    /// through builder obtained from `create_io_uring_builder()` with FD obtained from
    /// `shared_sqpoll_fd()` after this call.
    pub fn with_shared_sqpoll(self) -> io::Result<Self> {
        Ok(Self {
            #[cfg(target_os = "linux")]
            shared_sqpoll: Some(SharedSqPoll::new()?),
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn shared_sqpoll_fd(&self) -> Option<BorrowedFd<'_>> {
        self.shared_sqpoll.as_ref().map(|s| s.as_fd())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use {
        super::*,
        crate::{
            file_io::FileCreator,
            io_uring::{
                file_creator::IoUringFileCreator,
                sequential_file_reader::SequentialFileReaderBuilder,
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
        let io_setup = &IoSetupState::default().with_shared_sqpoll().unwrap();

        let read_bytes = RwLock::new(vec![]);
        let read_bytes_ref = &read_bytes;
        let mut file_creator = IoUringFileCreator::with_buffer_capacity(
            1 << 20,
            io_setup.shared_sqpoll_fd(),
            move |file_info| {
                let mut reader = SequentialFileReaderBuilder::new()
                    .shared_sqpoll(io_setup.shared_sqpoll_fd())
                    .build(file_info.path, 1 << 20)
                    .unwrap();
                reader
                    .read_to_end(read_bytes_ref.write().unwrap().as_mut())
                    .unwrap();
                None
            },
        )
        .unwrap();

        let temp_dir = tempfile::tempdir().unwrap();
        let dir_handle = Arc::new(File::open(temp_dir.path()).unwrap());
        let mut write_bytes = vec![0; 2 << 20];
        rand::rng().fill_bytes(&mut write_bytes);

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
