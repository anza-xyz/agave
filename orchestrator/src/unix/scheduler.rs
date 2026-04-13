use {
    agave_scheduler_bindings::{
        PackToWorkerMessage, ProgressMessage, TpuToPackMessage, WorkerToPackMessage,
    },
    nix::sys::socket::{self, ControlMessage, ControlMessageOwned, MsgFlags, UnixAddr},
    rts_alloc::Allocator,
    serde::Deserialize,
    std::{
        ffi::CStr,
        fs::File,
        io::IoSlice,
        os::{
            fd::{AsRawFd, BorrowedFd, FromRawFd},
            unix::net::UnixStream,
        },
    },
};

const SHMEM_NAME: &CStr = c"/agave-scheduler-bindings";

/// Number of global shared memory objects (allocator + tpu_to_pack + progress_tracker).
const GLOBAL_SHMEM: usize = 3;
pub const MAX_WORKERS: usize = 64;
pub const MAX_ALLOCATOR_HANDLES: usize = 128;
const GLOBAL_ALLOCATORS: usize = 1;

/// Maximum FD count: 3 global + 2 per worker (at MAX_WORKERS).
const MAX_FDS: usize = GLOBAL_SHMEM + MAX_WORKERS * 2;

/// The maximum size in bytes of the control message buffer for SCM_RIGHTS.
const CMSG_MAX_SIZE: usize = MAX_FDS * 4;

// ---------------------------------------------------------------------------
// Session types
// ---------------------------------------------------------------------------

/// An initialized scheduling session (Agave side).
pub struct AgaveSession {
    pub flags: u16,
    pub tpu_to_pack: AgaveTpuToPackSession,
    pub progress_tracker: shaq::spsc::Producer<ProgressMessage>,
    pub workers: Vec<AgaveWorkerSession>,
}

/// Shared memory objects for the tpu to pack worker.
pub struct AgaveTpuToPackSession {
    pub allocator: Allocator,
    pub producer: shaq::spsc::Producer<TpuToPackMessage>,
}

/// Shared memory objects for a single banking worker.
pub struct AgaveWorkerSession {
    pub allocator: Allocator,
    pub pack_to_worker: shaq::spsc::Consumer<PackToWorkerMessage>,
    pub worker_to_pack: shaq::spsc::Producer<WorkerToPackMessage>,
}

/// The complete initialized scheduling session (client/scheduler side).
pub struct ClientSession {
    pub allocators: Vec<Allocator>,
    pub tpu_to_pack: shaq::spsc::Consumer<TpuToPackMessage>,
    pub progress_tracker: shaq::spsc::Consumer<ProgressMessage>,
    pub workers: Vec<ClientWorkerSession>,
}

/// A per worker scheduling session (client/scheduler side).
pub struct ClientWorkerSession {
    pub pack_to_worker: shaq::spsc::Producer<PackToWorkerMessage>,
    pub worker_to_pack: shaq::spsc::Consumer<WorkerToPackMessage>,
}

#[derive(Debug, Deserialize)]
pub struct SchedulerTopology {
    pub worker_count: usize,
    pub allocator_size: usize,
    pub allocator_handles: usize,
    pub tpu_to_pack_capacity: usize,
    pub progress_tracker_capacity: usize,
    pub pack_to_worker_capacity: usize,
    pub worker_to_pack_capacity: usize,
    // TODO: Remove?
    #[serde(default)]
    pub flags: u16,
}

/// Header sent alongside shmem FDs so the receiver knows how to interpret them.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct SessionHeader {
    pub worker_count: u32,
    pub allocator_handles: u32,
    pub flags: u16,
    _padding: [u8; 2],
}

impl SessionHeader {
    pub fn new(worker_count: usize, allocator_handles: usize, flags: u16) -> Self {
        Self {
            worker_count: u32::try_from(worker_count).unwrap(),
            allocator_handles: u32::try_from(allocator_handles).unwrap(),
            flags,
            _padding: [0; 2],
        }
    }

    fn to_bytes(self) -> [u8; core::mem::size_of::<Self>()] {
        // SAFETY: SessionHeader is repr(C) and all byte patterns are valid.
        unsafe { core::mem::transmute(self) }
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < core::mem::size_of::<Self>() {
            return None;
        }
        // SAFETY: SessionHeader is repr(C) and all byte patterns are valid.
        Some(unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast()) })
    }
}

/// Creates all shared memory regions for a scheduling session.
///
/// Returns the FDs in canonical order:
/// `[allocator, tpu_to_pack, progress_tracker, pack_to_worker_0, worker_to_pack_0, ...]`
///
/// Each shmem region is initialized (headers written) so that both sides can `join`.
pub fn create_session(topology: &SchedulerTopology) -> Vec<File> {
    assert!(
        (1..=MAX_WORKERS).contains(&topology.worker_count),
        "worker_count out of range: {}",
        topology.worker_count,
    );
    assert!(
        (1..=MAX_ALLOCATOR_HANDLES).contains(&topology.allocator_handles),
        "allocator_handles out of range: {}",
        topology.allocator_handles,
    );

    // Create allocator shmem.
    let allocator_count = GLOBAL_ALLOCATORS
        .checked_add(topology.worker_count)
        .unwrap()
        .checked_add(topology.allocator_handles)
        .unwrap();
    let allocator_file = create_allocator(
        topology.allocator_size,
        u32::try_from(allocator_count).unwrap(),
    );

    // Create global queues.
    let tpu_to_pack_file = create_producer::<TpuToPackMessage>(topology.tpu_to_pack_capacity, true);
    let progress_tracker_file =
        create_producer::<ProgressMessage>(topology.progress_tracker_capacity, false);

    // Create per-worker queues.
    let mut files = vec![allocator_file, tpu_to_pack_file, progress_tracker_file];
    for _ in 0..topology.worker_count {
        let pack_to_worker_file =
            create_consumer::<PackToWorkerMessage>(topology.pack_to_worker_capacity);
        let worker_to_pack_file =
            create_producer::<WorkerToPackMessage>(topology.worker_to_pack_capacity, true);
        files.push(pack_to_worker_file);
        files.push(worker_to_pack_file);
    }

    files
}

/// Sends a session header + shmem FDs over a UDS via SCM_RIGHTS.
pub fn send_session(stream: BorrowedFd, files: &[File], header: SessionHeader) {
    let fds_raw: Vec<_> = files.iter().map(|file| file.as_raw_fd()).collect();
    let header_bytes = header.to_bytes();
    let iov = [IoSlice::new(&header_bytes)];
    let cmsgs = [ControlMessage::ScmRights(&fds_raw)];
    let sent =
        socket::sendmsg::<UnixAddr>(stream.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
            .expect("sendmsg failed");
    debug_assert_eq!(sent, header_bytes.len());
}

/// Receives shmem FDs from the orchestrator and constructs an [`AgaveSession`].
///
/// Applies the given timeout to the underlying socket before receiving. Pass `None` to
/// block indefinitely (useful for hot-swap listener threads). For one-shot recvs, the
/// caller should pick a generous timeout (e.g. 5 s) to cover the orchestrator spawn race.
pub fn recv_agave_session(
    stream: &UnixStream,
    timeout: Option<std::time::Duration>,
) -> AgaveSession {
    stream.set_read_timeout(timeout).expect("set_read_timeout");
    let (header, files) = recv_session(stream);

    join_agave_session(&header, &files)
}

/// Receives the client session configuration.
///
/// # Safety
///
/// - The caller must uphold `crate::orchestrator_uds` safety requirements.
pub unsafe fn recv_client_session() -> ClientSession {
    // SAFETY: Caller upholds `orchestrator_uds` safety requirements.
    let stream = unsafe { crate::orchestrator_uds() };
    stream.set_nonblocking(true).expect("set_nonblocking");
    let (header, files) = recv_session(&stream);

    // NB: We want the FD to release on process exit, not at the end of this
    // function.
    core::mem::forget(stream);

    join_client_session(&header, &files)
}

/// Joins pre-initialized shmem files as an [`AgaveSession`].
///
/// The files must be in canonical order as returned by [`create_session`].
pub fn join_agave_session(header: &SessionHeader, files: &[File]) -> AgaveSession {
    let worker_count = header.worker_count as usize;

    let (global_files, worker_files) = files.split_at(GLOBAL_SHMEM);
    let [allocator_file, tpu_to_pack_file, progress_tracker_file] = global_files else {
        unreachable!();
    };

    // Agave joins: tpu_to_pack as Producer, progress_tracker as Producer.
    let tpu_to_pack_allocator = Allocator::join(allocator_file).expect("allocator join");
    let tpu_to_pack_queue =
        unsafe { shaq::spsc::Producer::join(tpu_to_pack_file) }.expect("tpu_to_pack join");
    let progress_tracker =
        unsafe { shaq::spsc::Producer::join(progress_tracker_file) }.expect("progress join");

    // Per-worker: pack_to_worker as Consumer, worker_to_pack as Producer.
    assert_eq!(worker_files.len(), worker_count.checked_mul(2).unwrap());
    let workers = worker_files
        .chunks(2)
        .map(|chunk| {
            let [pack_to_worker_file, worker_to_pack_file] = chunk else {
                panic!();
            };
            let allocator = Allocator::join(allocator_file).expect("worker allocator join");
            let pack_to_worker = unsafe { shaq::spsc::Consumer::join(pack_to_worker_file) }
                .expect("pack_to_worker join");
            let worker_to_pack = unsafe { shaq::spsc::Producer::join(worker_to_pack_file) }
                .expect("worker_to_pack join");

            AgaveWorkerSession {
                allocator,
                pack_to_worker,
                worker_to_pack,
            }
        })
        .collect();

    AgaveSession {
        flags: header.flags,
        tpu_to_pack: AgaveTpuToPackSession {
            allocator: tpu_to_pack_allocator,
            producer: tpu_to_pack_queue,
        },
        progress_tracker,
        workers,
    }
}

/// Joins pre-initialized shmem files as a [`ClientSession`].
///
/// The files must be in canonical order as returned by [`create_session`].
pub fn join_client_session(header: &SessionHeader, files: &[File]) -> ClientSession {
    let worker_count = header.worker_count as usize;
    let allocator_handles = header.allocator_handles as usize;

    let (global_files, worker_files) = files.split_at(GLOBAL_SHMEM);
    let [allocator_file, tpu_to_pack_file, progress_tracker_file] = global_files else {
        unreachable!();
    };

    // Client joins: allocator x allocator_handles.
    let allocators = (0..allocator_handles)
        .map(|_| Allocator::join(allocator_file).expect("allocator join"))
        .collect();

    // Client joins: tpu_to_pack as Consumer, progress_tracker as Consumer.
    let tpu_to_pack =
        unsafe { shaq::spsc::Consumer::join(tpu_to_pack_file) }.expect("tpu_to_pack join");
    let progress_tracker =
        unsafe { shaq::spsc::Consumer::join(progress_tracker_file) }.expect("progress join");

    // Per-worker: pack_to_worker as Producer, worker_to_pack as Consumer.
    assert_eq!(worker_files.len(), worker_count.checked_mul(2).unwrap());
    let workers = worker_files
        .chunks(2)
        .map(|chunk| {
            let [pack_to_worker_file, worker_to_pack_file] = chunk else {
                panic!();
            };
            let pack_to_worker = unsafe { shaq::spsc::Producer::join(pack_to_worker_file) }
                .expect("pack_to_worker join");
            let worker_to_pack = unsafe { shaq::spsc::Consumer::join(worker_to_pack_file) }
                .expect("worker_to_pack join");

            ClientWorkerSession {
                pack_to_worker,
                worker_to_pack,
            }
        })
        .collect();

    ClientSession {
        allocators,
        tpu_to_pack,
        progress_tracker,
        workers,
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Receives a SessionHeader + FDs from a UDS.
fn recv_session(stream: &UnixStream) -> (SessionHeader, Vec<File>) {
    use std::io::IoSliceMut;

    let mut buf = [0u8; core::mem::size_of::<SessionHeader>()];
    let mut iov = [IoSliceMut::new(&mut buf)];
    // SAFETY: CMSG_LEN is always safe (const expression).
    let mut cmsgs = [0u8; unsafe { libc::CMSG_LEN(CMSG_MAX_SIZE as u32) as usize }];
    let msg = socket::recvmsg::<UnixAddr>(
        stream.as_raw_fd(),
        &mut iov,
        Some(&mut cmsgs),
        MsgFlags::empty(),
    )
    .expect("recvmsg failed");

    // Extract FDs from SCM_RIGHTS before dropping msg (which borrows buf).
    let mut cmsgs_iter = msg.cmsgs().unwrap();
    let fds = match cmsgs_iter.next() {
        Some(ControlMessageOwned::ScmRights(fds)) => fds,
        Some(msg) => panic!("unexpected cmsg: {msg:?}"),
        None => panic!("no SCM_RIGHTS received, orchestrator shutdown?"),
    };
    let _ = cmsgs_iter;
    let _ = msg;

    // Cast header.
    assert_eq!(msg.bytes, buf.len());
    let header = SessionHeader::from_bytes(&buf).expect("invalid session header");

    let expected_count = GLOBAL_SHMEM
        .checked_add((header.worker_count as usize).checked_mul(2).unwrap())
        .unwrap();
    assert_eq!(
        fds.len(),
        expected_count,
        "unexpected FD count: got {}, expected {expected_count}",
        fds.len(),
    );

    // SAFETY: FDs were just received via SCM_RIGHTS and are valid.
    let files = fds
        .into_iter()
        .map(|fd| unsafe { File::from_raw_fd(fd) })
        .collect();

    (header, files)
}

/// Creates an allocator shmem region, initializes it, then drops the handle (keeps the File).
fn create_allocator(allocator_size: usize, allocator_count: u32) -> File {
    let create = |huge: bool| {
        let file = create_shmem(huge)?;
        let file_size = align_file_size(allocator_size, huge);

        // SAFETY: We just created this file and thus can uniquely initialize it.
        let _allocator =
            unsafe { Allocator::create(&file, file_size, allocator_count, 2 * 1024 * 1024) }
                .map_err(std::io::Error::other)?;

        Ok::<_, std::io::Error>(file)
    };

    create(true).unwrap_or_else(|_| create(false).expect("allocator creation failed"))
}

/// Creates a producer-initialized shmem queue, drops the producer, keeps the File.
fn create_producer<T>(capacity: usize, huge: bool) -> File {
    let create = |huge: bool| {
        let file = create_shmem(huge)?;
        let minimum_file_size = shaq::spsc::minimum_file_size::<T>(capacity);
        let file_size = align_file_size(minimum_file_size, huge);

        // SAFETY: Uniquely creating as producer.
        let _producer = unsafe { shaq::spsc::Producer::<T>::create(&file, file_size) }
            .map_err(std::io::Error::other)?;

        Ok::<_, std::io::Error>(file)
    };

    match huge {
        true => create(true).unwrap_or_else(|_| create(false).expect("producer creation failed")),
        false => create(false).expect("producer creation failed"),
    }
}

/// Creates a consumer-initialized shmem queue, drops the consumer, keeps the File.
fn create_consumer<T>(capacity: usize) -> File {
    let create = |huge: bool| {
        let file = create_shmem(huge)?;
        let minimum_file_size = shaq::spsc::minimum_file_size::<T>(capacity);
        let file_size = align_file_size(minimum_file_size, huge);

        // SAFETY: Uniquely creating as consumer.
        let _consumer = unsafe { shaq::spsc::Consumer::<T>::create(&file, file_size) }
            .map_err(std::io::Error::other)?;

        Ok::<_, std::io::Error>(file)
    };

    create(true).unwrap_or_else(|_| create(false).expect("consumer creation failed"))
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
