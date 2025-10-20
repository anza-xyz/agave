use {
    crate::{
        hardened_unpack::{self, UnpackError},
        ArchiveFormat, ArchiveFormatDecompressor,
    },
    crossbeam_channel::Sender,
    std::{
        fs,
        io::{self, BufRead},
        path::{Path, PathBuf},
        thread::{self, JoinHandle},
    },
};

// Allows scheduling a large number of reads such that temporary disk access delays
// shouldn't block decompression (unless read bandwidth is saturated).
const MAX_SNAPSHOT_READER_BUF_SIZE: u64 = 128 * 1024 * 1024;

/// Streams unpacked files across channel
pub fn streaming_unarchive_snapshot(
    file_sender: Sender<PathBuf>,
    account_paths: Vec<PathBuf>,
    ledger_dir: PathBuf,
    snapshot_archive_path: PathBuf,
    archive_format: ArchiveFormat,
    memlock_budget_size: usize,
) -> JoinHandle<Result<(), UnpackError>> {
    let do_unpack = move |archive_path: &Path| {
        let archive_size = fs::metadata(archive_path)?.len() as usize;
        let read_write_budget_size = (memlock_budget_size / 2).min(archive_size);
        let read_buf_size = MAX_SNAPSHOT_READER_BUF_SIZE.min(read_write_budget_size as u64);
        let decompressor = decompressed_tar_reader(archive_format, archive_path, read_buf_size)?;
        hardened_unpack::streaming_unpack_snapshot(
            decompressor,
            read_write_budget_size,
            ledger_dir.as_path(),
            &account_paths,
            &file_sender,
        )
    };

    thread::Builder::new()
        .name("solTarUnpack".to_string())
        .spawn(move || {
            do_unpack(&snapshot_archive_path)
                .map_err(|err| UnpackError::Unpack(Box::new(err), snapshot_archive_path))
        })
        .unwrap()
}

fn decompressed_tar_reader(
    archive_format: ArchiveFormat,
    archive_path: impl AsRef<Path>,
    buf_size: u64,
) -> io::Result<ArchiveFormatDecompressor<impl BufRead>> {
    let buf_reader =
        solana_accounts_db::large_file_buf_reader(archive_path.as_ref(), buf_size as usize)?;
    ArchiveFormatDecompressor::new(archive_format, buf_reader)
}
