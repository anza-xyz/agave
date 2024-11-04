#![allow(clippy::arithmetic_side_effects)]
use {
    log::*,
    solana_file_downloader::{download_file, DownloadProgressCallbackOption},
    solana_runtime::{
        snapshot_hash::SnapshotHash,
        snapshot_package::SnapshotKind,
        snapshot_utils::{self, ArchiveFormat},
    },
    solana_sdk::{clock::Slot, genesis_config::DEFAULT_GENESIS_ARCHIVE},
    std::{
        fs,
        net::SocketAddr,
        num::NonZeroUsize,
        path::{Path, PathBuf},
        time::Duration,
    },
};

/// Structure modeling information about download progress
#[derive(Debug)]
pub struct DownloadProgressRecord {
    // Duration since the beginning of the download
    pub elapsed_time: Duration,
    // Duration since the the last notification
    pub last_elapsed_time: Duration,
    // the bytes/sec speed measured for the last notification period
    pub last_throughput: f32,
    // the bytes/sec speed measured from the beginning
    pub total_throughput: f32,
    // total bytes of the download
    pub total_bytes: usize,
    // bytes downloaded so far
    pub current_bytes: usize,
    // percentage downloaded
    pub percentage_done: f32,
    // Estimated remaining time (in seconds) to finish the download if it keeps at the the last download speed
    pub estimated_remaining_time: f32,
    // The times of the progress is being notified, it starts from 1 and increments by 1 each time
    pub notification_count: u64,
}

pub fn download_genesis_if_missing(
    rpc_addr: &SocketAddr,
    genesis_package: &Path,
    use_progress_bar: bool,
) -> Result<PathBuf, String> {
    if !genesis_package.exists() {
        let tmp_genesis_path = genesis_package.parent().unwrap().join("tmp-genesis");
        let tmp_genesis_package = tmp_genesis_path.join(DEFAULT_GENESIS_ARCHIVE);

        let _ignored = fs::remove_dir_all(&tmp_genesis_path);
        download_file(
            &format!("http://{rpc_addr}/{DEFAULT_GENESIS_ARCHIVE}"),
            &tmp_genesis_package,
            use_progress_bar,
            &mut None,
        )?;

        Ok(tmp_genesis_package)
    } else {
        Err("genesis already exists".to_string())
    }
}

/// Download a snapshot archive from `rpc_addr`.  Use `snapshot_kind` to specify downloading either
/// a full snapshot or an incremental snapshot.
pub fn download_snapshot_archive(
    rpc_addr: &SocketAddr,
    full_snapshot_archives_dir: &Path,
    incremental_snapshot_archives_dir: &Path,
    desired_snapshot_hash: (Slot, SnapshotHash),
    snapshot_kind: SnapshotKind,
    maximum_full_snapshot_archives_to_retain: NonZeroUsize,
    maximum_incremental_snapshot_archives_to_retain: NonZeroUsize,
    use_progress_bar: bool,
    progress_notify_callback: &mut DownloadProgressCallbackOption<'_>,
) -> Result<(), String> {
    snapshot_utils::purge_old_snapshot_archives(
        full_snapshot_archives_dir,
        incremental_snapshot_archives_dir,
        maximum_full_snapshot_archives_to_retain,
        maximum_incremental_snapshot_archives_to_retain,
    );

    let snapshot_archives_remote_dir =
        snapshot_utils::build_snapshot_archives_remote_dir(match snapshot_kind {
            SnapshotKind::FullSnapshot => full_snapshot_archives_dir,
            SnapshotKind::IncrementalSnapshot(_) => incremental_snapshot_archives_dir,
        });
    fs::create_dir_all(&snapshot_archives_remote_dir).unwrap();

    for archive_format in [
        ArchiveFormat::TarZstd,
        ArchiveFormat::TarGzip,
        ArchiveFormat::TarBzip2,
        ArchiveFormat::TarLz4,
        ArchiveFormat::Tar,
    ] {
        let destination_path = match snapshot_kind {
            SnapshotKind::FullSnapshot => snapshot_utils::build_full_snapshot_archive_path(
                &snapshot_archives_remote_dir,
                desired_snapshot_hash.0,
                &desired_snapshot_hash.1,
                archive_format,
            ),
            SnapshotKind::IncrementalSnapshot(base_slot) => {
                snapshot_utils::build_incremental_snapshot_archive_path(
                    &snapshot_archives_remote_dir,
                    base_slot,
                    desired_snapshot_hash.0,
                    &desired_snapshot_hash.1,
                    archive_format,
                )
            }
        };

        if destination_path.is_file() {
            return Ok(());
        }

        match download_file(
            &format!(
                "http://{}/{}",
                rpc_addr,
                destination_path.file_name().unwrap().to_str().unwrap()
            ),
            &destination_path,
            use_progress_bar,
            progress_notify_callback,
        ) {
            Ok(()) => return Ok(()),
            Err(err) => info!("{}", err),
        }
    }
    Err(format!(
        "Failed to download a snapshot archive for slot {} from {}",
        desired_snapshot_hash.0, rpc_addr
    ))
}
