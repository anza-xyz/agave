#![cfg(feature = "agave-unstable-api")]
pub use solana_file_download::DownloadProgressRecord;
use {
    agave_snapshots::{
        ArchiveFormat, SnapshotArchiveKind, ZstdConfig, paths as snapshot_paths,
        snapshot_hash::SnapshotHash,
    },
    log::*,
    solana_clock::Slot,
    solana_file_download::{DownloadProgressCallbackOption, download_file},
    solana_genesis_config::DEFAULT_GENESIS_ARCHIVE,
    solana_runtime::snapshot_utils,
    std::{
        fs,
        net::SocketAddr,
        num::NonZeroUsize,
        path::{Path, PathBuf},
    },
};

pub fn download_genesis_if_missing(
    snapshot_server_url: &str,
    genesis_package: &Path,
    use_progress_bar: bool,
) -> Result<PathBuf, String> {
    if !genesis_package.exists() {
        let tmp_genesis_path = genesis_package.parent().unwrap().join("tmp-genesis");
        let tmp_genesis_package = tmp_genesis_path.join(DEFAULT_GENESIS_ARCHIVE);

        let _ignored = fs::remove_dir_all(&tmp_genesis_path);
        download_file(
            &format!(
                "{}/{DEFAULT_GENESIS_ARCHIVE}",
                snapshot_server_url.trim_end_matches('/'),
            ),
            &tmp_genesis_package,
            use_progress_bar,
            &mut None,
        )?;

        Ok(tmp_genesis_package)
    } else {
        Err("genesis already exists".to_string())
    }
}

/// Download a snapshot archive from `rpc_addr`. Use `snapshot_kind` to specify downloading either
/// a full snapshot or an incremental snapshot.
pub fn download_snapshot_archive(
    rpc_addr: &SocketAddr,
    full_snapshot_archives_dir: &Path,
    incremental_snapshot_archives_dir: &Path,
    desired_snapshot_hash: (Slot, SnapshotHash),
    snapshot_kind: SnapshotArchiveKind,
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
        snapshot_paths::build_snapshot_archives_remote_dir(match snapshot_kind {
            SnapshotArchiveKind::Full => full_snapshot_archives_dir,
            SnapshotArchiveKind::Incremental(_) => incremental_snapshot_archives_dir,
        });
    fs::create_dir_all(&snapshot_archives_remote_dir).unwrap();

    for archive_format in [
        ArchiveFormat::TarZstd {
            config: ZstdConfig::default(),
        },
        ArchiveFormat::TarLz4,
    ] {
        let destination_path = match snapshot_kind {
            SnapshotArchiveKind::Full => snapshot_paths::build_full_snapshot_archive_path(
                &snapshot_archives_remote_dir,
                desired_snapshot_hash.0,
                &desired_snapshot_hash.1,
                archive_format,
            ),
            SnapshotArchiveKind::Incremental(base_slot) => {
                snapshot_paths::build_incremental_snapshot_archive_path(
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
                "http://{rpc_addr}/{}",
                destination_path.file_name().unwrap().to_str().unwrap()
            ),
            &destination_path,
            use_progress_bar,
            progress_notify_callback,
        ) {
            Ok(()) => return Ok(()),
            Err(err) => info!("{err}"),
        }
    }
    Err(format!(
        "Failed to download a snapshot archive for slot {} from {rpc_addr}",
        desired_snapshot_hash.0
    ))
}

#[allow(clippy::too_many_arguments)]
pub fn download_latest_snapshot_archive_from_snapshot_server(
    snapshot_server_url: &str,
    full_snapshot_archives_dir: &Path,
    incremental_snapshot_archives_dir: &Path,
    maximum_full_snapshot_archives_to_retain: NonZeroUsize,
    maximum_incremental_snapshot_archives_to_retain: NonZeroUsize,
    use_progress_bar: bool,
    progress_notify_callback: &mut DownloadProgressCallbackOption<'_>,
    snapshot_kind: SnapshotArchiveKind,
) -> Result<PathBuf, String> {
    snapshot_utils::purge_old_snapshot_archives(
        full_snapshot_archives_dir,
        incremental_snapshot_archives_dir,
        maximum_full_snapshot_archives_to_retain,
        maximum_incremental_snapshot_archives_to_retain,
    );

    let snapshot_url = format!(
        "{}/{}",
        snapshot_server_url.trim_end_matches('/'),
        match snapshot_kind {
            SnapshotArchiveKind::Full => "snapshot.tar.bz2",
            SnapshotArchiveKind::Incremental(_) => "incremental-snapshot.tar.bz2",
        }
    );
    let response = reqwest::blocking::Client::new()
        .get(&snapshot_url)
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|err| err.to_string())?;

    let trusted_filename = response
        .url()
        .path_segments()
        .and_then(|mut path_segments| path_segments.next_back())
        .filter(|filename| !filename.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("Unable to determine trusted filename for {snapshot_url}"))?;
    let trusted_url = response.url().to_string();
    drop(response);

    let snapshot_archives_remote_dir =
        snapshot_paths::build_snapshot_archives_remote_dir(match snapshot_kind {
            SnapshotArchiveKind::Full => {
                snapshot_paths::parse_full_snapshot_archive_filename(&trusted_filename)
                    .map_err(|err| format!("Invalid redirected full snapshot filename: {err}"))?;
                full_snapshot_archives_dir
            }
            SnapshotArchiveKind::Incremental(_) => {
                snapshot_paths::parse_incremental_snapshot_archive_filename(&trusted_filename)
                    .map_err(|err| {
                        format!("Invalid redirected incremental snapshot filename: {err}")
                    })?;
                incremental_snapshot_archives_dir
            }
        });
    fs::create_dir_all(&snapshot_archives_remote_dir).unwrap();
    let destination_path = snapshot_archives_remote_dir.join(trusted_filename);

    if destination_path.is_file() {
        return Ok(destination_path);
    }

    download_file(
        &trusted_url,
        &destination_path,
        use_progress_bar,
        progress_notify_callback,
    )?;

    Ok(destination_path)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::{
            io::{BufRead, BufReader, Write},
            net::TcpListener,
            thread::{self, JoinHandle},
        },
        tempfile::TempDir,
    };

    const HASH: &str = "AvFf9oS8A8U78HdjT9YG2sTTThLHJZmhaMn2g8vkWYnr";

    fn start_redirect_server(
        request_path: &'static str,
        redirected_filename: String,
        request_count: usize,
    ) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let snapshot_server_url = format!("http://{}", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    if header == "\r\n" || header.is_empty() {
                        break;
                    }
                }

                let path = request_line.split_whitespace().nth(1).unwrap();
                if path == request_path {
                    write!(
                        stream,
                        "HTTP/1.1 303 See Other\r\nLocation: \
                         /{redirected_filename}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                } else if path == format!("/{redirected_filename}") {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nsnapshot"
                    )
                    .unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                }
            }
        });

        (snapshot_server_url, handle)
    }

    #[test]
    fn test_trusted_snapshot_archive_filename() {
        for (request_path, filename, snapshot_kind, expected_remote_dir, expected_success) in [
            (
                "/snapshot.tar.bz2",
                format!("snapshot-100-{HASH}.tar.zst"),
                SnapshotArchiveKind::Full,
                SnapshotArchiveKind::Full,
                true,
            ),
            (
                "/incremental-snapshot.tar.bz2",
                format!("incremental-snapshot-100-200-{HASH}.tar.lz4"),
                SnapshotArchiveKind::Incremental(100),
                SnapshotArchiveKind::Incremental(100),
                true,
            ),
            (
                "/snapshot.tar.bz2",
                "not-a-snapshot.txt".to_string(),
                SnapshotArchiveKind::Full,
                SnapshotArchiveKind::Full,
                false,
            ),
        ] {
            let full_snapshot_archives_dir = TempDir::new().unwrap();
            let incremental_snapshot_archives_dir = TempDir::new().unwrap();
            let (snapshot_server_url, server) = start_redirect_server(
                request_path,
                filename.clone(),
                expected_success as usize + 2,
            );

            let result = download_latest_snapshot_archive_from_snapshot_server(
                &snapshot_server_url,
                full_snapshot_archives_dir.path(),
                incremental_snapshot_archives_dir.path(),
                NonZeroUsize::new(usize::MAX).unwrap(),
                NonZeroUsize::new(usize::MAX).unwrap(),
                false,
                &mut None,
                snapshot_kind,
            );
            server.join().unwrap();

            if expected_success {
                let expected_dir = match expected_remote_dir {
                    SnapshotArchiveKind::Full => full_snapshot_archives_dir.path(),
                    SnapshotArchiveKind::Incremental(_) => incremental_snapshot_archives_dir.path(),
                };
                assert_eq!(
                    result.unwrap(),
                    snapshot_paths::build_snapshot_archives_remote_dir(expected_dir).join(filename)
                );
            } else {
                assert!(result.is_err());
            }
        }
    }
}
