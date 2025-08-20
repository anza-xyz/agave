use {
    crate::file_io::{file_creator, FileCreator},
    bzip2::bufread::BzDecoder,
    crossbeam_channel::Sender,
    log::*,
    rand::{thread_rng, Rng},
    solana_genesis_config::{GenesisConfig, DEFAULT_GENESIS_ARCHIVE, DEFAULT_GENESIS_FILE},
    std::{
        collections::HashMap,
        fs::{self, File},
        io::{self, BufReader, Read},
        path::{
            Component::{self, CurDir, Normal},
            Path, PathBuf,
        },
        sync::Arc,
        time::Instant,
    },
    tar::{
        Archive,
        EntryType::{Directory, GNUSparse, Regular},
    },
    thiserror::Error,
};

#[derive(Error, Debug)]
pub enum UnpackError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Archive error: {0}")]
    Archive(String),
}

pub type Result<T> = std::result::Result<T, UnpackError>;

// 64 TiB; some safe margin to the max 128 TiB in amd64 linux userspace VmSize
// (ref: https://unix.stackexchange.com/a/386555/364236)
// note that this is directly related to the mmaped data size
// so protect against insane value
// This is the file size including holes for sparse files
const MAX_SNAPSHOT_ARCHIVE_UNPACKED_APPARENT_SIZE: u64 = 64 * 1024 * 1024 * 1024 * 1024;

// 4 TiB;
// This is the actually consumed disk usage for sparse files
const MAX_SNAPSHOT_ARCHIVE_UNPACKED_ACTUAL_SIZE: u64 = 4 * 1024 * 1024 * 1024 * 1024;

const MAX_SNAPSHOT_ARCHIVE_UNPACKED_COUNT: u64 = 5_000_000;
pub const MAX_GENESIS_ARCHIVE_UNPACKED_SIZE: u64 = 10 * 1024 * 1024; // 10 MiB
const MAX_GENESIS_ARCHIVE_UNPACKED_COUNT: u64 = 100;

// The buffer should be large enough to saturate write I/O bandwidth, while also accommodating:
// - Many small files: each file consumes at least one write-capacity-sized chunk (0.5-1 MiB).
// - Large files: their data may accumulate in backlog buffers while waiting for file open
//   operations to complete.
const MAX_UNPACK_WRITE_BUF_SIZE: usize = 512 * 1024 * 1024;

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
pub enum UnpackPath<'a> {
    Valid(&'a Path),
    Ignore,
    Invalid,
}

fn unpack_archive<'a, A, C, D>(
    mut archive: Archive<A>,
    input_archive_size: u64,
    apparent_limit_size: u64,
    actual_limit_size: u64,
    limit_count: u64,
    mut entry_checker: C,   // checks if entry is valid
    file_path_processor: D, // processes file paths after writing
) -> Result<()>
where
    A: Read,
    C: FnMut(&[&str], tar::EntryType) -> UnpackPath<'a>,
    D: FnMut(PathBuf),
{
    let mut apparent_total_size: u64 = 0;
    let mut actual_total_size: u64 = 0;
    let mut total_count: u64 = 0;

    let mut total_entries = 0;
    let mut open_dirs = Vec::new();

    // Bound the buffer based on provided limit of unpacked data and input archive size
    // (decompression multiplies content size, but buffering more than origin isn't necessary).
    let buf_size =
        (input_archive_size.min(actual_limit_size) as usize).min(MAX_UNPACK_WRITE_BUF_SIZE);
    let mut files_creator = file_creator(buf_size, file_path_processor)?;

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

        let unpack = unpack_entry(&mut files_creator, entry, entry_path, open_dir);
        check_unpack_result(unpack, path_str)?;

        total_entries += 1;
    }
    files_creator.drain()?;

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
        entry.unpack(&dst)?;
        // Sanitize permissions.
        set_perms(&dst, mode)?;

        if !entry.header().entry_type().is_dir() {
            // Process file after setting permissions
            files_creator.file_complete(dst);
        }
        return Ok(());
    }
    files_creator.schedule_create_at_dir(dst, mode, dst_open_dir, &mut entry)?;

    return Ok(());

    #[cfg(unix)]
    fn set_perms(dst: &Path, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let perm = fs::Permissions::from_mode(mode as _);
        fs::set_permissions(dst, perm)
    }

    #[cfg(windows)]
    fn set_perms(dst: &Path, _mode: u32) -> io::Result<()> {
        super::file_io::set_file_readonly(dst, false)
    }
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

/// Map from AppendVec file name to unpacked file system location
pub type UnpackedAppendVecMap = HashMap<String, PathBuf>;

/// Unpacks snapshot and collects AppendVec file names & paths
pub fn unpack_snapshot<A: Read>(
    archive: Archive<A>,
    input_archive_size: u64,
    ledger_dir: &Path,
    account_paths: &[PathBuf],
) -> Result<UnpackedAppendVecMap> {
    let mut unpacked_append_vec_map = UnpackedAppendVecMap::new();
    unpack_snapshot_with_processors(
        archive,
        input_archive_size,
        ledger_dir,
        account_paths,
        |file, path| {
            unpacked_append_vec_map.insert(file.to_string(), path.join("accounts").join(file));
        },
        |_| {},
    )
    .map(|_| unpacked_append_vec_map)
}

/// Unpacks snapshot from (potentially partial) `archive` and
/// sends entry file paths through the `sender` channel
pub fn streaming_unpack_snapshot<A: Read>(
    archive: Archive<A>,
    input_archive_size: u64,
    ledger_dir: &Path,
    account_paths: &[PathBuf],
    sender: &Sender<PathBuf>,
) -> Result<()> {
    unpack_snapshot_with_processors(
        archive,
        input_archive_size,
        ledger_dir,
        account_paths,
        |_, _| {},
        |file_path| {
            let result = sender.send(file_path);
            if let Err(err) = result {
                panic!(
                    "failed to send path '{}' from unpacker to rebuilder: {err}",
                    err.0.display(),
                );
            }
        },
    )
}

fn unpack_snapshot_with_processors<A, F, G>(
    archive: Archive<A>,
    input_archive_size: u64,
    ledger_dir: &Path,
    account_paths: &[PathBuf],
    mut accounts_path_processor: F,
    file_path_processor: G,
) -> Result<()>
where
    A: Read,
    F: FnMut(&str, &Path),
    G: FnMut(PathBuf),
{
    assert!(!account_paths.is_empty());

    unpack_archive(
        archive,
        input_archive_size,
        MAX_SNAPSHOT_ARCHIVE_UNPACKED_APPARENT_SIZE,
        MAX_SNAPSHOT_ARCHIVE_UNPACKED_ACTUAL_SIZE,
        MAX_SNAPSHOT_ARCHIVE_UNPACKED_COUNT,
        |parts, kind| {
            if is_valid_snapshot_archive_entry(parts, kind) {
                if let ["accounts", file] = parts {
                    // Randomly distribute the accounts files about the available `account_paths`,
                    let path_index = thread_rng().gen_range(0..account_paths.len());
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
        file_path_processor,
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

#[derive(Error, Debug)]
pub enum OpenGenesisConfigError {
    #[error("unpack error: {0}")]
    Unpack(#[from] UnpackError),
    #[error("Genesis load error: {0}")]
    Load(#[from] std::io::Error),
}

pub fn open_genesis_config(
    ledger_path: &Path,
    max_genesis_archive_unpacked_size: u64,
) -> std::result::Result<GenesisConfig, OpenGenesisConfigError> {
    match GenesisConfig::load(ledger_path) {
        Ok(genesis_config) => Ok(genesis_config),
        Err(load_err) => {
            warn!(
                "Failed to load genesis_config at {ledger_path:?}: {load_err}. Will attempt to \
                 unpack genesis archive and then retry loading."
            );

            let genesis_package = ledger_path.join(DEFAULT_GENESIS_ARCHIVE);
            unpack_genesis_archive(
                &genesis_package,
                ledger_path,
                max_genesis_archive_unpacked_size,
            )?;
            GenesisConfig::load(ledger_path).map_err(OpenGenesisConfigError::Load)
        }
    }
}

pub fn unpack_genesis_archive(
    archive_filename: &Path,
    destination_dir: &Path,
    max_genesis_archive_unpacked_size: u64,
) -> std::result::Result<(), UnpackError> {
    info!("Extracting {archive_filename:?}...");
    let extract_start = Instant::now();

    fs::create_dir_all(destination_dir)?;
    let tar_bz2 = File::open(archive_filename)?;
    let archive_size = tar_bz2.metadata()?.len();
    let tar = BzDecoder::new(BufReader::new(tar_bz2));
    let archive = Archive::new(tar);
    unpack_genesis(
        archive,
        archive_size,
        destination_dir,
        max_genesis_archive_unpacked_size,
    )?;
    info!(
        "Extracted {:?} in {:?}",
        archive_filename,
        Instant::now().duration_since(extract_start)
    );
    Ok(())
}

fn unpack_genesis<A: Read>(
    archive: Archive<A>,
    input_archive_size: u64,
    unpack_dir: &Path,
    max_genesis_archive_unpacked_size: u64,
) -> Result<()> {
    unpack_archive(
        archive,
        input_archive_size,
        max_genesis_archive_unpacked_size,
        max_genesis_archive_unpacked_size,
        MAX_GENESIS_ARCHIVE_UNPACKED_COUNT,
        |p, k| is_valid_genesis_archive_entry(unpack_dir, p, k),
        |_| {},
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
        assert_matches::assert_matches,
        tar::{Builder, Header},
    };

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
        solana_logger::setup();
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
        C: Fn(Archive<BufReader<&[u8]>>, &Path) -> Result<()>,
    {
        let data = archive.into_inner().unwrap();
        let reader = BufReader::new(&data[..]);
        let archive = Archive::new(reader);
        let temp_dir = tempfile::TempDir::new().unwrap();

        checker(archive, temp_dir.path())?;
        // Check that there is no bad permissions preventing deletion.
        let result = temp_dir.close();
        assert_matches!(result, Ok(()));
        Ok(())
    }

    fn finalize_and_unpack_snapshot(archive: tar::Builder<Vec<u8>>) -> Result<()> {
        with_finalize_and_unpack(archive, |a, b| {
            unpack_snapshot_with_processors(a, 256, b, &[PathBuf::new()], |_, _| {}, |_| {})
                .map(|_| ())
        })
    }

    fn finalize_and_unpack_genesis(archive: tar::Builder<Vec<u8>>) -> Result<()> {
        with_finalize_and_unpack(archive, |a, b| {
            unpack_genesis(a, 256, b, MAX_GENESIS_ARCHIVE_UNPACKED_SIZE)
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
        with_finalize_and_unpack(archive, |mut unpacking_archive, path| {
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
            unpack_snapshot_with_processors(
                ar,
                256,
                tmp,
                &[tmp.join("accounts_dest")],
                |_, _| {},
                |path| assert_eq!(path, tmp.join("accounts_dest/123.456")),
            )
        });
        assert_matches!(result, Ok(()));
    }
}
