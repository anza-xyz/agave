use {
    crate::{
        hardened_unpack::{self, UnpackError},
        ArchiveFormat, ArchiveFormatDecompressor,
    },
    agave_fs::{buffered_reader, file_io::file_creator},
    bzip2::bufread::BzDecoder,
    crossbeam_channel::Sender,
    std::{
        fs,
        io::{self, BufRead, BufReader},
        path::{Path, PathBuf},
        thread::{self, JoinHandle},
        time::Instant,
    },
};

// Allows scheduling a large number of reads such that temporary disk access delays
// shouldn't block decompression (unless read bandwidth is saturated).
const MAX_SNAPSHOT_READER_BUF_SIZE: u64 = 128 * 1024 * 1024;
// The buffer should be large enough to saturate write I/O bandwidth, while also accommodating:
// - Many small files: each file consumes at least one write-capacity-sized chunk (0.5-1 MiB).
// - Large files: their data may accumulate in backlog buffers while waiting for file open
//   operations to complete.
const MAX_UNPACK_WRITE_BUF_SIZE: usize = 512 * 1024 * 1024;

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
        // Bound the buffer based on available memlock budget (reader and writer might use it to
        // register buffer in kernel) and input archive size (decompression multiplies content size,
        // but buffering more than origin isn't necessary).
        let read_write_budget_size = (memlock_budget_size / 2).min(archive_size);
        let read_buf_size = MAX_SNAPSHOT_READER_BUF_SIZE.min(read_write_budget_size as u64);
        let decompressor = decompressed_tar_reader(archive_format, archive_path, read_buf_size)?;

        let write_buf_size = MAX_UNPACK_WRITE_BUF_SIZE.min(read_write_budget_size);
        let file_creator = file_creator(write_buf_size, move |file_path| {
            let result = file_sender.send(file_path);
            if let Err(err) = result {
                panic!(
                    "failed to send path '{}' from unpacker to rebuilder: {err}",
                    err.0.display(),
                );
            }
        })?;

        hardened_unpack::streaming_unpack_snapshot(
            decompressor,
            file_creator,
            ledger_dir.as_path(),
            &account_paths,
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

pub fn unpack_genesis_archive(
    archive_filename: &Path,
    destination_dir: &Path,
    max_genesis_archive_unpacked_size: u64,
) -> Result<(), UnpackError> {
    log::info!("Extracting {archive_filename:?}...");
    let extract_start = Instant::now();

    fs::create_dir_all(destination_dir)?;
    let tar_bz2 = fs::File::open(archive_filename)?;
    let tar = BzDecoder::new(BufReader::new(tar_bz2));
    let file_creator = file_creator(
        0, /* don't provide memlock budget (forces sync IO), since genesis archives are small */
        |_| {},
    )?;
    hardened_unpack::unpack_genesis(
        tar,
        file_creator,
        destination_dir,
        max_genesis_archive_unpacked_size,
    )?;
    log::info!(
        "Extracted {:?} in {:?}",
        archive_filename,
        Instant::now().duration_since(extract_start)
    );
    Ok(())
}

fn decompressed_tar_reader(
    archive_format: ArchiveFormat,
    archive_path: impl AsRef<Path>,
    buf_size: u64,
) -> io::Result<ArchiveFormatDecompressor<impl BufRead>> {
    let buf_reader =
        buffered_reader::large_file_buf_reader(archive_path.as_ref(), buf_size as usize)?;
    ArchiveFormatDecompressor::new(archive_format, buf_reader)
}
