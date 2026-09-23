use {
    crate::{
        ArchiveFormat, Result, SnapshotArchiveKind, error::ArchiveSnapshotPackageError,
        multiframe::MultiFrameZstdWriter, paths, snapshot_archive_info::SnapshotArchiveInfo,
        snapshot_hash::SnapshotHash,
    },
    agave_fs::{
        FileSize,
        buffered_reader::FileBufRead as _,
        buffered_writer::{SizeLimitedWriter, large_file_buf_writer},
        io_setup::IoSetupState,
    },
    log::info,
    solana_accounts_db::{
        account_storage::AccountStoragesOrderer,
        account_storage_entry::AccountStorageEntry,
        account_storage_reader::{
            ACCOUNT_STORAGE_MAX_BUFFER_SIZE, AccountStorageReader, TombstonesFilter,
            open_storage_files, storage_file_buf_reader,
        },
        accounts_file::AccountsFile,
    },
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_metrics::datapoint_info,
    std::{
        fs,
        io::{self, Write},
        path::Path,
        sync::Arc,
    },
};

// Balance large and small files order in snapshot tar with bias towards small (4 small + 1 large),
// such that during unpacking large writes are mixed with file metadata operations
// and towards the end of archive (sizes equalize) writes are >256KiB / file.
const INTERLEAVE_TAR_ENTRIES_SMALL_TO_LARGE_RATIO: (usize, usize) = (4, 1);

// Max number of storage files to open at once for archiving. Bounds extra fd
// usage and io_uring prefetch queue depth. A multiple of the interleave ratio
// sum keeps each chunk balanced between small and large files.
const STORAGE_FILE_OPEN_CHUNK_SIZE: usize = 25
    * (INTERLEAVE_TAR_ENTRIES_SMALL_TO_LARGE_RATIO.0
        + INTERLEAVE_TAR_ENTRIES_SMALL_TO_LARGE_RATIO.1);

// Uncompressed bytes per zstd frame. 32 MiB keeps compression loss below ~0.02
// while giving parallel decompressors enough chunk size.
const ZSTD_FRAME_SIZE: u32 = 32 * 1024 * 1024;

/// Archives a snapshot into `archive_path`
pub fn archive_snapshot(
    snapshot_archive_kind: SnapshotArchiveKind,
    snapshot_slot: Slot,
    snapshot_hash: SnapshotHash,
    snapshot_storages: &[Arc<AccountStorageEntry>],
    bank_snapshot_dir: impl AsRef<Path>,
    archive_path: impl AsRef<Path>,
    archive_format: ArchiveFormat,
    io_setup: &IoSetupState,
) -> Result<SnapshotArchiveInfo> {
    use ArchiveSnapshotPackageError as E;
    const ACCOUNTS_DIR: &str = "accounts";
    info!("Generating snapshot archive for slot {snapshot_slot}, kind: {snapshot_archive_kind:?}");

    let mut timer = Measure::start("snapshot_package-package_snapshots");
    let tar_dir = archive_path
        .as_ref()
        .parent()
        .expect("Tar output path is invalid");

    fs::create_dir_all(tar_dir).map_err(|err| E::CreateArchiveDir(err, tar_dir.to_path_buf()))?;

    // Create the staging directories
    let staging_dir_prefix = paths::TMP_SNAPSHOT_ARCHIVE_PREFIX;
    let staging_dir = tempfile::Builder::new()
        .prefix(&format!("{staging_dir_prefix}{snapshot_slot}-"))
        .tempdir_in(tar_dir)
        .map_err(|err| E::CreateStagingDir(err, tar_dir.to_path_buf()))?;
    let staging_snapshots_dir = staging_dir.path().join(paths::BANK_SNAPSHOTS_DIR);

    let slot_str = snapshot_slot.to_string();
    let staging_snapshot_dir = staging_snapshots_dir.join(&slot_str);
    // Creates staging snapshots/<slot>/
    fs::create_dir_all(&staging_snapshot_dir)
        .map_err(|err| E::CreateSnapshotStagingDir(err, staging_snapshot_dir.clone()))?;

    // To be a source for symlinking and archiving, the path need to be an absolute path
    let src_snapshot_dir = bank_snapshot_dir.as_ref().canonicalize().map_err(|err| {
        E::CanonicalizeSnapshotSourceDir(err, bank_snapshot_dir.as_ref().to_path_buf())
    })?;
    let staging_snapshot_file = staging_snapshot_dir.join(&slot_str);
    let src_snapshot_file = src_snapshot_dir.join(slot_str);
    symlink::symlink_file(&src_snapshot_file, &staging_snapshot_file)
        .map_err(|err| E::SymlinkSnapshot(err, src_snapshot_file, staging_snapshot_file))?;

    // Following the existing archive format, the status cache is under snapshots/, not under <slot>/
    // like in the snapshot dir.
    let staging_status_cache = staging_snapshots_dir.join(paths::SNAPSHOT_STATUS_CACHE_FILENAME);
    let src_status_cache = src_snapshot_dir.join(paths::SNAPSHOT_STATUS_CACHE_FILENAME);
    symlink::symlink_file(&src_status_cache, &staging_status_cache)
        .map_err(|err| E::SymlinkStatusCache(err, src_status_cache, staging_status_cache))?;

    // The bank snapshot has the version file, so symlink it to the correct staging path
    let staging_version_file = staging_dir.path().join(paths::SNAPSHOT_VERSION_FILENAME);
    let src_version_file = src_snapshot_dir.join(paths::SNAPSHOT_VERSION_FILENAME);
    symlink::symlink_file(&src_version_file, &staging_version_file).map_err(|err| {
        E::SymlinkVersionFile(err, src_version_file, staging_version_file.clone())
    })?;

    // Tar the staging directory into the archive at `staging_archive_path`
    let staging_archive_path = tar_dir.join(format!(
        "{}{}.{}",
        staging_dir_prefix,
        snapshot_slot,
        archive_format.extension(),
    ));

    {
        let archive_writer = large_file_buf_writer(&staging_archive_path, io_setup)
            .map_err(|err| E::CreateArchiveFile(err, staging_archive_path.clone()))?;

        let do_archive_files = |encoder: &mut dyn Write| -> std::result::Result<(), E> {
            let mut archive = tar::Builder::new(encoder);
            // Disable sparse file handling.  This seems to be the root cause of an issue when
            // upgrading v2.0 to v2.1, and the tar crate from 0.4.41 to 0.4.42.
            // Since the tarball will still go through compression (zstd/etc) afterwards, disabling
            // sparse handling in the tar itself should be fine.
            //
            // Likely introduced in [^1].  Tracking resolution in [^2].
            // [^1] https://github.com/alexcrichton/tar-rs/pull/375
            // [^2] https://github.com/alexcrichton/tar-rs/issues/403
            archive.sparse(false);
            // Serialize the version and snapshots files before accounts so we can quickly determine the version
            // and other bank fields. This is necessary if we want to interleave unpacking with reconstruction
            archive
                .append_path_with_name(&staging_version_file, paths::SNAPSHOT_VERSION_FILENAME)
                .map_err(E::ArchiveVersionFile)?;
            archive
                .append_dir_all(paths::BANK_SNAPSHOTS_DIR, &staging_snapshots_dir)
                .map_err(E::ArchiveSnapshotsDir)?;

            let storages_orderer = AccountStoragesOrderer::with_small_to_large_ratio(
                snapshot_storages,
                INTERLEAVE_TAR_ENTRIES_SMALL_TO_LARGE_RATIO,
            );
            // For incremental snapshots, the storage files have just been
            // written and are likely still hot in the page cache, so prefer
            // page-cached reads over direct I/O.
            let use_page_cache =
                matches!(snapshot_archive_kind, SnapshotArchiveKind::Incremental(_));
            let use_direct_io = io_setup.use_direct_io && !use_page_cache;

            // Tombstones must always be included in the snapshot archive.
            // This is to handle the scenario where a long-running RPC scan_accounts()
            // causes snapshot handling to flush the write cache _without_ also cleaning,
            // which could allow an account to have duplicates: an older open version and a new closed (zero lamport) version.
            // If the newer closed (tombstone) version is filtered out, the older open
            // version would remain, and revive the now-zombie account.
            let tombstones_filter = TombstonesFilter::Include;

            // Walk storages and their (lazily-opened) file handles in chunks,
            // bounding how many archive-mode fds are simultaneously open.
            let mut storage_file_pairs = storages_orderer
                .iter()
                .zip(open_storage_files(storages_orderer.iter(), use_direct_io));
            let mut buf_reader =
                storage_file_buf_reader(ACCOUNT_STORAGE_MAX_BUFFER_SIZE, use_page_cache, io_setup)
                    .map_err(E::StorageFileBufReaderError)?;
            let mut chunk = Vec::with_capacity(STORAGE_FILE_OPEN_CHUNK_SIZE);
            loop {
                chunk.clear();
                for (storage, file) in (&mut storage_file_pairs).take(STORAGE_FILE_OPEN_CHUNK_SIZE)
                {
                    chunk.push((storage, file.map_err(E::StorageFileBufReaderError)?));
                }
                if chunk.is_empty() {
                    break;
                }
                // Cheaply re-bind the reader to scope file borrows to this chunk.
                let mut chunk_reader = buf_reader.rebind().map_err(E::StorageFileBufReaderError)?;

                // Queue the whole chunk for read-ahead before consuming any of
                // it, so the io_uring pipeline can saturate across files.
                for (storage, file) in &chunk {
                    chunk_reader
                        .add_file_to_prefetch(file.as_ref(), storage.accounts.len() as FileSize)
                        .map_err(E::StorageFileBufReaderError)?;
                }

                for (storage, file) in &chunk {
                    let path_in_archive = Path::new(ACCOUNTS_DIR)
                        .join(AccountsFile::file_name(storage.slot(), storage.id()));

                    chunk_reader
                        .set_file(file.as_ref(), storage.accounts.len() as FileSize)
                        .map_err(|err| {
                            E::AccountStorageReaderError(err, storage.path().to_path_buf())
                        })?;
                    let reader = AccountStorageReader::new(
                        storage,
                        Some(snapshot_slot),
                        tombstones_filter,
                        &mut chunk_reader,
                    )
                    .map_err(|err| {
                        E::AccountStorageReaderError(err, storage.path().to_path_buf())
                    })?;
                    let mut header = tar::Header::new_gnu();
                    header.set_path(path_in_archive).map_err(|err| {
                        E::ArchiveAccountStorageFile(err, storage.path().to_path_buf())
                    })?;
                    header.set_size(reader.len_for_archive() as u64);
                    header.set_cksum();
                    append_entry(archive.get_mut(), &header, |output| reader.write_to(output))
                        .map_err(|err| {
                            E::ArchiveAccountStorageFile(err, storage.path().to_path_buf())
                        })?;
                }

                buf_reader = chunk_reader
                    .rebind()
                    .map_err(E::StorageFileBufReaderError)?;
            }

            archive.into_inner().map_err(E::FinishArchive)?;
            Ok(())
        };

        match archive_format {
            ArchiveFormat::TarZstd { config } => {
                let mut encoder = MultiFrameZstdWriter::new(
                    archive_writer,
                    config.compression_level,
                    ZSTD_FRAME_SIZE,
                )
                .map_err(E::CreateEncoder)?;
                do_archive_files(&mut encoder)?;
                let mut writer = encoder.finish().map_err(E::FinishEncoder)?;
                writer.flush().map_err(E::FinishEncoder)?;
            }
            ArchiveFormat::TarLz4 => {
                let mut encoder = lz4::EncoderBuilder::new()
                    .level(1)
                    .build(archive_writer)
                    .map_err(E::CreateEncoder)?;
                do_archive_files(&mut encoder)?;
                let (mut writer, result) = encoder.finish();
                result.map_err(E::FinishEncoder)?;
                writer.flush().map_err(E::FinishEncoder)?;
            }
        };
    }

    // Atomically move the archive into position for other validators to find
    let metadata = fs::metadata(&staging_archive_path)
        .map_err(|err| E::QueryArchiveMetadata(err, staging_archive_path.clone()))?;
    let archive_path = archive_path.as_ref().to_path_buf();
    fs::rename(&staging_archive_path, &archive_path)
        .map_err(|err| E::MoveArchive(err, staging_archive_path, archive_path.clone()))?;

    timer.stop();
    info!(
        "Successfully created {}. slot: {}, elapsed ms: {}, size: {}",
        archive_path.display(),
        snapshot_slot,
        timer.as_ms(),
        metadata.len()
    );

    datapoint_info!(
        "archive-snapshot-package",
        ("slot", snapshot_slot, i64),
        ("archive_format", archive_format.to_string(), String),
        ("duration_ms", timer.as_ms(), i64),
        (
            if snapshot_archive_kind == SnapshotArchiveKind::Full {
                "full-snapshot-archive-size"
            } else {
                "incremental-snapshot-archive-size"
            },
            metadata.len(),
            i64
        ),
    );
    Ok(SnapshotArchiveInfo {
        path: archive_path,
        slot: snapshot_slot,
        hash: snapshot_hash,
        archive_format,
    })
}

/// Appends entry with `header` to `archive_writer` via `write_fn`.
///
/// This is similar to tar::Builder::append(), but without requiring
/// the entry's source to implement `Read`.
///
/// The header must have its path, size, and checksum set.
/// `write_fn` writes data directly into the archive through the `Write` fn param.
fn append_entry(
    mut archive_writer: impl Write,
    header: &tar::Header,
    write_fn: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    let size = header.size()?;
    archive_writer.write_all(header.as_bytes())?;
    let mut entry_writer = SizeLimitedWriter::new(archive_writer.by_ref(), size);
    write_fn(&mut entry_writer)?;
    if entry_writer.bytes_written() != size {
        // SizeLimitedWriter will return an error itself if the size limit is exceeded.
        // So if we get here, that means we didn't write enough.
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "archive entry is shorter than its declared size",
        ));
    }

    const TAR_BLOCK_SIZE: u64 = 512;
    let padding = size.next_multiple_of(TAR_BLOCK_SIZE).wrapping_sub(size);
    archive_writer.write_all(&[0; TAR_BLOCK_SIZE as usize][..padding as usize])
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::{cmp, io::Read as _},
    };

    fn header(size: u64) -> tar::Header {
        let mut header = tar::Header::new_gnu();
        header.set_path("accounts/1.0").unwrap();
        header.set_size(size);
        header.set_cksum();
        header
    }

    // Deliberately has no Seek implementation and accepts only partial writes.
    #[derive(Default)]
    struct ShortWriter(Vec<u8>);

    impl Write for ShortWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let n = cmp::min(bytes.len(), 17);
            self.0.extend_from_slice(&bytes[..n]);
            Ok(n)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_append_entry_good() {
        let mut expected = tar::Builder::new(Vec::new());
        let mut actual = tar::Builder::new(ShortWriter::default());
        let sizes = [0, 1, 2, 3, 4, 5, 6, 7, 511, 512, 513, 8192, 20 * 1024 + 7];
        for size in sizes {
            let entry = vec![42; size];
            let header = header(size as u64);
            expected.append(&header, entry.as_slice()).unwrap();
            append_entry(actual.get_mut(), &header, |output| {
                for chunk in entry.chunks(333) {
                    output.write_all(chunk)?;
                }
                Ok(())
            })
            .unwrap();
        }
        // ensure can still use regular `tar::Builder::append()` after our own `append_entry()`
        expected.append(&header(4), &b"tail"[..]).unwrap();
        actual.append(&header(4), &b"tail"[..]).unwrap();
        let actual = actual.into_inner().unwrap().0;
        assert_eq!(actual, expected.into_inner().unwrap());

        let mut archive = tar::Archive::new(actual.as_slice());
        let mut entries = archive.entries().unwrap();
        for size in sizes {
            let mut entry = Vec::new();
            entries
                .next()
                .unwrap()
                .unwrap()
                .read_to_end(&mut entry)
                .unwrap();
            assert_eq!(entry, vec![42; size]);
        }
        let mut tail = Vec::new();
        entries
            .next()
            .unwrap()
            .unwrap()
            .read_to_end(&mut tail)
            .unwrap();
        assert_eq!(tail, b"tail");
        assert!(entries.next().is_none());
    }

    #[test]
    fn test_append_entry_bad_entry_too_small() {
        let mut archive = tar::Builder::new(Vec::new());
        let value = b"abc";
        let size = value.len();
        let err = append_entry(archive.get_mut(), &header(size as u64 + 1), |output| {
            output.write_all(value)
        })
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(archive.get_ref().len(), 512 + 3);
    }

    #[test]
    fn test_append_entry_bad_entry_too_big() {
        let mut archive = tar::Builder::new(Vec::new());
        let value = b"abcde";
        let size = value.len();
        let err = append_entry(archive.get_mut(), &header(size as u64 - 1), |output| {
            output.write_all(&value[..size - 1])?;
            output.write_all(&value[size - 1..])
        })
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(archive.get_ref().len(), 512 + 4);
    }

    #[test]
    fn test_append_entry_bad_callback() {
        let mut archive = tar::Builder::new(Vec::new());
        let err = append_entry(archive.get_mut(), &header(4), |output| {
            output.write_all(b"ab")?;
            Err(io::Error::other("scan failed"))
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "scan failed");
        assert_eq!(archive.get_ref().len(), 512 + 2);
    }

    #[test]
    fn test_append_entry_bad_archive_writer() {
        // A slice writer returns WriteZero when its capacity is exhausted.
        // Exercise failures in the header, contents, and padding respectively.
        for capacity in [511, 514, 1023] {
            let mut bytes = vec![0; capacity];
            let mut archive = tar::Builder::new(bytes.as_mut_slice());
            let err = append_entry(archive.get_mut(), &header(4), |output| {
                output.write_all(b"abcd")
            })
            .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::WriteZero);
        }
    }
}
