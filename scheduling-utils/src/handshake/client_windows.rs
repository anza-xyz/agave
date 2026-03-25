use {
    crate::handshake::{
        ClientHandshakeError, ClientLogon, ClientSession,
        pipe_windows::pipe_name,
        shared::{HANDSHAKE_BUFFER_SIZE, LOGON_FAILURE, join_session, send_logon},
    },
    std::{
        ffi::OsString,
        fs::{File, OpenOptions},
        io::Read,
        os::windows::{
            ffi::OsStringExt,
            fs::OpenOptionsExt,
            io::{FromRawHandle, RawHandle},
        },
        path::{Path, PathBuf},
        time::{Duration, Instant},
    },
    windows_sys::Win32::{
        Foundation::{ERROR_PIPE_BUSY, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
            FILE_SHARE_WRITE, GENERIC_READ, GENERIC_WRITE, OPEN_EXISTING,
        },
        System::Pipes::WaitNamedPipeW,
    },
};

pub fn connect(
    path: impl AsRef<Path>,
    logon: ClientLogon,
    timeout: Duration,
) -> Result<ClientSession, ClientHandshakeError> {
    connect_path(path.as_ref(), logon, timeout)
}

fn connect_path(
    path: &Path,
    logon: ClientLogon,
    timeout: Duration,
) -> Result<ClientSession, ClientHandshakeError> {
    let mut stream = connect_pipe(path, timeout)?;
    send_logon(&mut stream, logon)?;
    let files = recv_response(&mut stream)?;
    join_session(&logon, files)
}

pub fn setup_session(
    logon: &ClientLogon,
    files: Vec<File>,
) -> Result<ClientSession, ClientHandshakeError> {
    join_session(logon, files)
}

fn connect_pipe(path: &Path, timeout: Duration) -> Result<File, ClientHandshakeError> {
    let pipe_name = pipe_name(path);
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);

    loop {
        let handle = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                core::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                0,
            )
        };

        if handle != INVALID_HANDLE_VALUE {
            return Ok(unsafe { File::from_raw_handle(handle as RawHandle) });
        }

        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
            return Err(err.into());
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(ClientHandshakeError::TimedOut);
        }

        let remaining = deadline.saturating_duration_since(now).as_millis();
        let wait_ms = u32::try_from(remaining).unwrap_or(u32::MAX);
        let waited = unsafe { WaitNamedPipeW(pipe_name.as_ptr(), wait_ms) };
        if waited == 0 {
            let err = std::io::Error::last_os_error();
            if Instant::now() >= deadline {
                return Err(ClientHandshakeError::TimedOut);
            }
            return Err(err.into());
        }
    }
}

fn recv_response(stream: &mut File) -> Result<Vec<File>, ClientHandshakeError> {
    let mut marker = [0; 1];
    stream.read_exact(&mut marker)?;

    if marker[0] == LOGON_FAILURE {
        let mut reason_len = [0; 1];
        stream.read_exact(&mut reason_len)?;
        let mut reason = vec![0; usize::from(reason_len[0])];
        stream.read_exact(&mut reason)?;
        let reason =
            String::from_utf8(reason).map_err(|_| ClientHandshakeError::ProtocolViolation)?;
        return Err(ClientHandshakeError::Rejected(reason));
    }

    let mut count_bytes = [0; 4];
    stream.read_exact(&mut count_bytes)?;
    let count = u32::from_le_bytes(count_bytes);
    let mut files = Vec::with_capacity(count as usize);

    for _ in 0..count {
        let path = read_path(stream)?;
        files.push(open_region_file(&path)?);
    }

    Ok(files)
}

fn read_path(stream: &mut File) -> Result<PathBuf, ClientHandshakeError> {
    let mut len_bytes = [0; 4];
    stream.read_exact(&mut len_bytes)?;
    let byte_len = u32::from_le_bytes(len_bytes) as usize;
    if !byte_len.is_multiple_of(2) || byte_len > HANDSHAKE_BUFFER_SIZE * 64 {
        return Err(ClientHandshakeError::ProtocolViolation);
    }

    let mut bytes = vec![0; byte_len];
    stream.read_exact(&mut bytes)?;
    let wide = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

fn open_region_file(path: &Path) -> Result<File, ClientHandshakeError> {
    let mut open_options = OpenOptions::new();
    open_options.read(true).write(true);
    open_options.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE);
    open_options.open(path).map_err(Into::into)
}
