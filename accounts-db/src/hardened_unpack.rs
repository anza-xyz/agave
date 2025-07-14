use {
    bzip2::bufread::BzDecoder,
    log::*,
    rand::{thread_rng, Rng},
    solana_genesis_config::{GenesisConfig, DEFAULT_GENESIS_ARCHIVE, DEFAULT_GENESIS_FILE},
    solana_perf::packet::bytes::{Buf, Bytes, BytesMut},
    std::{
        collections::{HashMap, VecDeque},
        fs::{self, File},
        io::{self, BufReader, Read},
        path::{
            Component::{self, CurDir, Normal},
            Path, PathBuf,
        },
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

/// Collection of shareable byte slices forming a chain of bytes to read (using `std::io::Read`)
pub struct MultiBytes(VecDeque<Bytes>);

impl MultiBytes {
    pub fn new() -> Self {
        // Typically we expect 2 entries:
        // archive spanning until end of decode buffer +
        // short continuation of last entry from next buffer
        Self(VecDeque::with_capacity(2))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn push(&mut self, bytes: Bytes) {
        self.0.push_back(bytes);
    }
}

impl Default for MultiBytes {
    fn default() -> Self {
        Self::new()
    }
}

impl Read for MultiBytes {
    fn read(&mut self, mut buf: &mut [u8]) -> io::Result<usize> {
        let mut copied_len = 0;
        while let Some(bytes) = self.0.front_mut() {
            let to_copy_len = bytes.len().min(buf.len());
            let (to_copy_dst_buf, remaining_buf) = buf.split_at_mut(to_copy_len);
            bytes.copy_to_slice(to_copy_dst_buf);
            copied_len += to_copy_len;
            if bytes.is_empty() {
                self.0.pop_front();
            }
            if remaining_buf.is_empty() {
                break;
            }
            buf = remaining_buf;
        }
        Ok(copied_len)
    }
}

pub struct BytesChannelReader {
    current_bytes: MultiBytes,
    receiver: crossbeam_channel::Receiver<MultiBytes>,
}

impl BytesChannelReader {
    pub fn new(receiver: crossbeam_channel::Receiver<MultiBytes>) -> Self {
        Self {
            current_bytes: MultiBytes::new(),
            receiver,
        }
    }
}

impl Read for BytesChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.current_bytes.is_empty() {
            let Ok(new_bytes) = self.receiver.recv() else {
                return Ok(0);
            };
            self.current_bytes = new_bytes;
        }
        self.current_bytes.read(buf)
    }
}

#[derive(Debug)]
pub struct ArchiveChunker<R> {
    input: R,
    /// Intermediate buffer with tar contents to seek and split on entry boundaries
    current_decoded: Bytes,
    /// Number of bytes from last entry that were not available in decoded buffer
    num_started_entry_bytes: usize,
    mempool: VecDeque<Bytes>,
}

impl<R: Read> ArchiveChunker<R> {
    const TAR_BLOCK_SIZE: usize = size_of::<tar::Header>();
    // Buffer size will influence typical amount of bytes sent as single work item.
    // Pick value significantly larger than majority of entries, yet not too large to keep
    // the work-queue non-empty as much as possible.
    const DECODE_BUF_SIZE: usize = 64 * 1024 * 1024;

    pub fn new(input: R) -> Self {
        Self {
            input,
            current_decoded: Bytes::new(),
            num_started_entry_bytes: 0,
            mempool: VecDeque::new(),
        }
    }

    /// Read `self.input`, split it at TAR archive boundaries and send chunks consisting
    /// of complete, independent tar archives into `chunk_sender`.
    pub fn decode_and_send_chunks(
        mut self,
        chunk_sender: crossbeam_channel::Sender<MultiBytes>,
    ) -> io::Result<()> {
        // Bytes for chunk of archive to be sent to workers for unpacking
        let mut current_chunk = MultiBytes::new();
        while self.refill_decoded_buf()? {
            let (new_bytes, was_archive_completion) = if self.has_started_entry() {
                let started_entry_bytes = self.take_started_entry_bytes();
                let did_finish_entry = !self.has_started_entry();
                (started_entry_bytes, did_finish_entry)
            } else {
                (self.take_complete_archive()?, true)
            };
            if !new_bytes.is_empty() {
                current_chunk.push(new_bytes);
                if was_archive_completion {
                    let chunk = std::mem::take(&mut current_chunk);
                    if chunk_sender.send(chunk).is_err() {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// Take as many bytes as possible from decoded data until last entry boundary.
    fn take_complete_archive(&mut self) -> io::Result<Bytes> {
        let mut archive = Archive::new(self.current_decoded.as_ref());

        let mut completed_entry_end = 0;
        let mut entry_end = 0;
        for entry in archive.entries()? {
            let entry = entry?;
            // End of file data
            assert_ne!(tar::EntryType::GNUSparse, entry.header().entry_type());
            entry_end = (entry.raw_file_position() + entry.size()) as usize;

            // Padding to block size
            entry_end = Self::TAR_BLOCK_SIZE * entry_end.div_ceil(Self::TAR_BLOCK_SIZE);
            if entry_end <= self.current_decoded.len() {
                // Entry ends within decoded input, we can consume it
                completed_entry_end = entry_end;
            }
            if entry_end + Self::TAR_BLOCK_SIZE > self.current_decoded.len() {
                // Next entry's header spans beyond input - can't decode it,
                // so terminate at last completed entry and keep remaining input after it
                break;
            }
        }
        // Either we run out of entries or last entry crosses input
        let completed_entry = self.current_decoded.split_to(completed_entry_end);
        if completed_entry.is_empty() && entry_end == completed_entry_end {
            // Archive ended, clear any tar footer from remaining input
            assert!(
                self.current_decoded.len() <= 1024,
                "Footer should be at most 1024 len"
            );
            self.current_decoded.clear();
        }
        self.num_started_entry_bytes = entry_end - completed_entry_end;
        Ok(completed_entry)
    }

    fn has_started_entry(&self) -> bool {
        self.num_started_entry_bytes > 0
    }

    fn take_started_entry_bytes(&mut self) -> Bytes {
        let num_bytes = self.num_started_entry_bytes.min(self.current_decoded.len());
        self.num_started_entry_bytes -= num_bytes;
        self.current_decoded.split_to(num_bytes)
    }

    /// Re-fill decoded buffer such that it has minimum bytes to decode TAR header.
    ///
    /// Return `false` on EOF
    fn refill_decoded_buf(&mut self) -> io::Result<bool> {
        if self.current_decoded.len() < Self::TAR_BLOCK_SIZE {
            let mut next_buffer = self.get_next_buffer();
            if !self.current_decoded.is_empty() {
                next_buffer.extend_from_slice(&self.current_decoded);
            }
            self.current_decoded = self.decode_bytes(next_buffer)?;
        }
        Ok(!self.current_decoded.is_empty())
    }

    /// Acquire memory buffer for decoding input reusing already consumed chunks.
    fn get_next_buffer(&mut self) -> BytesMut {
        if self.mempool.front().is_some_and(Bytes::is_unique) {
            let mut reclaimed: BytesMut = self.mempool.pop_front().unwrap().into();
            reclaimed.clear();
            reclaimed
        } else {
            BytesMut::with_capacity(Self::DECODE_BUF_SIZE)
        }
    }

    /// Fill `decode_buf` with data from `self.input`.
    fn decode_bytes(&mut self, mut decode_buf: BytesMut) -> io::Result<Bytes> {
        let mut_slice = unsafe {
            std::slice::from_raw_parts_mut(decode_buf.as_mut_ptr(), decode_buf.capacity())
        };
        let mut current_len = decode_buf.len();
        while current_len < decode_buf.capacity() {
            let new_bytes = self.input.read(&mut mut_slice[current_len..])?;
            if new_bytes == 0 {
                break;
            }
            current_len += new_bytes;
        }
        unsafe { decode_buf.set_len(current_len) };
        let bytes: Bytes = decode_buf.into();
        self.mempool.push_back(bytes.clone());
        Ok(bytes)
    }
}

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

fn check_unpack_result(unpack_result: bool, path: String) -> Result<()> {
    if !unpack_result {
        return Err(UnpackError::Archive(format!("failed to unpack: {path:?}")));
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
    apparent_limit_size: u64,
    actual_limit_size: u64,
    limit_count: u64,
    mut entry_checker: C, // checks if entry is valid
    entry_processor: D,   // processes entry after setting permissions
) -> Result<()>
where
    A: Read,
    C: FnMut(&[&str], tar::EntryType) -> UnpackPath<'a>,
    D: Fn(PathBuf),
{
    let mut apparent_total_size: u64 = 0;
    let mut actual_total_size: u64 = 0;
    let mut total_count: u64 = 0;

    let mut total_entries = 0;
    let mut sanitized_paths_cache = Vec::new();

    for entry in archive.entries()? {
        let mut entry = entry?;
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
            sanitize_path(&account, unpack_dir, &mut sanitized_paths_cache)
        } else {
            sanitize_path(&path, unpack_dir, &mut sanitized_paths_cache)
        }?; // ? handles file system errors
        let Some(entry_path) = entry_path else {
            continue; // skip it
        };

        let unpack = entry.unpack(&entry_path);
        check_unpack_result(unpack.map(|_unpack| true)?, path_str)?;

        // Sanitize permissions.
        let mode = match entry.header().entry_type() {
            GNUSparse | Regular => 0o644,
            _ => 0o755,
        };
        set_perms(&entry_path, mode)?;

        // Process entry after setting permissions
        entry_processor(entry_path);

        total_entries += 1;
    }
    info!("unpacked {total_entries} entries total");

    return Ok(());

    #[cfg(unix)]
    fn set_perms(dst: &Path, mode: u32) -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let perm = fs::Permissions::from_mode(mode as _);
        fs::set_permissions(dst, perm)
    }

    #[cfg(windows)]
    fn set_perms(dst: &Path, _mode: u32) -> io::Result<()> {
        let mut perm = fs::metadata(dst)?.permissions();
        // This is OK for Windows, but clippy doesn't realize we're doing this
        // only on Windows.
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        fs::set_permissions(dst, perm)
    }
}

// return Err on file system error
// return Some(path) if path is good
// return None if we should skip this file
fn sanitize_path(
    entry_path: &Path,
    dst: &Path,
    cache: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<Option<PathBuf>> {
    // We cannot call unpack_in because it errors if we try to use 2 account paths.
    // So, this code is borrowed from unpack_in
    // ref: https://docs.rs/tar/*/tar/struct.Entry.html#method.unpack_in
    let mut file_dst = dst.to_path_buf();
    const SKIP: Result<Option<PathBuf>> = Ok(None);
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

    if let Err(insert_at) = cache.binary_search_by(|(dst_cached, parent_cached)| {
        parent.cmp(parent_cached).then_with(|| dst.cmp(dst_cached))
    }) {
        fs::create_dir_all(parent)?;

        // Here we are different than untar_in. The code for tar::unpack_in internally calling unpack is a little different.
        // ignore return value here
        validate_inside_dst(dst, parent)?;
        cache.insert(insert_at, (dst.to_path_buf(), parent.to_path_buf()));
    }
    let target = parent.join(entry_path.file_name().unwrap());

    Ok(Some(target))
}

// copied from:
// https://github.com/alexcrichton/tar-rs/blob/d90a02f582c03dfa0fd11c78d608d0974625ae5d/src/entry.rs#L781
fn validate_inside_dst(dst: &Path, file_dst: &Path) -> Result<PathBuf> {
    // Abort if target (canonical) parent is outside of `dst`
    let canon_parent = file_dst.canonicalize().map_err(|err| {
        UnpackError::Archive(format!(
            "{} while canonicalizing {}",
            err,
            file_dst.display()
        ))
    })?;
    let canon_target = dst.canonicalize().map_err(|err| {
        UnpackError::Archive(format!("{} while canonicalizing {}", err, dst.display()))
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
    ledger_dir: &Path,
    account_paths: &[PathBuf],
) -> Result<UnpackedAppendVecMap> {
    let mut unpacked_append_vec_map = UnpackedAppendVecMap::new();
    unpack_snapshot_with_processors(
        archive,
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
    ledger_dir: &Path,
    account_paths: &[PathBuf],
    sender: &crossbeam_channel::Sender<PathBuf>,
) -> Result<()> {
    unpack_snapshot_with_processors(
        archive,
        ledger_dir,
        account_paths,
        |_, _| {},
        |entry_path_buf| {
            if entry_path_buf.is_file() {
                let result = sender.send(entry_path_buf);
                if let Err(err) = result {
                    panic!(
                        "failed to send path '{}' from unpacker to rebuilder: {err}",
                        err.0.display(),
                    );
                }
            }
        },
    )
}

fn unpack_snapshot_with_processors<A, F, G>(
    archive: Archive<A>,
    ledger_dir: &Path,
    account_paths: &[PathBuf],
    mut accounts_path_processor: F,
    entry_processor: G,
) -> Result<()>
where
    A: Read,
    F: FnMut(&str, &Path),
    G: Fn(PathBuf),
{
    assert!(!account_paths.is_empty());

    unpack_archive(
        archive,
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
        entry_processor,
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
    let tar = BzDecoder::new(BufReader::new(tar_bz2));
    let archive = Archive::new(tar);
    unpack_genesis(archive, destination_dir, max_genesis_archive_unpacked_size)?;
    info!(
        "Extracted {:?} in {:?}",
        archive_filename,
        Instant::now().duration_since(extract_start)
    );
    Ok(())
}

fn unpack_genesis<A: Read>(
    archive: Archive<A>,
    unpack_dir: &Path,
    max_genesis_archive_unpacked_size: u64,
) -> Result<()> {
    unpack_archive(
        archive,
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
        let archive: Archive<std::io::BufReader<&[u8]>> = Archive::new(reader);
        let temp_dir = tempfile::TempDir::new().unwrap();

        checker(archive, temp_dir.path())?;
        // Check that there is no bad permissions preventing deletion.
        let result = temp_dir.close();
        assert_matches!(result, Ok(()));
        Ok(())
    }

    fn finalize_and_unpack_snapshot(archive: tar::Builder<Vec<u8>>) -> Result<()> {
        with_finalize_and_unpack(archive, |a, b| {
            unpack_snapshot_with_processors(a, b, &[PathBuf::new()], |_, _| {}, |_| {}).map(|_| ())
        })
    }

    fn finalize_and_unpack_genesis(archive: tar::Builder<Vec<u8>>) -> Result<()> {
        with_finalize_and_unpack(archive, |a, b| {
            unpack_genesis(a, b, MAX_GENESIS_ARCHIVE_UNPACKED_SIZE)
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
        let result = check_unpack_result(false, "abc".to_string());
        assert_matches!(result, Err(UnpackError::Archive(ref message)) if message == "failed to unpack: \"abc\"");
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
                tmp,
                &[tmp.join("accounts_dest")],
                |_, _| {},
                |path| assert_eq!(path, tmp.join("accounts_dest/123.456")),
            )
        });
        assert_matches!(result, Ok(()));
    }
}
