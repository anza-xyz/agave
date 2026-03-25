use {
    crate::handshake::{
        AgaveHandshakeError, AgaveTpuToPackSession, AgaveWorkerSession, ClientLogon,
        pipe_windows::pipe_name,
        shared::{
            AgaveSession, GLOBAL_ALLOCATORS, HANDSHAKE_BUFFER_SIZE, LOGON_FAILURE, LOGON_SUCCESS,
            recv_logon,
        },
    },
    agave_scheduler_bindings::PackToWorkerMessage,
    rts_alloc::Allocator,
    std::{
        ffi::OsStr,
        fs::{File, OpenOptions},
        io::Write,
        os::windows::{
            ffi::OsStrExt,
            fs::OpenOptionsExt,
            io::{FromRawHandle, RawHandle},
        },
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    },
    windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            FILE_ATTRIBUTE_TEMPORARY, FILE_FLAG_DELETE_ON_CLOSE, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE,
        },
        System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_ACCESS_DUPLEX,
            PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
        },
    },
};

type ShaqError = shaq::error::Error;
type RtsAllocError = rts_alloc::error::Error;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);

pub struct Server {
    pipe_name: Vec<u16>,
    buffer: [u8; HANDSHAKE_BUFFER_SIZE],
}

struct RegionFile {
    file: File,
    path: PathBuf,
}

impl Server {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        Ok(Self {
            pipe_name: pipe_name(path.as_ref()),
            buffer: [0; HANDSHAKE_BUFFER_SIZE],
        })
    }

    pub fn accept(&mut self) -> Result<AgaveSession, AgaveHandshakeError> {
        let handle = unsafe {
            CreateNamedPipeW(
                self.pipe_name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                HANDSHAKE_BUFFER_SIZE as u32,
                HANDSHAKE_BUFFER_SIZE as u32,
                timeout_ms(HANDSHAKE_TIMEOUT),
                core::ptr::null(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error().into());
        }

        let connected = unsafe { ConnectNamedPipe(handle, core::ptr::null_mut()) };
        if connected == 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(ERROR_PIPE_CONNECTED as i32) {
                unsafe {
                    CloseHandle(handle);
                }
                return Err(err.into());
            }
        }

        let mut stream = unsafe { File::from_raw_handle(handle as RawHandle) };
        let result = match self.handle_logon(&mut stream) {
            Ok(session) => Ok(session),
            Err(err) => {
                let reason = err.to_string();
                let reason_len = u8::try_from(reason.len()).unwrap_or(u8::MAX);

                let mut buffer = [0; HANDSHAKE_BUFFER_SIZE];
                let buffer_len = 2usize.checked_add(usize::from(reason_len)).unwrap();
                buffer[0] = LOGON_FAILURE;
                buffer[1] = reason_len;
                buffer[2..buffer_len]
                    .copy_from_slice(&reason.as_bytes()[..usize::from(reason_len)]);

                let _ = stream.write_all(&buffer[..buffer_len]);
                Err(err)
            }
        };

        unsafe {
            DisconnectNamedPipe(handle);
        }

        result
    }

    fn handle_logon(&mut self, stream: &mut File) -> Result<AgaveSession, AgaveHandshakeError> {
        let logon = recv_logon(stream, &mut self.buffer)?;
        let (session, files) = Self::setup_session_regions(logon)?;
        write_success_response(stream, &files)?;
        Ok(session)
    }

    pub fn setup_session(
        logon: ClientLogon,
    ) -> Result<(AgaveSession, Vec<File>), AgaveHandshakeError> {
        let (session, files) = Self::setup_session_regions(logon)?;
        Ok((session, files.into_iter().map(|file| file.file).collect()))
    }

    fn setup_session_regions(
        logon: ClientLogon,
    ) -> Result<(AgaveSession, Vec<RegionFile>), AgaveHandshakeError> {
        let (allocator_file, tpu_to_pack_allocator) = Self::create_allocator(&logon)?;
        let (tpu_to_pack_file, tpu_to_pack_queue) =
            Self::create_producer(logon.tpu_to_pack_capacity, true)?;
        let (progress_tracker_file, progress_tracker) =
            Self::create_producer(logon.progress_tracker_capacity, false)?;

        let (worker_files, workers) = (0..logon.worker_count).try_fold(
            (Vec::default(), Vec::default()),
            |(mut files, mut workers), _| {
                let allocator = Allocator::join(&allocator_file.file)?;

                let (pack_to_worker_file, pack_to_worker) =
                    Self::create_consumer(logon.pack_to_worker_capacity)?;
                let (worker_to_pack_file, worker_to_pack) =
                    Self::create_producer(logon.worker_to_pack_capacity, true)?;

                files.extend([pack_to_worker_file, worker_to_pack_file]);
                workers.push(AgaveWorkerSession {
                    allocator,
                    pack_to_worker,
                    worker_to_pack,
                });

                Ok::<_, AgaveHandshakeError>((files, workers))
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

    fn create_allocator(logon: &ClientLogon) -> Result<(RegionFile, Allocator), RtsAllocError> {
        let allocator_count = GLOBAL_ALLOCATORS
            .checked_add(logon.worker_count)
            .unwrap()
            .checked_add(logon.allocator_handles)
            .unwrap();

        let create = |huge: bool| {
            let allocator_file = Self::create_shmem(huge)?;
            let allocator_file_size = Self::align_file_size(logon.allocator_size, huge);

            unsafe {
                Allocator::create(
                    &allocator_file.file,
                    allocator_file_size,
                    u32::try_from(allocator_count).unwrap(),
                    2 * 1024 * 1024,
                )
            }
            .map(|allocator| (allocator_file, allocator))
        };

        create(true).or_else(|_| create(false))
    }

    fn create_producer<T>(
        capacity: usize,
        huge: bool,
    ) -> Result<(RegionFile, shaq::spsc::Producer<T>), ShaqError> {
        let create = |huge: bool| {
            let file = Self::create_shmem(huge)?;
            let minimum_file_size = shaq::spsc::minimum_file_size::<T>(capacity);
            let file_size = Self::align_file_size(minimum_file_size, huge);

            unsafe { shaq::spsc::Producer::create(&file.file, file_size) }
                .map(|producer| (file, producer))
        };

        match huge {
            true => create(true).or_else(|_| create(false)),
            false => create(false),
        }
    }

    fn create_consumer(
        capacity: usize,
    ) -> Result<(RegionFile, shaq::spsc::Consumer<PackToWorkerMessage>), ShaqError> {
        let create = |huge: bool| {
            let file = Self::create_shmem(huge)?;
            let minimum_file_size = shaq::spsc::minimum_file_size::<PackToWorkerMessage>(capacity);
            let file_size = Self::align_file_size(minimum_file_size, huge);

            unsafe { shaq::spsc::Consumer::create(&file.file, file_size) }
                .map(|producer| (file, producer))
        };

        create(true).or_else(|_| create(false))
    }

    fn create_shmem(huge: bool) -> Result<RegionFile, std::io::Error> {
        if huge {
            return Err(std::io::ErrorKind::Unsupported.into());
        }

        let path = unique_region_path();
        let mut open_options = OpenOptions::new();
        open_options.read(true).write(true).create_new(true);
        open_options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
        open_options.attributes(FILE_ATTRIBUTE_TEMPORARY);
        open_options.custom_flags(FILE_FLAG_DELETE_ON_CLOSE);
        let file = open_options.open(&path)?;

        Ok(RegionFile { file, path })
    }

    fn align_file_size(size: usize, huge: bool) -> usize {
        match huge {
            true => size.next_multiple_of(2 * 1024 * 1024),
            false => size.next_multiple_of(4096),
        }
    }
}

fn write_success_response(
    stream: &mut File,
    files: &[RegionFile],
) -> Result<(), AgaveHandshakeError> {
    stream.write_all(&[LOGON_SUCCESS])?;
    stream.write_all(&(u32::try_from(files.len()).unwrap()).to_le_bytes())?;
    for file in files {
        write_path(stream, &file.path)?;
    }
    Ok(())
}

fn write_path(stream: &mut File, path: &Path) -> Result<(), AgaveHandshakeError> {
    let wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let byte_len = wide
        .len()
        .checked_mul(2)
        .and_then(|len| u32::try_from(len).ok())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
    stream.write_all(&byte_len.to_le_bytes())?;
    for unit in wide {
        stream.write_all(&unit.to_le_bytes())?;
    }
    Ok(())
}

fn unique_region_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("agave-scheduler-bindings-{pid}-{id}.shmem"))
}

fn timeout_ms(timeout: Duration) -> u32 {
    u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX)
}
