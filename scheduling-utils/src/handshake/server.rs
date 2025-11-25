use {
    crate::handshake::{
        shared::{
            GLOBAL_ALLOCATORS, LOGON_FAILURE, LOGON_SUCCESS, MAX_ALLOCATOR_HANDLES, MAX_WORKERS,
            VERSION,
        },
        ClientLogon,
    },
    agave_scheduler_bindings::{
        PackToWorkerMessage, ProgressMessage, TpuToPackMessage, WorkerToPackMessage,
    },
    nix::sys::socket::{self, ControlMessage, MsgFlags, UnixAddr},
    rts_alloc::Allocator,
    std::{
        ffi::CStr,
        fs::File,
        io::{IoSlice, Read, Write},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::net::{UnixListener, UnixStream},
        },
        path::Path,
        time::{Duration, Instant},
    },
    thiserror::Error,
};

type ShaqError = shaq::error::Error;
type RtsAllocError = rts_alloc::error::Error;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);
const SHMEM_NAME: &CStr = c"/agave-scheduler-bindings";

/// Implements the Agave side of the scheduler bindings handshake protocol.
pub struct Server {
    listener: UnixListener,

    buffer: [u8; 1024],
}

impl Server {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let listener = UnixListener::bind(path)?;

        Ok(Self {
            listener,
            buffer: [0; 1024],
        })
    }

    pub fn accept(&mut self) -> Result<AgaveSession, AgaveHandshakeError> {
        // Wait for next stream.
        let (mut stream, _) = self.listener.accept()?;
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;

        match self.handle_logon(&mut stream) {
            Ok(session) => Ok(session),
            Err(err) => {
                let reason = err.to_string();
                let reason_len = u8::try_from(reason.len()).unwrap_or(u8::MAX);

                let buffer_len = 2usize.checked_add(usize::from(reason_len)).unwrap();
                self.buffer[0] = LOGON_FAILURE;
                self.buffer[1] = reason_len;
                self.buffer[2..buffer_len]
                    .copy_from_slice(&reason.as_bytes()[..usize::from(reason_len)]);

                stream.set_nonblocking(true)?;
                // NB: Caller will still error out even if our write fails so it's fine to ignore the
                // result.
                let _ = stream.write(&self.buffer[..buffer_len])?;

                Err(err)
            }
        }
    }

    fn handle_logon(
        &mut self,
        stream: &mut UnixStream,
    ) -> Result<AgaveSession, AgaveHandshakeError> {
        // Receive & validate the logon message.
        let logon = self.recv_logon(stream)?;

        // Setup the requested shared memory regions.
        let (session, files) = Self::setup_session(logon)?;

        // Send the file descriptors to the client.
        let fds_raw: Vec<_> = files.iter().map(|file| file.as_raw_fd()).collect();
        let iov = [IoSlice::new(&[LOGON_SUCCESS])];
        let cmsgs = [ControlMessage::ScmRights(&fds_raw)];
        let sent =
            socket::sendmsg::<UnixAddr>(stream.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
                .map_err(std::io::Error::from)?;
        debug_assert_eq!(sent, 1);

        Ok(session)
    }

    fn recv_logon(&mut self, stream: &mut UnixStream) -> Result<ClientLogon, AgaveHandshakeError> {
        // Read the logon message.
        let handshake_start = Instant::now();
        let mut buffer_len = 0;
        while buffer_len < self.buffer.len() {
            let read = stream.read(&mut self.buffer[buffer_len..])?;
            if read == 0 {
                return Err(AgaveHandshakeError::EofDuringHandshake);
            }

            // SAFETY: We cannot read a value greater than buffer.len() which itself is a usize.
            buffer_len = buffer_len.checked_add(read).unwrap();

            if handshake_start.elapsed() > HANDSHAKE_TIMEOUT {
                return Err(AgaveHandshakeError::Timeout);
            }
        }

        // Ensure exact version match, version will be bumped any time a backwards incompatible
        // change is made to handshake/shared memory objects.
        let version = u64::from_le_bytes(self.buffer[..8].try_into().unwrap());
        if version != VERSION {
            return Err(AgaveHandshakeError::Version {
                server: VERSION,
                client: version,
            });
        }

        // Read the logon message, cannot panic as we ensure the correct buf size at compile time
        // (hence the const just below).
        const LOGON_END: usize = 8 + core::mem::size_of::<ClientLogon>();
        let logon = ClientLogon::try_from_bytes(&self.buffer[8..LOGON_END]).unwrap();

        // Put a hard limit of 64 worker threads for now.
        if !(1..=MAX_WORKERS).contains(&logon.worker_count) {
            return Err(AgaveHandshakeError::WorkerCount(logon.worker_count));
        }

        // Hard limit allocator handles to 128.
        if !(1..=MAX_ALLOCATOR_HANDLES).contains(&logon.allocator_handles) {
            return Err(AgaveHandshakeError::AllocatorHandles(
                logon.allocator_handles,
            ));
        }

        Ok(logon)
    }

    pub fn setup_session(
        logon: ClientLogon,
    ) -> Result<(AgaveSession, Vec<File>), AgaveHandshakeError> {
        // Setup the allocator in shared memory (`worker_count` & `allocator_handles` have been
        // validated so this won't panic).
        let (allocator_file, tpu_to_pack_allocator) = Self::create_allocator(&logon)?;

        // Setup the global queues.
        let (tpu_to_pack_file, tpu_to_pack_queue) =
            Self::create_producer(logon.tpu_to_pack_capacity, true)?;
        let (progress_tracker_file, progress_tracker) =
            Self::create_producer(logon.progress_tracker_capacity, false)?;

        // Setup the worker sessions.
        let (worker_files, workers) = (0..logon.worker_count).try_fold(
            (Vec::default(), Vec::default()),
            |(mut fds, mut workers), offset| {
                // NB: Server validates all requested counts are within expected bands so this
                // should never panic.
                let worker_index = GLOBAL_ALLOCATORS.checked_add(offset).unwrap();
                let worker_index = u32::try_from(worker_index).unwrap();
                // SAFETY: Worker index is guaranteed to be unique.
                let allocator = unsafe { Allocator::join(&allocator_file, worker_index) }?;

                let (pack_to_worker_file, pack_to_worker) =
                    Self::create_consumer(logon.pack_to_worker_capacity)?;
                let (worker_to_pack_file, worker_to_pack) =
                    Self::create_producer(logon.worker_to_pack_capacity, true)?;

                fds.extend([pack_to_worker_file, worker_to_pack_file]);
                workers.push(AgaveWorkerSession {
                    allocator,
                    pack_to_worker,
                    worker_to_pack,
                });

                Ok::<_, AgaveHandshakeError>((fds, workers))
            },
        )?;

        Ok((
            AgaveSession {
                flags: logon.flags,
                tpu_to_pack: AgaveTpuToPackSession {
                    allocator: tpu_to_pack_allocator,
                    producer: tpu_to_pack_queue,
                },
                progress_tracker,
                workers,
            },
            [allocator_file, tpu_to_pack_file, progress_tracker_file]
                .into_iter()
                .chain(worker_files)
                .collect(),
        ))
    }

    fn create_allocator(logon: &ClientLogon) -> Result<(File, Allocator), RtsAllocError> {
        let allocator_count = GLOBAL_ALLOCATORS
            .checked_add(logon.worker_count)
            .unwrap()
            .checked_add(logon.allocator_handles)
            .unwrap();

        let create = |huge: bool| {
            let allocator_file = Self::create_shmem(huge)?;
            let allocator_file_size = Self::align_file_size(logon.allocator_size, huge);

            Allocator::create(
                &allocator_file,
                allocator_file_size,
                u32::try_from(allocator_count).unwrap(),
                2 * 1024 * 1024,
                0,
            )
            .map(|allocator| (allocator_file, allocator))
        };

        // Try to create with huge pages, fallback to regular pages.
        create(true).or_else(|_| create(false))
    }

    fn create_producer<T>(
        capacity: usize,
        huge: bool,
    ) -> Result<(File, shaq::Producer<T>), ShaqError> {
        let create = |huge: bool| {
            let file = Self::create_shmem(huge)?;
            let minimum_file_size = shaq::minimum_file_size::<T>(capacity);
            let file_size = Self::align_file_size(minimum_file_size, huge);

            // SAFETY: uniqely creating as producer
            unsafe { shaq::Producer::create(&file, file_size) }.map(|producer| (file, producer))
        };

        // Try to create with huge pages, fallback to regular pages.
        match huge {
            true => create(true).or_else(|_| create(false)),
            false => create(false),
        }
    }

    fn create_consumer(
        capacity: usize,
    ) -> Result<(File, shaq::Consumer<PackToWorkerMessage>), ShaqError> {
        let create = |huge: bool| {
            let file = Self::create_shmem(huge)?;
            let minimum_file_size = shaq::minimum_file_size::<PackToWorkerMessage>(capacity);
            let file_size = Self::align_file_size(minimum_file_size, huge);

            // SAFETY: uniquely creating as consumer.
            unsafe { shaq::Consumer::create(&file, file_size) }.map(|producer| (file, producer))
        };

        // Try to create with huge pages, fallback to regular pages.
        create(true).or_else(|_| create(false))
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "l4re",
        target_os = "android",
        target_os = "emscripten"
    ))]
    fn create_shmem(huge: bool) -> Result<File, std::io::Error> {
        let flags = match huge {
            true => libc::MFD_HUGETLB | libc::MFD_HUGE_2MB,
            false => 0,
        };

        unsafe {
            let ret = libc::memfd_create(SHMEM_NAME.as_ptr(), flags);
            if ret == -1 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(File::from_raw_fd(ret))
        }
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "l4re",
        target_os = "android",
        target_os = "emscripten"
    )))]
    fn create_shmem(huge: bool) -> Result<File, std::io::Error> {
        if huge {
            return Err(std::io::ErrorKind::Unsupported.into());
        }

        unsafe {
            // Clean up the previous link if one exists.
            let ret = libc::shm_unlink(SHMEM_NAME.as_ptr());
            if ret == -1 {
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::NotFound {
                    return Err(err);
                }
            }

            // Create a new shared memory object.
            let ret = libc::shm_open(
                SHMEM_NAME.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                #[cfg(not(target_os = "macos"))]
                {
                    libc::S_IRUSR | libc::S_IWUSR
                },
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                {
                    (libc::S_IRUSR | libc::S_IWUSR) as libc::c_uint
                },
            );
            if ret == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let file = File::from_raw_fd(ret);

            // Clean up after ourself.
            let ret = libc::shm_unlink(SHMEM_NAME.as_ptr());
            if ret == -1 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(file)
        }
    }

    fn align_file_size(size: usize, huge: bool) -> usize {
        match huge {
            true => size.next_multiple_of(2 * 1024 * 1024),
            false => size.next_multiple_of(4096),
        }
    }
}

/// An initialized scheduling session.
pub struct AgaveSession {
    pub flags: u16,
    pub tpu_to_pack: AgaveTpuToPackSession,
    pub progress_tracker: shaq::Producer<ProgressMessage>,
    pub workers: Vec<AgaveWorkerSession>,
}

/// Shared memory objects for the tpu to pack worker.
pub struct AgaveTpuToPackSession {
    pub allocator: Allocator,
    pub producer: shaq::Producer<TpuToPackMessage>,
}

/// Shared memory objects for a single banking worker.
pub struct AgaveWorkerSession {
    pub allocator: Allocator,
    pub pack_to_worker: shaq::Consumer<PackToWorkerMessage>,
    pub worker_to_pack: shaq::Producer<WorkerToPackMessage>,
}

/// Potential errors that can occur during the Agave side of the handshake.
///
/// # Note
///
/// These errors are stringified (up to 256 bytes then truncated) and sent to the client.
#[derive(Debug, Error)]
pub enum AgaveHandshakeError {
    #[error("Io; err={0}")]
    Io(#[from] std::io::Error),
    #[error("Timeout")]
    Timeout,
    #[error("Close during handshake")]
    EofDuringHandshake,
    #[error("Version; server={server}; client={client}")]
    Version { server: u64, client: u64 },
    #[error("Worker count; count={0}")]
    WorkerCount(usize),
    #[error("Allocator handles; count={0}")]
    AllocatorHandles(usize),
    #[error("Rts alloc; err={0:?}")]
    RtsAlloc(#[from] RtsAllocError),
    #[error("Shaq; err={0:?}")]
    Shaq(#[from] ShaqError),
}
