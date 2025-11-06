use {
    crate::{
        hardened_unpack::{self, UnpackError},
        ArchiveFormat, ArchiveFormatDecompressor,
    },
    agave_fs::{buffered_reader, file_io::file_creator, io_setup::IoSetupState},
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
const MAX_SNAPSHOT_READER_BUF_SIZE: usize = 128 * 1024 * 1024;
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

        let io_setup =
            IoSetupState::new_with_memlock_budget(memlock_budget_size).with_shared_sqpoll()?;

        // Bound the buffers based input archive size (decompression multiplies content size,
        // but buffering more than origin isn't necessary).
        let read_buf_size = MAX_SNAPSHOT_READER_BUF_SIZE.min(archive_size);
        let write_buf_size = MAX_UNPACK_WRITE_BUF_SIZE.min(archive_size);

        let file_creator = file_creator(write_buf_size, io_setup.clone(), move |file_path| {
            let result = file_sender.send(file_path);
            if let Err(err) = result {
                panic!(
                    "failed to send path '{}' from unpacker to rebuilder: {err}",
                    err.0.display(),
                );
            }
        })?;

        let decompressor =
            decompressed_tar_reader(archive_format, archive_path, read_buf_size, io_setup)?;

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

    // don't need to provide memlock budget (forces sync IO), since genesis archives are small
    let io_setup = IoSetupState::default();

    fs::create_dir_all(destination_dir)?;
    let tar_bz2 = fs::File::open(archive_filename)?;
    let archive_size = tar_bz2.metadata()?.len() as usize;
    let tar = BzDecoder::new(BufReader::new(tar_bz2));
    let file_creator = file_creator(
        archive_size.min(MAX_UNPACK_WRITE_BUF_SIZE),
        io_setup,
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
    desired_buf_size: usize,
    io_setup: IoSetupState,
) -> io::Result<ArchiveFormatDecompressor<impl BufRead>> {
    let buf_reader =
        buffered_reader::large_file_buf_reader(archive_path.as_ref(), desired_buf_size, io_setup)?;
    ArchiveFormatDecompressor::new(archive_format, buf_reader)
}
