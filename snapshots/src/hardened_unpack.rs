use {
    agave_fs::file_io::{self, FileCreator},
    log::*,
    rand::{rng, Rng},
    solana_genesis_config::DEFAULT_GENESIS_FILE,
    std::{
        fs::{self, File},
        io::{self, Read},
        path::{
            Component::{self, CurDir, Normal},
            Path, PathBuf,
        },
        sync::Arc,
    },
    tar::{
        Archive,
        EntryType::{Directory, GNUSparse, Regular},
        Unpacked,
    },
    thiserror::Error,
};

#[derive(Error, Debug)]
pub enum UnpackError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Archive error: {0}")]
    Archive(String),
    #[error("Unpacking '{1}' failed: {0}")]
    Unpack(Box<UnpackError>, PathBuf),
}

pub type Result<T> = std::result::Result<T, UnpackError>;

// 64 TiB; some safe margin to the max 128 TiB in amd64 linux userspace VmSize
// (ref: https://unix.stackexchange.com/a/386555/364236)
// note that this is directly related to the mmapped data size
// so protect against insane value
// This is the file size including holes for sparse files
const MAX_SNAPSHOT_ARCHIVE_UNPACKED_APPARENT_SIZE: u64 = 64 * 1024 * 1024 * 1024 * 1024;

// 4 TiB;
// This is the actually consumed disk usage for sparse files
const MAX_SNAPSHOT_ARCHIVE_UNPACKED_ACTUAL_SIZE: u64 = 4 * 1024 * 1024 * 1024 * 1024;

const MAX_SNAPSHOT_ARCHIVE_UNPACKED_COUNT: u64 = 5_000_000;
const MAX_GENESIS_ARCHIVE_UNPACKED_COUNT: u64 = 100;

fn checked_total_size_sum(total_size: u64, entry_size: u64, limit_size: u64) -> Result<u64> {
    trace!("checked_total_size_sum: {total_size} + {entry_size} < {limit_size}");
    let total_size = total_size.saturating_add(entry_size);
    if total_size > limit_size {
        return Err(UnpackError::Archive(format!(
            "too large archive: {total_size} than limit: {limit_size}",
        )));
    }
    Ok(total_size)
}

#[allow(clippy::arithmetic_side_effects)]
fn checked_total_count_increment(total_count: u64, limit_count: u64) -> Result<u64> {
    let total_count = total_count + 1;
    if total_count > limit_count {
        return Err(UnpackError::Archive(format!(
            "too many files in snapshot: {total_count:?}"
        )));
    }
    Ok(total_count)
}

fn check_unpack_result(unpack_result: Result<()>, path: String) -> Result<()> {
    if let Err(err) = unpack_result {
        return Err(UnpackError::Archive(format!(
            "failed to unpack {path:?}: {err}"
        )));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum UnpackPath<'a> {
    Valid(&'a Path),
    Ignore,
    Invalid,
}

#[allow(clippy::arithmetic_side_effects)]
fn unpack_archive<'a, C>(
    input: impl Read,
    mut file_creator: Box<dyn FileCreator>,
    apparent_limit_size: u64,
    actual_limit_size: u64,
    limit_count: u64,
    mut entry_checker: C, // checks if entry is valid
) -> Result<()>
where
    C: FnMut(&[&str], tar::EntryType) -> UnpackPath<'a>,
{
    let mut apparent_total_size: u64 = 0;
    let mut actual_total_size: u64 = 0;
    let mut total_count: u64 = 0;

    let mut total_entries = 0;
    let mut open_dirs = Vec::new();

    let mut archive = Archive::new(input);
    for entry in archive.entries()? {
        let entry = entry?;
        let path = entry.path()?;
        let path_str = path.display().to_string();

        // Although the `tar` crate safely skips at the actual unpacking, fail
        // first by ourselves when there are odd paths like including `..` or /
        // for our clearer pattern matching reasoning:
        //   https://docs.rs/tar/0.4.26/src/tar/entry.rs.html#371
        let parts = path
            .components()
            .map(|p| match p {
                CurDir => Ok("."),
                Normal(c) => c.to_str().ok_or(()),
                _ => Err(()), // Prefix (for Windows) and RootDir are forbidden
            })
            .collect::<std::result::Result<Vec<_>, _>>();

        // Reject old-style BSD directory entries that aren't explicitly tagged as directories
        let legacy_dir_entry =
            entry.header().as_ustar().is_none() && entry.path_bytes().ends_with(b"/");
        let kind = entry.header().entry_type();
        let reject_legacy_dir_entry = legacy_dir_entry && (kind != Directory);
        let (Ok(parts), false) = (parts, reject_legacy_dir_entry) else {
            return Err(UnpackError::Archive(format!(
                "invalid path found: {path_str:?}"
            )));
        };

        let unpack_dir = match entry_checker(parts.as_slice(), kind) {
            UnpackPath::Invalid => {
                return Err(UnpackError::Archive(format!(
                    "extra entry found: {:?} {:?}",
                    path_str,
                    entry.header().entry_type(),
                )));
            }
            UnpackPath::Ignore => {
                continue;
            }
            UnpackPath::Valid(unpack_dir) => unpack_dir,
        };

        apparent_total_size = checked_total_size_sum(
            apparent_total_size,
            entry.header().size()?,
            apparent_limit_size,
        )?;
        actual_total_size = checked_total_size_sum(
            actual_total_size,
            entry.header().entry_size()?,
            actual_limit_size,
        )?;
        total_count = checked_total_count_increment(total_count, limit_count)?;

        let account_filename = match parts.as_slice() {
            ["accounts", account_filename] => Some(PathBuf::from(account_filename)),
            _ => None,
        };
        let entry_path = if let Some(account) = account_filename {
            // Special case account files. We're unpacking an account entry inside one of the
            // account_paths returned by `entry_checker`. We want to unpack into
            // account_path/<account> instead of account_path/accounts/<account> so we strip the
            // accounts/ prefix.
            sanitize_path_and_open_dir(&account, unpack_dir, &mut open_dirs)
        } else {
            sanitize_path_and_open_dir(&path, unpack_dir, &mut open_dirs)
        }?; // ? handles file system errors
        let Some((entry_path, open_dir)) = entry_path else {
            continue; // skip it
        };

        let unpack = unpack_entry(&mut file_creator, entry, entry_path, open_dir);
        check_unpack_result(unpack, path_str)?;

        total_entries += 1;
    }
    file_creator.drain()?;

    info!("unpacked {total_entries} entries total");
    Ok(())
}

fn unpack_entry<'a, R: Read>(
    files_creator: &mut Box<dyn FileCreator + 'a>,
    mut entry: tar::Entry<'_, R>,
    dst: PathBuf,
    dst_open_dir: Arc<File>,
) -> Result<()> {
    let mode = match entry.header().entry_type() {
        GNUSparse | Regular => 0o644,
        _ => 0o755,
    };
    if should_fallback_to_tar_unpack(&entry) {
        let unpacked = entry.unpack(&dst)?;

        // Sanitize permissions.
        file_io::set_path_permissions(&dst, mode)?;

        if let Unpacked::File(unpacked_file) = unpacked {
            // Process file after setting permissions
            let size = entry.header().size()?;
            files_creator.file_complete(unpacked_file, dst, size);
        }
        return Ok(());
    }
    files_creator.schedule_create_at_dir(dst, mode, dst_open_dir, &mut entry)?;

    Ok(())
}

fn should_fallback_to_tar_unpack<R: io::Read>(entry: &tar::Entry<'_, R>) -> bool {
    // Follows cases that are handled as directory or in special way by tar-rs library,
    // we want to handle just cases where the library would write plain files with entry's content.
    matches!(
        entry.header().entry_type(),
        tar::EntryType::Directory
            | tar::EntryType::Link
            | tar::EntryType::Symlink
            | tar::EntryType::XGlobalHeader
            | tar::EntryType::XHeader
            | tar::EntryType::GNULongName
            | tar::EntryType::GNULongLink
    ) || entry.header().as_ustar().is_none() && entry.path_bytes().ends_with(b"/")
}

// return Err on file system error
// return Some((path, open_dir)) if path is good
// return None if we should skip this file
fn sanitize_path_and_open_dir(
    entry_path: &Path,
    dst: &Path,
    open_dirs: &mut Vec<(PathBuf, Arc<File>)>,
) -> Result<Option<(PathBuf, Arc<File>)>> {
    // We cannot call unpack_in because it errors if we try to use 2 account paths.
    // So, this code is borrowed from unpack_in
    // ref: https://docs.rs/tar/*/tar/struct.Entry.html#method.unpack_in
    let mut file_dst = dst.to_path_buf();
    const SKIP: Result<Option<(PathBuf, Arc<File>)>> = Ok(None);
    {
        let path = entry_path;
        for part in path.components() {
            match part {
                // Leading '/' characters, root paths, and '.'
                // components are just ignored and treated as "empty
                // components"
                Component::Prefix(..) | Component::RootDir | Component::CurDir => continue,

                // If any part of the filename is '..', then skip over
                // unpacking the file to prevent directory traversal
                // security issues.  See, e.g.: CVE-2001-1267,
                // CVE-2002-0399, CVE-2005-1918, CVE-2007-4131
                Component::ParentDir => return SKIP,

                Component::Normal(part) => file_dst.push(part),
            }
        }
    }

    // Skip cases where only slashes or '.' parts were seen, because
    // this is effectively an empty filename.
    if *dst == *file_dst {
        return SKIP;
    }

    // Skip entries without a parent (i.e. outside of FS root)
    let Some(parent) = file_dst.parent() else {
        return SKIP;
    };

    let open_dst_dir = match open_dirs.binary_search_by(|(key, _)| parent.cmp(key)) {
        Err(insert_at) => {
            fs::create_dir_all(parent)?;

            // Here we are different than untar_in. The code for tar::unpack_in internally calling unpack is a little different.
            // ignore return value here
            validate_inside_dst(dst, parent)?;

            let opened_dir = Arc::new(File::open(parent)?);
            open_dirs.insert(insert_at, (parent.to_path_buf(), opened_dir.clone()));
            opened_dir
        }
        Ok(index) => open_dirs[index].1.clone(),
    };

    Ok(Some((file_dst, open_dst_dir)))
}

// copied from:
// https://github.com/alexcrichton/tar-rs/blob/d90a02f582c03dfa0fd11c78d608d0974625ae5d/src/entry.rs#L781
fn validate_inside_dst(dst: &Path, file_dst: &Path) -> Result<PathBuf> {
    // Abort if target (canonical) parent is outside of `dst`
    let canon_parent = file_dst.canonicalize().map_err(|err| {
        UnpackError::Archive(format!("{err} while canonicalizing {}", file_dst.display()))
    })?;
    let canon_target = dst.canonicalize().map_err(|err| {
        UnpackError::Archive(format!("{err} while canonicalizing {}", dst.display()))
    })?;
    if !canon_parent.starts_with(&canon_target) {
        return Err(UnpackError::Archive(format!(
            "trying to unpack outside of destination path: {}",
            canon_target.display()
        )));
    }
    Ok(canon_target)
}

/// Unpacks snapshot from (potentially partial) `archive` and
/// sends entry file paths through the `sender` channel
pub(super) fn streaming_unpack_snapshot(
    input: impl Read,
    file_creator: Box<dyn FileCreator>,
    ledger_dir: &Path,
    account_paths: &[PathBuf],
) -> Result<()> {
    unpack_snapshot_with_processors(input, file_creator, ledger_dir, account_paths, |_, _| {})
}

fn unpack_snapshot_with_processors<F>(
    input: impl Read,
    file_creator: Box<dyn FileCreator>,
    ledger_dir: &Path,
    account_paths: &[PathBuf],
    mut accounts_path_processor: F,
) -> Result<()>
where
    F: FnMut(&str, &Path),
{
    assert!(!account_paths.is_empty());

    unpack_archive(
        input,
        file_creator,
        MAX_SNAPSHOT_ARCHIVE_UNPACKED_APPARENT_SIZE,
        MAX_SNAPSHOT_ARCHIVE_UNPACKED_ACTUAL_SIZE,
        MAX_SNAPSHOT_ARCHIVE_UNPACKED_COUNT,
        |parts, kind| {
            if is_valid_snapshot_archive_entry(parts, kind) {
                if let ["accounts", file] = parts {
                    // Randomly distribute the accounts files about the available `account_paths`,
                    let path_index = rng().random_range(0..account_paths.len());
                    match account_paths
                        .get(path_index)
                        .map(|path_buf| path_buf.as_path())
                    {
                        Some(path) => {
                            accounts_path_processor(file, path);
                            UnpackPath::Valid(path)
                        }
                        None => UnpackPath::Invalid,
                    }
                } else {
                    UnpackPath::Valid(ledger_dir)
                }
            } else {
                UnpackPath::Invalid
            }
        },
    )
}

fn all_digits(v: &str) -> bool {
    if v.is_empty() {
        return false;
    }
    for x in v.chars() {
        if !x.is_ascii_digit() {
            return false;
        }
    }
    true
}

#[allow(clippy::arithmetic_side_effects)]
fn like_storage(v: &str) -> bool {
    let mut periods = 0;
    let mut saw_numbers = false;
    for x in v.chars() {
        if !x.is_ascii_digit() {
            if x == '.' {
                if periods > 0 || !saw_numbers {
                    return false;
                }
                saw_numbers = false;
                periods += 1;
            } else {
                return false;
            }
        } else {
            saw_numbers = true;
        }
    }
    saw_numbers && periods == 1
}

fn is_valid_snapshot_archive_entry(parts: &[&str], kind: tar::EntryType) -> bool {
    match (parts, kind) {
        (["version"], Regular) => true,
        (["accounts"], Directory) => true,
        (["accounts", file], GNUSparse) if like_storage(file) => true,
        (["accounts", file], Regular) if like_storage(file) => true,
        (["snapshots"], Directory) => true,
        (["snapshots", "status_cache"], GNUSparse) => true,
        (["snapshots", "status_cache"], Regular) => true,
        (["snapshots", dir, file], GNUSparse) if all_digits(dir) && all_digits(file) => true,
        (["snapshots", dir, file], Regular) if all_digits(dir) && all_digits(file) => true,
        (["snapshots", dir], Directory) if all_digits(dir) => true,
        _ => false,
    }
}

pub(super) fn unpack_genesis(
    input: impl Read,
    file_creator: Box<dyn FileCreator>,
    unpack_dir: &Path,
    max_genesis_archive_unpacked_size: u64,
) -> Result<()> {
    unpack_archive(
        input,
        file_creator,
        max_genesis_archive_unpacked_size,
        max_genesis_archive_unpacked_size,
        MAX_GENESIS_ARCHIVE_UNPACKED_COUNT,
        |p, k| is_valid_genesis_archive_entry(unpack_dir, p, k),
    )
}

fn is_valid_genesis_archive_entry<'a>(
    unpack_dir: &'a Path,
    parts: &[&str],
    kind: tar::EntryType,
) -> UnpackPath<'a> {
    trace!("validating: {parts:?} {kind:?}");
    #[allow(clippy::match_like_matches_macro)]
    match (parts, kind) {
        ([DEFAULT_GENESIS_FILE], GNUSparse) => UnpackPath::Valid(unpack_dir),
        ([DEFAULT_GENESIS_FILE], Regular) => UnpackPath::Valid(unpack_dir),
        (["rocksdb"], Directory) => UnpackPath::Ignore,
        (["rocksdb", _], GNUSparse) => UnpackPath::Ignore,
        (["rocksdb", _], Regular) => UnpackPath::Ignore,
        (["rocksdb_fifo"], Directory) => UnpackPath::Ignore,
        (["rocksdb_fifo", _], GNUSparse) => UnpackPath::Ignore,
        (["rocksdb_fifo", _], Regular) => UnpackPath::Ignore,
        _ => UnpackPath::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_fs::{file_io::file_creator, io_setup::IoSetupState},
        assert_matches::assert_matches,
        std::io::BufReader,
        tar::{Builder, Header},
    };

    const MAX_GENESIS_SIZE_FOR_TESTS: u64 = 1024;

    #[test]
    fn test_archive_is_valid_entry() {
        assert!(is_valid_snapshot_archive_entry(
            &["snapshots"],
            tar::EntryType::Directory
        ));
        assert!(!is_valid_snapshot_archive_entry(
            &["snapshots", ""],
            tar::EntryType::Directory
        ));
        assert!(is_valid_snapshot_archive_entry(
            &["snapshots", "3"],
            tar::EntryType::Directory
        ));
        assert!(is_valid_snapshot_archive_entry(
            &["snapshots", "3", "3"],
            tar::EntryType::Regular
        ));
        assert!(is_valid_snapshot_archive_entry(
            &["version"],
            tar::EntryType::Regular
        ));
        assert!(is_valid_snapshot_archive_entry(
            &["accounts"],
            tar::EntryType::Directory
        ));
        assert!(!is_valid_snapshot_archive_entry(
            &["accounts", ""],
            tar::EntryType::Regular
        ));

        assert!(!is_valid_snapshot_archive_entry(
            &["snapshots"],
            tar::EntryType::Regular
        ));
        assert!(!is_valid_snapshot_archive_entry(
            &["snapshots", "x0"],
            tar::EntryType::Directory
        ));
        assert!(!is_valid_snapshot_archive_entry(
            &["snapshots", "0x"],
            tar::EntryType::Directory
        ));
        assert!(!is_valid_snapshot_archive_entry(
            &["snapshots", "①"],
            tar::EntryType::Directory
        ));
        assert!(!is_valid_snapshot_archive_entry(
            &["snapshots", "0", "aa"],
            tar::EntryType::Regular
        ));
        assert!(!is_valid_snapshot_archive_entry(
            &["aaaa"],
            tar::EntryType::Regular
        ));
    }

    #[test]
    fn test_valid_snapshot_accounts() {
        agave_logger::setup();
        assert!(is_valid_snapshot_archive_entry(
            &["accounts", "0.0"],
            tar::EntryType::Regular
        ));
        assert!(is_valid_snapshot_archive_entry(
            &["accounts", "01829.077"],
            tar::EntryType::Regular
        ));

        assert!(!is_valid_snapshot_archive_entry(
            &["accounts", "1.2.34"],
            tar::EntryType::Regular
        ));
        assert!(!is_valid_snapshot_archive_entry(
            &["accounts", "12."],
            tar::EntryType::Regular
        ));
        assert!(!is_valid_snapshot_archive_entry(
            &["accounts", ".12"],
            tar::EntryType::Regular
        ));
        assert!(!is_valid_snapshot_archive_entry(
            &["accounts", "0x0"],
            tar::EntryType::Regular
        ));
        assert!(!is_valid_snapshot_archive_entry(
            &["accounts", "abc"],
            tar::EntryType::Regular
        ));
        assert!(!is_valid_snapshot_archive_entry(
            &["accounts", "232323"],
            tar::EntryType::Regular
        ));
        assert!(!is_valid_snapshot_archive_entry(
            &["accounts", "৬.¾"],
            tar::EntryType::Regular
        ));
    }

    #[test]
    fn test_archive_is_valid_archive_entry() {
        let path = Path::new("");
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["genesis.bin"], tar::EntryType::Regular),
            UnpackPath::Valid(path)
        );
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["genesis.bin"], tar::EntryType::GNUSparse,),
            UnpackPath::Valid(path)
        );
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["rocksdb"], tar::EntryType::Directory),
            UnpackPath::Ignore
        );
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["rocksdb", "foo"], tar::EntryType::Regular),
            UnpackPath::Ignore
        );
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["rocksdb", "foo"], tar::EntryType::GNUSparse,),
            UnpackPath::Ignore
        );
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["rocksdb_fifo"], tar::EntryType::Directory),
            UnpackPath::Ignore
        );
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["rocksdb_fifo", "foo"], tar::EntryType::Regular),
            UnpackPath::Ignore
        );
        assert_eq!(
            is_valid_genesis_archive_entry(
                path,
                &["rocksdb_fifo", "foo"],
                tar::EntryType::GNUSparse,
            ),
            UnpackPath::Ignore
        );
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["aaaa"], tar::EntryType::Regular),
            UnpackPath::Invalid
        );
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["aaaa"], tar::EntryType::GNUSparse,),
            UnpackPath::Invalid
        );
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["rocksdb"], tar::EntryType::Regular),
            UnpackPath::Invalid
        );
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["rocksdb"], tar::EntryType::GNUSparse,),
            UnpackPath::Invalid
        );
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["rocksdb", "foo"], tar::EntryType::Directory,),
            UnpackPath::Invalid
        );
        assert_eq!(
            is_valid_genesis_archive_entry(
                path,
                &["rocksdb", "foo", "bar"],
                tar::EntryType::Directory,
            ),
            UnpackPath::Invalid
        );
        assert_eq!(
            is_valid_genesis_archive_entry(
                path,
                &["rocksdb", "foo", "bar"],
                tar::EntryType::Regular
            ),
            UnpackPath::Invalid
        );
        assert_eq!(
            is_valid_genesis_archive_entry(
                path,
                &["rocksdb", "foo", "bar"],
                tar::EntryType::GNUSparse
            ),
            UnpackPath::Invalid
        );
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["rocksdb_fifo"], tar::EntryType::Regular),
            UnpackPath::Invalid
        );
        assert_eq!(
            is_valid_genesis_archive_entry(path, &["rocksdb_fifo"], tar::EntryType::GNUSparse,),
            UnpackPath::Invalid
        );
        assert_eq!(
            is_valid_genesis_archive_entry(
                path,
                &["rocksdb_fifo", "foo"],
                tar::EntryType::Directory,
            ),
            UnpackPath::Invalid
        );
        assert_eq!(
            is_valid_genesis_archive_entry(
                path,
                &["rocksdb_fifo", "foo", "bar"],
                tar::EntryType::Directory,
            ),
            UnpackPath::Invalid
        );
        assert_eq!(
            is_valid_genesis_archive_entry(
                path,
                &["rocksdb_fifo", "foo", "bar"],
                tar::EntryType::Regular
            ),
            UnpackPath::Invalid
        );
        assert_eq!(
            is_valid_genesis_archive_entry(
                path,
                &["rocksdb_fifo", "foo", "bar"],
                tar::EntryType::GNUSparse
            ),
            UnpackPath::Invalid
        );
    }

    fn with_finalize_and_unpack<C>(archive: tar::Builder<Vec<u8>>, checker: C) -> Result<()>
    where
        C: FnOnce(&[u8], &Path) -> Result<()>,
    {
        let data = archive.into_inner().unwrap();
        let temp_dir = tempfile::TempDir::new().unwrap();

        checker(data.as_slice(), temp_dir.path())?;
        // Check that there is no bad permissions preventing deletion.
        let result = temp_dir.close();
        assert_matches!(result, Ok(()));
        Ok(())
    }

    fn finalize_and_unpack_snapshot(archive: tar::Builder<Vec<u8>>) -> Result<()> {
        let file_creator = file_creator(256, &IoSetupState::default(), |file_info| {
            Some(file_info.file)
        })?;
        with_finalize_and_unpack(archive, move |a, b| {
            unpack_snapshot_with_processors(a, file_creator, b, &[PathBuf::new()], |_, _| {})
                .map(|_| ())
        })
    }

    fn finalize_and_unpack_genesis(archive: tar::Builder<Vec<u8>>) -> Result<()> {
        let file_creator = file_creator(0, &IoSetupState::default(), |file_info| {
            Some(file_info.file)
        })?;
        with_finalize_and_unpack(archive, move |a, b| {
            unpack_genesis(a, file_creator, b, MAX_GENESIS_SIZE_FOR_TESTS)
        })
    }

    #[test]
    fn test_archive_unpack_snapshot_ok() {
        let mut header = Header::new_gnu();
        header.set_path("version").unwrap();
        header.set_size(4);
        header.set_cksum();

        let data: &[u8] = &[1, 2, 3, 4];

        let mut archive = Builder::new(Vec::new());
        archive.append(&header, data).unwrap();

        let result = finalize_and_unpack_snapshot(archive);
        assert_matches!(result, Ok(()));
    }

    #[test]
    fn test_archive_unpack_genesis_ok() {
        let mut header = Header::new_gnu();
        header.set_path("genesis.bin").unwrap();
        header.set_size(4);
        header.set_cksum();

        let data: &[u8] = &[1, 2, 3, 4];

        let mut archive = Builder::new(Vec::new());
        archive.append(&header, data).unwrap();

        let result = finalize_and_unpack_genesis(archive);
        assert_matches!(result, Ok(()));
    }

    #[test]
    fn test_archive_unpack_genesis_size_limit() {
        let data: Vec<u8> = (0..MAX_GENESIS_SIZE_FOR_TESTS + 1)
            .map(|x| x as u8)
            .collect();

        let mut header = Header::new_gnu();
        header.set_path("genesis.bin").unwrap();
        header.set_entry_type(tar::EntryType::Regular);

        {
            let mut archive = Builder::new(Vec::new());
            header.set_size(data.len() as u64);
            header.set_cksum();
            archive.append(&header, data.as_slice()).unwrap();
            finalize_and_unpack_genesis(archive).expect_err(&format!(
                "too large archive: {} than limit: {MAX_GENESIS_SIZE_FOR_TESTS}",
                data.len()
            ));
        }

        {
            let data = &data[..MAX_GENESIS_SIZE_FOR_TESTS as usize];
            let mut archive = Builder::new(Vec::new());
            header.set_size(data.len() as u64);
            header.set_cksum();
            archive.append(&header, data).unwrap();
            finalize_and_unpack_genesis(archive).expect("should unpack max size genesis");
        }
    }

    #[test]
    fn test_archive_unpack_genesis_bad_perms() {
        let mut archive = Builder::new(Vec::new());

        let mut header = Header::new_gnu();
        header.set_path("rocksdb").unwrap();
        header.set_entry_type(Directory);
        header.set_size(0);
        header.set_cksum();
        let data: &[u8] = &[];
        archive.append(&header, data).unwrap();

        let mut header = Header::new_gnu();
        header.set_path("rocksdb/test").unwrap();
        header.set_size(4);
        header.set_cksum();
        let data: &[u8] = &[1, 2, 3, 4];
        archive.append(&header, data).unwrap();

        // Removing all permissions makes it harder to delete this directory
        // or work with files inside it.
        let mut header = Header::new_gnu();
        header.set_path("rocksdb").unwrap();
        header.set_entry_type(Directory);
        header.set_mode(0o000);
        header.set_size(0);
        header.set_cksum();
        let data: &[u8] = &[];
        archive.append(&header, data).unwrap();

        let result = finalize_and_unpack_genesis(archive);
        assert_matches!(result, Ok(()));
    }

    #[test]
    fn test_archive_unpack_genesis_bad_rocksdb_subdir() {
        let mut archive = Builder::new(Vec::new());

        let mut header = Header::new_gnu();
        header.set_path("rocksdb").unwrap();
        header.set_entry_type(Directory);
        header.set_size(0);
        header.set_cksum();
        let data: &[u8] = &[];
        archive.append(&header, data).unwrap();

        // tar-rs treats following entry as a Directory to support old tar formats.
        let mut header = Header::new_gnu();
        header.set_path("rocksdb/test/").unwrap();
        header.set_entry_type(Regular);
        header.set_size(0);
        header.set_cksum();
        let data: &[u8] = &[];
        archive.append(&header, data).unwrap();

        let result = finalize_and_unpack_genesis(archive);
        assert_matches!(result, Err(UnpackError::Archive(ref message)) if message == "invalid path found: \"rocksdb/test/\"");
    }

    #[test]
    fn test_archive_unpack_snapshot_invalid_path() {
        let mut header = Header::new_gnu();
        // bypass the sanitization of the .set_path()
        for (p, c) in header
            .as_old_mut()
            .name
            .iter_mut()
            .zip(b"foo/../../../dangerous".iter().chain(Some(&0)))
        {
            *p = *c;
        }
        header.set_size(4);
        header.set_cksum();

        let data: &[u8] = &[1, 2, 3, 4];

        let mut archive = Builder::new(Vec::new());
        archive.append(&header, data).unwrap();
        let result = finalize_and_unpack_snapshot(archive);
        assert_matches!(result, Err(UnpackError::Archive(ref message)) if message == "invalid path found: \"foo/../../../dangerous\"");
    }

    fn with_archive_unpack_snapshot_invalid_path(path: &str) -> Result<()> {
        let mut header = Header::new_gnu();
        // bypass the sanitization of the .set_path()
        for (p, c) in header
            .as_old_mut()
            .name
            .iter_mut()
            .zip(path.as_bytes().iter().chain(Some(&0)))
        {
            *p = *c;
        }
        header.set_size(4);
        header.set_cksum();

        let data: &[u8] = &[1, 2, 3, 4];

        let mut archive = Builder::new(Vec::new());
        archive.append(&header, data).unwrap();
        with_finalize_and_unpack(archive, |data, path| {
            let mut unpacking_archive = Archive::new(BufReader::new(data));
            for entry in unpacking_archive.entries()? {
                if !entry?.unpack_in(path)? {
                    return Err(UnpackError::Archive("failed!".to_string()));
                } else if !path.join(path).exists() {
                    return Err(UnpackError::Archive("not existing!".to_string()));
                }
            }
            Ok(())
        })
    }

    #[test]
    fn test_archive_unpack_itself() {
        assert_matches!(
            with_archive_unpack_snapshot_invalid_path("ryoqun/work"),
            Ok(())
        );
        // Absolute paths are neutralized as relative
        assert_matches!(
            with_archive_unpack_snapshot_invalid_path("/etc/passwd"),
            Ok(())
        );
        assert_matches!(with_archive_unpack_snapshot_invalid_path("../../../dangerous"), Err(UnpackError::Archive(ref message)) if message == "failed!");
    }

    #[test]
    fn test_archive_unpack_snapshot_invalid_entry() {
        let mut header = Header::new_gnu();
        header.set_path("foo").unwrap();
        header.set_size(4);
        header.set_cksum();

        let data: &[u8] = &[1, 2, 3, 4];

        let mut archive = Builder::new(Vec::new());
        archive.append(&header, data).unwrap();
        let result = finalize_and_unpack_snapshot(archive);
        assert_matches!(result, Err(UnpackError::Archive(ref message)) if message == "extra entry found: \"foo\" Regular");
    }

    #[test]
    fn test_archive_unpack_snapshot_too_large() {
        let mut header = Header::new_gnu();
        header.set_path("version").unwrap();
        header.set_size(1024 * 1024 * 1024 * 1024 * 1024);
        header.set_cksum();

        let data: &[u8] = &[1, 2, 3, 4];

        let mut archive = Builder::new(Vec::new());
        archive.append(&header, data).unwrap();
        let result = finalize_and_unpack_snapshot(archive);
        assert_matches!(
            result,
            Err(UnpackError::Archive(ref message))
                if message == &format!(
                    "too large archive: 1125899906842624 than limit: {MAX_SNAPSHOT_ARCHIVE_UNPACKED_APPARENT_SIZE}"
                )
        );
    }

    #[test]
    fn test_archive_unpack_snapshot_bad_unpack() {
        let result = check_unpack_result(
            Err(UnpackError::Io(io::ErrorKind::FileTooLarge.into())),
            "abc".to_string(),
        );
        assert_matches!(result, Err(UnpackError::Archive(ref message)) if message == "failed to unpack \"abc\": IO error: file too large");
    }

    #[test]
    fn test_archive_checked_total_size_sum() {
        let result = checked_total_size_sum(500, 500, MAX_SNAPSHOT_ARCHIVE_UNPACKED_ACTUAL_SIZE);
        assert_matches!(result, Ok(1000));

        let result =
            checked_total_size_sum(u64::MAX - 2, 2, MAX_SNAPSHOT_ARCHIVE_UNPACKED_ACTUAL_SIZE);
        assert_matches!(
            result,
            Err(UnpackError::Archive(ref message))
                if message == &format!(
                    "too large archive: 18446744073709551615 than limit: {MAX_SNAPSHOT_ARCHIVE_UNPACKED_ACTUAL_SIZE}"
                )
        );
    }

    #[test]
    fn test_archive_checked_total_size_count() {
        let result = checked_total_count_increment(101, MAX_SNAPSHOT_ARCHIVE_UNPACKED_COUNT);
        assert_matches!(result, Ok(102));

        let result =
            checked_total_count_increment(999_999_999_999, MAX_SNAPSHOT_ARCHIVE_UNPACKED_COUNT);
        assert_matches!(
            result,
            Err(UnpackError::Archive(ref message))
                if message == "too many files in snapshot: 1000000000000"
        );
    }

    #[test]
    fn test_archive_unpack_account_path() {
        let mut header = Header::new_gnu();
        header.set_path("accounts/123.456").unwrap();
        header.set_size(4);
        header.set_cksum();
        let data: &[u8] = &[1, 2, 3, 4];

        let mut archive = Builder::new(Vec::new());
        archive.append(&header, data).unwrap();
        let result = with_finalize_and_unpack(archive, |ar, tmp| {
            let tmp_path_buf = tmp.to_path_buf();
            let file_creator = file_creator(256, &IoSetupState::default(), move |file_info| {
                assert_eq!(file_info.path, tmp_path_buf.join("accounts_dest/123.456"));
                assert_eq!(data.len(), file_info.size as usize);
                Some(file_info.file)
            })
            .expect("must make file_creator");
            unpack_snapshot_with_processors(
                ar,
                file_creator,
                tmp,
                &[tmp.join("accounts_dest")],
                |_, _| {},
            )
        });
        assert_matches!(result, Ok(()));
    }
}
