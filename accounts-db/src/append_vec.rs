//! Persistent storage for accounts.
//!
//! For more information, see:
//!
//! <https://docs.anza.xyz/implemented-proposals/persistent-account-storage>

mod meta;
pub mod test_utils;

#[cfg(feature = "dev-context-only-utils")]
pub use meta::{AccountMeta, StoredAccountMeta, StoredMeta};
#[cfg(not(feature = "dev-context-only-utils"))]
use meta::{AccountMeta, StoredAccountMeta, StoredMeta};
use {
    crate::{
        account_info::Offset,
        account_storage::stored_account_info::{StoredAccountInfo, StoredAccountInfoWithoutData},
        accounts_file::{InternalsForArchive, StorageAccess, StoredAccountsInfo},
        is_zero_lamport::IsZeroLamport,
        storable_accounts::StorableAccounts,
        u64_align,
        utils::create_account_shared_data,
    },
    agave_fs::{
        buffered_reader::{
            BufReaderWithOverflow, BufferedReader, FileBufRead as _, RequiredLenBufFileRead,
            RequiredLenBufRead as _,
        },
        file_io::{read_into_buffer, write_buffer_to_file},
    },
    log::*,
    memmap2::MmapMut,
    meta::StoredAccountNoData,
    solana_account::{AccountSharedData, ReadableAccount, WritableAccount},
    solana_pubkey::Pubkey,
    solana_system_interface::MAX_PERMITTED_DATA_LENGTH,
    std::{
        self,
        convert::TryFrom,
        fs::{remove_file, File, OpenOptions},
        io::{self, BufRead, Seek, SeekFrom, Write},
        mem::{self, MaybeUninit},
        path::{Path, PathBuf},
        ptr, slice,
        sync::{
            atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
            Mutex, MutexGuard,
        },
    },
    thiserror::Error,
};

/// size of the fixed sized fields in an append vec
/// we need to add data len and align it to get the actual stored size
pub const STORE_META_OVERHEAD: usize = 136;

// Ensure the STORE_META_OVERHEAD constant remains accurate
const _: () = assert!(
    STORE_META_OVERHEAD
        == mem::size_of::<StoredMeta>()
            + mem::size_of::<AccountMeta>()
            + mem::size_of::<ObsoleteAccountHash>()
);

/// Returns the size this item will take to store plus possible alignment padding bytes before the next entry.
/// fixed-size portion of per-account data written
/// plus 'data_len', aligned to next boundary
pub fn aligned_stored_size(data_len: usize) -> usize {
    u64_align!(STORE_META_OVERHEAD + data_len)
}

/// Checked variant of [`aligned_stored_size`].
#[inline(always)]
fn aligned_stored_size_checked(data_len: usize) -> Option<usize> {
    Some(u64_align!(stored_size_checked(data_len)?))
}

/// Compute the (unaligned) stored size of an account.
#[inline(always)]
fn stored_size_checked(data_len: usize) -> Option<usize> {
    STORE_META_OVERHEAD.checked_add(data_len)
}

pub const MAXIMUM_APPEND_VEC_FILE_SIZE: u64 = 16 * 1024 * 1024 * 1024; // 16 GiB

pub type Result<T> = std::result::Result<T, AppendVecError>;

/// An enum for AppendVec related errors.
#[derive(Error, Debug)]
pub enum AppendVecError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("too small file size {0} for AppendVec")]
    FileSizeTooSmall(usize),

    #[error("too large file size {0} for AppendVec")]
    FileSizeTooLarge(usize),

    #[error("incorrect layout/length/data in the appendvec at path {}", .0.display())]
    IncorrectLayout(PathBuf),

    #[error("offset ({0}) is larger than file size ({1})")]
    OffsetOutOfBounds(usize, usize),
}

/// A slice whose contents are known to be valid.
/// The slice contains no undefined bytes.
#[derive(Debug, Copy, Clone)]
pub(crate) struct ValidSlice<'a>(&'a [u8]);

impl<'a> ValidSlice<'a> {
    #[inline(always)]
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self(data)
    }

    #[inline(always)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
}

/// offsets to help navigate the persisted format of `AppendVec`
#[derive(Debug)]
struct AccountOffsets {
    /// offset to the end of the &[u8] data
    offset_to_end_of_data: usize,
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug)]
enum AppendVecFileBacking {
    /// A file-backed block of memory that is used to store the data for each appended item.
    Mmap(MmapMut),
    /// This was opened as a read only file
    File(File),
}

/// Validates and serializes appends (when `append_guard` is called) such that only
/// writable AppendVec is updated and only from a single thread at a time.
#[derive(Debug)]
enum ReadWriteState {
    ReadOnly,
    Writable {
        /// A lock used to serialize append operations.
        append_lock: Mutex<()>,
    },
}

impl ReadWriteState {
    fn new(allow_writes: bool) -> Self {
        if allow_writes {
            Self::Writable {
                append_lock: Mutex::new(()),
            }
        } else {
            Self::ReadOnly
        }
    }

    fn append_guard(&self) -> MutexGuard<'_, ()> {
        match self {
            Self::ReadOnly => panic!("append not allowed in read-only state"),
            Self::Writable { append_lock } => append_lock.lock().unwrap(),
        }
    }
}

/// A thread-safe, file-backed block of memory used to store `Account` instances. Append operations
/// are serialized using `read_write_state`'s internal lock such that only one thread updates the
/// file at a time. No restrictions are placed on reading. That is, one may read items from one
/// thread while another is appending new items.
#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug)]
pub struct AppendVec {
    /// The file path where the data is stored.
    path: PathBuf,

    /// access the file data
    backing: AppendVecFileBacking,

    /// Guards and serializes writes if allowed
    read_write_state: ReadWriteState,

    /// The number of bytes used to store items, not the number of items.
    current_len: AtomicUsize,

    /// The number of bytes available for storing items.
    file_size: u64,

    /// if true, remove file when dropped
    remove_file_on_drop: AtomicBool,

    /// Flags if the append vec is dirty or not.
    /// Since fastboot requires that all storages are flushed to disk, be smart about it.
    /// AppendVecs are (almost) always write-once.  The common case is that an AppendVec
    /// will only need to be flushed once.  This avoids unnecessary syscalls/kernel work
    /// when nothing in the AppendVec has changed.
    is_dirty: AtomicBool,
}

const PAGE_SIZE: usize = 4 * 1024;

pub struct AppendVecStat {
    pub open_as_mmap: AtomicU64,
    pub open_as_file_io: AtomicU64,
    pub files_open: AtomicU64,
    pub files_dirty: AtomicU64,
}

pub static APPEND_VEC_STATS: AppendVecStat = AppendVecStat {
    open_as_mmap: AtomicU64::new(0),
    open_as_file_io: AtomicU64::new(0),
    files_open: AtomicU64::new(0),
    files_dirty: AtomicU64::new(0),
};

impl Drop for AppendVec {
    fn drop(&mut self) {
        APPEND_VEC_STATS.files_open.fetch_sub(1, Ordering::Relaxed);

        if *self.is_dirty.get_mut() {
            APPEND_VEC_STATS.files_dirty.fetch_sub(1, Ordering::Relaxed);
        }

        match &self.backing {
            AppendVecFileBacking::Mmap(_) => {
                APPEND_VEC_STATS
                    .open_as_mmap
                    .fetch_sub(1, Ordering::Relaxed);
            }
            AppendVecFileBacking::File(_) => {
                APPEND_VEC_STATS
                    .open_as_file_io
                    .fetch_sub(1, Ordering::Relaxed);
            }
        }
        if self.remove_file_on_drop.load(Ordering::Acquire) {
            // If we're reopening in readonly mode, we don't delete the file. See
            // AppendVec::reopen_as_readonly.
            if let Err(_err) = remove_file(&self.path) {
                // promote this to panic soon.
                // disabled due to many false positive warnings while running tests.
                // blocked by rpc's upgrade to jsonrpc v17
                //error!("AppendVec failed to remove {}: {err}", &self.path.display());
                inc_new_counter_info!("append_vec_drop_fail", 1);
            }
        }
    }
}

impl AppendVec {
    pub fn new(
        file: impl Into<PathBuf>,
        create: bool,
        size: usize,
        storage_access: StorageAccess,
    ) -> Self {
        let file = file.into();
        let initial_len = 0;
        AppendVec::sanitize_len_and_size(initial_len, size).unwrap();

        if create {
            let _ignored = remove_file(&file);
        }

        let mut data = OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(&file)
            .map_err(|e| {
                panic!(
                    "Unable to {} data file {} in current dir({:?}): {:?}",
                    if create { "create" } else { "open" },
                    file.display(),
                    std::env::current_dir(),
                    e
                );
            })
            .unwrap();

        // Theoretical performance optimization: write a zero to the end of
        // the file so that we won't have to resize it later, which may be
        // expensive.
        data.seek(SeekFrom::Start((size - 1) as u64)).unwrap();
        data.write_all(&[0]).unwrap();
        data.rewind().unwrap();
        data.flush().unwrap();

        let backing = match storage_access {
            #[allow(deprecated)]
            StorageAccess::Mmap => {
                //UNSAFE: Required to create a Mmap
                let mmap = unsafe { MmapMut::map_mut(&data) };
                let mmap = mmap.unwrap_or_else(|err| {
                    panic!(
                        "Failed to map the data file (size: {size}): {err}. Please increase \
                         sysctl vm.max_map_count or equivalent for your platform.",
                    );
                });
                APPEND_VEC_STATS
                    .open_as_mmap
                    .fetch_add(1, Ordering::Relaxed);
                AppendVecFileBacking::Mmap(mmap)
            }
            StorageAccess::File => {
                APPEND_VEC_STATS
                    .open_as_file_io
                    .fetch_add(1, Ordering::Relaxed);
                AppendVecFileBacking::File(data)
            }
        };
        APPEND_VEC_STATS.files_open.fetch_add(1, Ordering::Relaxed);

        AppendVec {
            path: file,
            backing,
            // writable state's mutex forces append to be single threaded, but concurrent with
            // reads. See UNSAFE usage in `append_ptr`
            read_write_state: ReadWriteState::new(true),
            current_len: AtomicUsize::new(initial_len),
            file_size: size as u64,
            remove_file_on_drop: AtomicBool::new(true),
            is_dirty: AtomicBool::new(false),
        }
    }

    fn sanitize_len_and_size(current_len: usize, file_size: usize) -> Result<()> {
        if file_size == 0 {
            Err(AppendVecError::FileSizeTooSmall(file_size))
        } else if usize::try_from(MAXIMUM_APPEND_VEC_FILE_SIZE)
            .map(|max| file_size > max)
            .unwrap_or(true)
        {
            Err(AppendVecError::FileSizeTooLarge(file_size))
        } else if current_len > file_size {
            Err(AppendVecError::OffsetOutOfBounds(current_len, file_size))
        } else {
            Ok(())
        }
    }

    pub fn dead_bytes_due_to_zero_lamport_single_ref(&self, count: usize) -> usize {
        aligned_stored_size(0) * count
    }

    /// Flushes contents to disk
    pub fn flush(&self) -> Result<()> {
        // Check to see if we're actually dirty before flushing.
        let should_flush = self.is_dirty.swap(false, Ordering::AcqRel);
        if should_flush {
            match &self.backing {
                AppendVecFileBacking::Mmap(mmap) => {
                    mmap.flush()?;
                }
                AppendVecFileBacking::File(file) => {
                    file.sync_all()?;
                }
            }
            APPEND_VEC_STATS.files_dirty.fetch_sub(1, Ordering::Relaxed);
        }
        Ok(())
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn reset(&self) {
        // Writable state's mutex forces append to be single threaded, but concurrent
        // with reads. See UNSAFE usage in `append_ptr`
        let _lock = self.read_write_state.append_guard();
        self.current_len.store(0, Ordering::Release);
    }

    /// Return AppendVec opened in read-only file-io mode or `None` if it already is such
    pub(crate) fn reopen_as_readonly_file_io(&self) -> Option<Self> {
        if matches!(self.read_write_state, ReadWriteState::ReadOnly)
            && matches!(self.backing, AppendVecFileBacking::File(_))
        {
            // Early return if already in read-only mode *and* already a file-io
            return None;
        }

        // we are re-opening the file, so don't remove the file on disk when the old one is dropped
        self.remove_file_on_drop.store(false, Ordering::Release);

        // add a memory barrier to ensure the the last mmap writes
        // happen before the first file-io reads
        std::sync::atomic::fence(Ordering::AcqRel);

        // The file should have already been sanitized. Don't need to check when we open the file again.
        let mut new =
            AppendVec::new_from_file_unchecked(self.path.clone(), self.len(), StorageAccess::File)
                .ok()?;
        if self.is_dirty.swap(false, Ordering::AcqRel) {
            // *move* the dirty-ness to the new append vec
            *new.is_dirty.get_mut() = true;
        }
        Some(new)
    }

    /// how many more bytes can be stored in this append vec
    pub fn remaining_bytes(&self) -> u64 {
        self.capacity()
            .saturating_sub(u64_align!(self.len()) as u64)
    }

    /// Returns the number of bytes, *not items*, used in the AppendVec
    pub fn len(&self) -> usize {
        self.current_len.load(Ordering::Acquire)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the total number of bytes, *not items*, the AppendVec can hold
    pub fn capacity(&self) -> u64 {
        self.file_size
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_from_file(
        path: impl Into<PathBuf>,
        current_len: usize,
        storage_access: StorageAccess,
    ) -> Result<(Self, usize)> {
        let path = path.into();
        let new = Self::new_from_file_unchecked(path, current_len, storage_access)?;

        let num_accounts = new.sanitize_layout_and_length()?;
        Ok((new, num_accounts))
    }

    /// Creates a new AppendVec for the underlying storage at `path`
    ///
    /// This version of `new()` may only be called when reconstructing storages as part of startup.
    /// It trusts the snapshot's value for `current_len`, and relies on later index generation or
    /// accounts verification to ensure it is valid.
    pub fn new_for_startup(
        path: impl Into<PathBuf>,
        current_len: usize,
        storage_access: StorageAccess,
    ) -> Result<Self> {
        let new = Self::new_from_file_unchecked(path, current_len, storage_access)?;

        // The current_len is allowed to be either exactly the same as file_size, or
        // u64-aligned-equivalent to file_size.  This is because `flush` and `shink` compute the
        // required file size with *aligned* stored size per account, including the very last
        // account.  So the file size may have padding to the next u64-alignment.
        // For our usage, this is still safe as these padding bytes could never be used for an
        // account.  This renders the `get_` and `scan_` functions safe.
        if (current_len as u64 == new.file_size)
            || (u64_align!(current_len) as u64 == new.file_size)
        {
            Ok(new)
        } else {
            // However, if opening a minimized snapshot, the file sizes can be
            // larger than current length [^1].  So when the `if` condition fails,
            // fallback to the old/slow impl that does the full sanitization.
            // [^1]: https://github.com/anza-xyz/agave/issues/6797
            info!(
                "Could not optimistically create new AppendVec, falling back to pessimistic impl: \
                 file size ({}) and current length ({}) do not match for '{}'",
                new.file_size,
                current_len,
                new.path.display(),
            );
            let _num_accounts = new.sanitize_layout_and_length()?;
            Ok(new)
        }
    }

    /// Creates an appendvec from existing file in read-only mode and without full data checks
    ///
    /// Validation of account data and counting the number of accounts is skipped.
    pub fn new_from_file_unchecked(
        path: impl Into<PathBuf>,
        current_len: usize,
        storage_access: StorageAccess,
    ) -> Result<Self> {
        let path = path.into();
        let file_size = std::fs::metadata(&path)?.len();
        Self::sanitize_len_and_size(current_len, file_size as usize)?;

        // AppendVec is in read-only mode, but mmap access requires file to be writable
        #[allow(deprecated)]
        let is_writable = storage_access == StorageAccess::Mmap;
        let data = OpenOptions::new()
            .read(true)
            .write(is_writable)
            .create(false)
            .open(&path)?;

        APPEND_VEC_STATS.files_open.fetch_add(1, Ordering::Relaxed);

        if storage_access == StorageAccess::File {
            APPEND_VEC_STATS
                .open_as_file_io
                .fetch_add(1, Ordering::Relaxed);

            return Ok(AppendVec {
                path,
                backing: AppendVecFileBacking::File(data),
                read_write_state: ReadWriteState::ReadOnly,
                current_len: AtomicUsize::new(current_len),
                file_size,
                remove_file_on_drop: AtomicBool::new(true),
                is_dirty: AtomicBool::new(false),
            });
        }

        let mmap = unsafe {
            let result = MmapMut::map_mut(&data);
            if result.is_err() {
                // for vm.max_map_count, error is: {code: 12, kind: Other, message: "Cannot allocate memory"}
                info!(
                    "memory map error: {result:?}. This may be because vm.max_map_count is not \
                     set correctly."
                );
            }
            result?
        };

        APPEND_VEC_STATS
            .open_as_mmap
            .fetch_add(1, Ordering::Relaxed);

        Ok(AppendVec {
            path,
            backing: AppendVecFileBacking::Mmap(mmap),
            read_write_state: ReadWriteState::ReadOnly,
            current_len: AtomicUsize::new(current_len),
            file_size,
            remove_file_on_drop: AtomicBool::new(true),
            is_dirty: AtomicBool::new(false),
        })
    }

    /// Opens the AppendVec at `path` for use by `store-tool`
    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_store_tool(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let file_size = std::fs::metadata(&path)?.len();
        Self::new_from_file_unchecked(path, file_size as usize, StorageAccess::default())
    }

    /// Checks that all accounts layout is correct and returns the number of accounts.
    fn sanitize_layout_and_length(&self) -> Result<usize> {
        // This discards allocated accounts immediately after check at each loop iteration.
        //
        // This code should not reuse AppendVec.accounts() method as the current form or
        // extend it to be reused here because it would allow attackers to accumulate
        // some measurable amount of memory needlessly.
        let mut num_accounts = 0;
        let mut matches = true;
        let mut last_offset = 0;
        self.scan_stored_accounts_no_data(|account| {
            if !matches || !account.sanitize() {
                matches = false;
                return;
            }
            last_offset = account.offset() + account.stored_size();
            num_accounts += 1;
        })?;
        let aligned_current_len = u64_align!(self.current_len.load(Ordering::Acquire));

        if !matches || last_offset != aligned_current_len {
            return Err(AppendVecError::IncorrectLayout(self.path.clone()));
        }

        Ok(num_accounts)
    }

    /// Get a reference to the data at `offset` of `size` bytes if that slice
    /// doesn't overrun the internal buffer. Otherwise return None.
    /// Also return the offset of the first byte after the requested data that
    /// falls on a 64-byte boundary.
    fn get_slice(slice: ValidSlice<'_>, offset: usize, size: usize) -> Option<(&[u8], usize)> {
        // SAFETY: Wrapping math is safe here because if `end` does wrap, the Range
        // parameter to `.get()` will be invalid, and `.get()` will correctly return None.
        let end = offset.wrapping_add(size);
        slice
            .0
            .get(offset..end)
            .map(|subslice| (subslice, u64_align!(end)))
    }

    /// Copy `len` bytes from `src` to the first 64-byte boundary after position `offset` of
    /// the internal buffer. Then update `offset` to the first byte after the copied data.
    fn append_ptr(&self, offset: &mut usize, src: *const u8, len: usize) -> io::Result<()> {
        let pos = u64_align!(*offset);
        match &self.backing {
            AppendVecFileBacking::Mmap(mmap) => {
                let data = &mmap[pos..(pos + len)];
                //UNSAFE: This mut append is safe because only 1 thread can append at a time
                //Mutex<()> guarantees exclusive write access to the memory occupied in
                //the range.
                unsafe {
                    let dst = data.as_ptr() as *mut _;
                    ptr::copy(src, dst, len);
                };
            }
            AppendVecFileBacking::File(file) => {
                // Safety: caller should ensure the passed pointer and length are valid.
                let data = unsafe { slice::from_raw_parts(src, len) };
                write_buffer_to_file(file, data, pos as u64)?;
            }
        }
        *offset = pos + len;
        Ok(())
    }

    /// Copy each value in `vals`, in order, to the first 64-byte boundary after position `offset`.
    /// If there is sufficient space, then update `offset` and the internal `current_len` to the
    /// first byte after the copied data and return the starting position of the copied data.
    /// Otherwise return None and leave `offset` unchanged.
    fn append_ptrs_locked(
        &self,
        offset: &mut usize,
        vals: &[(*const u8, usize)],
    ) -> io::Result<Option<usize>> {
        let mut end = *offset;
        for val in vals {
            end = u64_align!(end);
            end += val.1;
        }

        if (self.file_size as usize) < end {
            return Ok(None);
        }

        let pos = u64_align!(*offset);
        for val in vals {
            self.append_ptr(offset, val.0, val.1)?
        }
        self.current_len.store(*offset, Ordering::Release);
        Ok(Some(pos))
    }

    /// Return a reference to the type at `offset` if its data doesn't overrun the internal buffer.
    /// Otherwise return None. Also return the offset of the first byte after the requested data
    /// that falls on a 64-byte boundary.
    fn get_type<T>(slice: ValidSlice<'_>, offset: usize) -> Option<(&T, usize)> {
        let (data, next) = Self::get_slice(slice, offset, mem::size_of::<T>())?;
        let ptr = data.as_ptr().cast();
        //UNSAFE: The cast is safe because the slice is aligned and fits into the memory
        //and the lifetime of the &T is tied to self, which holds the underlying memory map
        Some((unsafe { &*ptr }, next))
    }

    /// MmapMut could have more capacity than `len()` knows is valid.
    /// Return the subset of `mmap` that is known to be valid.
    /// This allows comparisons against the slice len.
    fn get_valid_slice_from_mmap<'a>(&self, mmap: &'a MmapMut) -> ValidSlice<'a> {
        ValidSlice(&mmap[..self.len()])
    }

    /// Calls `callback` with the stored account at `offset`.
    ///
    /// Returns `None` if there is no account at `offset`, otherwise returns the result of
    /// `callback` in `Some`.
    ///
    /// This fn does *not* load the account's data, just the data length.  If the data is needed,
    /// use `get_stored_account_callback()` instead.  However, prefer this fn when possible.
    pub fn get_stored_account_without_data_callback<Ret>(
        &self,
        offset: usize,
        mut callback: impl for<'local> FnMut(StoredAccountInfoWithoutData<'local>) -> Ret,
    ) -> Option<Ret> {
        self.get_stored_account_no_data_callback(offset, |stored_account| {
            let account = StoredAccountInfoWithoutData {
                pubkey: stored_account.pubkey(),
                lamports: stored_account.lamports(),
                owner: stored_account.owner(),
                data_len: stored_account.data_len() as usize,
                executable: stored_account.executable(),
                rent_epoch: stored_account.rent_epoch(),
            };
            callback(account)
        })
    }

    /// Calls `callback` with the stored account at `offset`.
    ///
    /// Returns `None` if there is no account at `offset`, otherwise returns the result of
    /// `callback` in `Some`.
    ///
    /// This fn *does* load the account's data.  If the data is not needed,
    /// use `get_stored_account_without_data_callback()` instead.
    pub fn get_stored_account_callback<Ret>(
        &self,
        offset: usize,
        mut callback: impl for<'local> FnMut(StoredAccountInfo<'local>) -> Ret,
    ) -> Option<Ret> {
        self.get_stored_account_meta_callback(offset, |stored_account_meta| {
            let account = StoredAccountInfo {
                pubkey: stored_account_meta.pubkey(),
                lamports: stored_account_meta.lamports(),
                owner: stored_account_meta.owner(),
                data: stored_account_meta.data(),
                executable: stored_account_meta.executable(),
                rent_epoch: stored_account_meta.rent_epoch(),
            };
            callback(account)
        })
    }

    /// calls `callback` with the stored account metadata for the account at `offset` if its data doesn't overrun
    /// the internal buffer. Otherwise return None.
    ///
    /// Prefer get_stored_account_callback() when possible, as it does not contain file format
    /// implementation details, and thus potentially can read less and be faster.
    pub fn get_stored_account_meta_callback<Ret>(
        &self,
        offset: usize,
        mut callback: impl for<'local> FnMut(StoredAccountMeta<'local>) -> Ret,
    ) -> Option<Ret> {
        match &self.backing {
            AppendVecFileBacking::Mmap(mmap) => {
                let slice = self.get_valid_slice_from_mmap(mmap);
                let (meta, next) = Self::get_type::<StoredMeta>(slice, offset)?;
                let (account_meta, next) = Self::get_type::<AccountMeta>(slice, next)?;
                let (_hash, next) = Self::get_type::<ObsoleteAccountHash>(slice, next)?;
                let (data, next) = Self::get_slice(slice, next, meta.data_len as usize)?;
                let stored_size = next - offset;
                Some(callback(StoredAccountMeta {
                    meta,
                    account_meta,
                    data,
                    offset,
                    stored_size,
                }))
            }
            AppendVecFileBacking::File(file) => {
                // 4096 was just picked to be a single page size
                let mut buf = [MaybeUninit::<u8>::uninit(); PAGE_SIZE];
                // SAFETY: `read_into_buffer` will only write to uninitialized memory.
                let bytes_read = read_into_buffer(file, self.len(), offset, unsafe {
                    slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len())
                })
                .ok()?;
                // SAFETY: we only read the initialized portion.
                let valid_bytes = ValidSlice(unsafe {
                    slice::from_raw_parts(buf.as_ptr() as *const u8, bytes_read)
                });
                let (meta, next) = Self::get_type::<StoredMeta>(valid_bytes, 0)?;
                let (account_meta, next) = Self::get_type::<AccountMeta>(valid_bytes, next)?;
                let (_hash, next) = Self::get_type::<ObsoleteAccountHash>(valid_bytes, next)?;
                let data_len = meta.data_len;
                let remaining_bytes_for_data = bytes_read - next;
                Some(if remaining_bytes_for_data >= data_len as usize {
                    // we already read enough data to load this account
                    let (data, next) = Self::get_slice(valid_bytes, next, meta.data_len as usize)?;
                    let stored_size = next;
                    let account = StoredAccountMeta {
                        meta,
                        account_meta,
                        data,
                        offset,
                        stored_size,
                    };
                    callback(account)
                } else {
                    // not enough was read from file to get `data`
                    assert!(data_len <= MAX_PERMITTED_DATA_LENGTH, "{data_len}");
                    let mut data: Box<[MaybeUninit<u8>]> = Box::new_uninit_slice(data_len as usize);
                    // instead, we could piece together what we already read here. Maybe we just needed 1 more byte.
                    // Note here `next` is a 0-based offset from the beginning of this account.
                    // SAFETY: `read_into_buffer` will only write to uninitialized memory.
                    let bytes_read = read_into_buffer(file, self.len(), offset + next, unsafe {
                        slice::from_raw_parts_mut(data.as_mut_ptr() as *mut u8, data_len as usize)
                    })
                    .ok()?;
                    if bytes_read < data_len as usize {
                        // eof or otherwise couldn't read all the data
                        return None;
                    }
                    // SAFETY: we've just checked that `bytes_read` is at least `data_len`.
                    let data = unsafe { data.assume_init() };
                    let stored_size = aligned_stored_size(data_len as usize);
                    let account = StoredAccountMeta {
                        meta,
                        account_meta,
                        data: &data[..],
                        offset,
                        stored_size,
                    };
                    callback(account)
                })
            }
        }
    }

    /// calls `callback` with the stored account fixed portion for the account at `offset` if its data doesn't overrun
    /// the internal buffer. Otherwise return None.
    fn get_stored_account_no_data_callback<Ret>(
        &self,
        offset: usize,
        mut callback: impl for<'local> FnMut(StoredAccountNoData<'local>) -> Ret,
    ) -> Option<Ret> {
        match &self.backing {
            AppendVecFileBacking::Mmap(mmap) => {
                let slice = self.get_valid_slice_from_mmap(mmap);
                let (meta, next) = Self::get_type::<StoredMeta>(slice, offset)?;
                let (account_meta, _) = Self::get_type::<AccountMeta>(slice, next)?;
                let stored_size = aligned_stored_size_checked(meta.data_len as usize)?;

                Some(callback(StoredAccountNoData {
                    meta,
                    account_meta,
                    offset,
                    stored_size,
                }))
            }
            AppendVecFileBacking::File(file) => {
                let mut buf = [MaybeUninit::<u8>::uninit(); STORE_META_OVERHEAD];
                // SAFETY: `read_into_buffer` will only write to uninitialized memory.
                let bytes_read = read_into_buffer(file, self.len(), offset, unsafe {
                    slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, buf.len())
                })
                .ok()?;
                // SAFETY: we only read the initialized portion.
                let valid_bytes = ValidSlice(unsafe {
                    slice::from_raw_parts(buf.as_ptr() as *const u8, bytes_read)
                });
                let (meta, next) = Self::get_type::<StoredMeta>(valid_bytes, 0)?;
                let (account_meta, _) = Self::get_type::<AccountMeta>(valid_bytes, next)?;
                let stored_size = aligned_stored_size_checked(meta.data_len as usize)?;

                Some(callback(StoredAccountNoData {
                    meta,
                    account_meta,
                    offset,
                    stored_size,
                }))
            }
        }
    }

    /// return an `AccountSharedData` for an account at `offset`.
    /// This fn can efficiently return exactly what is needed by a caller.
    /// This is on the critical path of tx processing for accounts not in the read or write caches.
    pub fn get_account_shared_data(&self, offset: usize) -> Option<AccountSharedData> {
        match &self.backing {
            AppendVecFileBacking::Mmap(_) => self
                .get_stored_account_meta_callback(offset, |account| {
                    create_account_shared_data(&account)
                }),
            AppendVecFileBacking::File(file) => {
                let mut buf = MaybeUninit::<[u8; PAGE_SIZE]>::uninit();
                let bytes_read =
                    read_into_buffer(file, self.len(), offset, unsafe { &mut *buf.as_mut_ptr() })
                        .ok()?;
                // SAFETY: we only read the initialized portion.
                let valid_bytes = ValidSlice(unsafe {
                    slice::from_raw_parts(buf.as_ptr() as *const u8, bytes_read)
                });
                let (meta, next) = Self::get_type::<StoredMeta>(valid_bytes, 0)?;
                let (account_meta, next) = Self::get_type::<AccountMeta>(valid_bytes, next)?;
                let (_hash, next) = Self::get_type::<ObsoleteAccountHash>(valid_bytes, next)?;
                let data_len = meta.data_len;
                let remaining_bytes_for_data = bytes_read - next;
                Some(if remaining_bytes_for_data >= data_len as usize {
                    // we already read enough data to load this account
                    let (data, next) = Self::get_slice(valid_bytes, next, meta.data_len as usize)?;
                    let stored_size = next;
                    let account = StoredAccountMeta {
                        meta,
                        account_meta,
                        data,
                        offset,
                        stored_size,
                    };
                    // data is within `buf`, so just allocate a new vec for data
                    create_account_shared_data(&account)
                } else {
                    // not enough was read from file to get `data`
                    assert!(data_len <= MAX_PERMITTED_DATA_LENGTH, "{data_len}");
                    let mut data = Vec::with_capacity(data_len as usize);
                    let slice = data.spare_capacity_mut();
                    // Note here `next` is a 0-based offset from the beginning of this account.
                    // SAFETY: `read_into_buffer` will only write to uninitialized memory.
                    let bytes_read = read_into_buffer(file, self.len(), offset + next, unsafe {
                        slice::from_raw_parts_mut(slice.as_mut_ptr() as *mut u8, data_len as usize)
                    })
                    .ok()?;
                    if bytes_read < data_len as usize {
                        // eof or otherwise couldn't read all the data
                        return None;
                    }
                    // SAFETY: we've just checked that `bytes_read` is at least `data_len`.
                    unsafe { data.set_len(data_len as usize) };
                    AccountSharedData::create(
                        account_meta.lamports,
                        data,
                        account_meta.owner,
                        account_meta.executable,
                        account_meta.rent_epoch,
                    )
                })
            }
        }
    }

    #[cfg(test)]
    pub fn get_account_test(
        &self,
        offset: usize,
    ) -> Option<(Pubkey, solana_account::AccountSharedData)> {
        let data_len = self.get_account_data_lens(&[offset]);
        let sizes: usize = data_len
            .iter()
            .map(|len| AppendVec::calculate_stored_size(*len))
            .sum();
        let result = self.get_stored_account_meta_callback(offset, |r_callback| {
            let r2 = self.get_account_shared_data(offset);
            assert!(solana_account::accounts_equal(
                &r_callback,
                r2.as_ref().unwrap()
            ));
            assert_eq!(sizes, r_callback.stored_size());
            let pubkey = r_callback.meta().pubkey;
            Some((pubkey, create_account_shared_data(&r_callback)))
        });
        if result.is_none() {
            assert!(self
                .get_stored_account_meta_callback(offset, |_| {})
                .is_none());
            assert!(self.get_account_shared_data(offset).is_none());
            // it has different rules for checking len and returning None
            assert_eq!(sizes, 0);
        }
        result.flatten()
    }

    /// Returns the path to the file where the data is stored
    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    /// help with the math of offsets when navigating the on-disk layout in an AppendVec.
    /// data is at the end of each account and is variable sized
    /// the next account is then aligned on a 64 bit boundary.
    /// With these helpers, we can skip over reading some of the data depending on what the caller wants.
    ///
    /// *Safety* - The caller must ensure that the `stored_meta.data_len` won't overflow the calculation.
    fn next_account_offset(start_offset: usize, stored_meta: &StoredMeta) -> AccountOffsets {
        let stored_size_unaligned = STORE_META_OVERHEAD
            .checked_add(stored_meta.data_len as usize)
            .expect("stored size cannot overflow");
        let offset_to_end_of_data = start_offset + stored_size_unaligned;

        AccountOffsets {
            offset_to_end_of_data,
        }
    }

    /// Iterate over all accounts and call `callback` with each account.
    ///
    /// `callback` parameters:
    /// * Offset: the offset within the file of this account
    /// * StoredAccountInfoWithoutData: the account itself, without account data
    ///
    /// Note that account data is not read/passed to the callback.
    pub fn scan_accounts_without_data(
        &self,
        mut callback: impl for<'local> FnMut(Offset, StoredAccountInfoWithoutData<'local>),
    ) -> Result<()> {
        self.scan_stored_accounts_no_data(|stored_account| {
            let offset = stored_account.offset();
            let account = StoredAccountInfoWithoutData {
                pubkey: stored_account.pubkey(),
                lamports: stored_account.lamports(),
                owner: stored_account.owner(),
                data_len: stored_account.data_len() as usize,
                executable: stored_account.executable(),
                rent_epoch: stored_account.rent_epoch(),
            };
            callback(offset, account);
        })
    }

    /// Iterate over all accounts and call `callback` with each account.
    ///
    /// `callback` parameters:
    /// * Offset: the offset within the file of this account
    /// * StoredAccountInfo: the account itself, with account data
    ///
    /// Prefer scan_accounts_without_data() when account data is not needed,
    /// as it can potentially read less and be faster.
    pub(crate) fn scan_accounts<'a>(
        &'a self,
        reader: &mut impl RequiredLenBufFileRead<'a>,
        mut callback: impl for<'local> FnMut(Offset, StoredAccountInfo<'local>),
    ) -> Result<()> {
        self.scan_accounts_stored_meta(reader, |stored_account_meta| {
            let offset = stored_account_meta.offset();
            let account = StoredAccountInfo {
                pubkey: stored_account_meta.pubkey(),
                lamports: stored_account_meta.lamports(),
                owner: stored_account_meta.owner(),
                data: stored_account_meta.data(),
                executable: stored_account_meta.executable(),
                rent_epoch: stored_account_meta.rent_epoch(),
            };
            callback(offset, account);
        })
    }

    /// Iterate over all accounts and call `callback` with each account.
    ///
    /// Prefer scan_accounts() when possible, as it does not contain file format
    /// implementation details, and thus potentially can read less and be faster.
    #[allow(clippy::blocks_in_conditions)]
    fn scan_accounts_stored_meta<'a>(
        &'a self,
        reader: &mut impl RequiredLenBufFileRead<'a>,
        mut callback: impl for<'local> FnMut(StoredAccountMeta<'local>),
    ) -> Result<()> {
        match &self.backing {
            AppendVecFileBacking::Mmap(_mmap) => {
                let mut offset = 0;
                while self
                    .get_stored_account_meta_callback(offset, |account| {
                        offset += account.stored_size();
                        if account.is_zero_lamport() && account.pubkey() == &Pubkey::default() {
                            // we passed the last useful account
                            return false;
                        }

                        callback(account);
                        true
                    })
                    .unwrap_or_default()
                {}
            }
            AppendVecFileBacking::File(file) => {
                reader.set_file(file, self.len())?;

                let mut min_buf_len = STORE_META_OVERHEAD;
                loop {
                    let offset = reader.get_file_offset();
                    let bytes = match reader.fill_buf_required(min_buf_len) {
                        Ok([]) => break,
                        Ok(bytes) => ValidSlice::new(bytes),
                        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
                        Err(err) => return Err(AppendVecError::Io(err)),
                    };

                    let (meta, next) = Self::get_type::<StoredMeta>(bytes, 0).unwrap();
                    let (account_meta, next) = Self::get_type::<AccountMeta>(bytes, next).unwrap();
                    if account_meta.lamports == 0 && meta.pubkey == Pubkey::default() {
                        // we passed the last useful account
                        break;
                    }
                    let (_hash, next) = Self::get_type::<ObsoleteAccountHash>(bytes, next).unwrap();
                    let data_len = meta.data_len as usize;
                    let leftover = bytes.len() - next;
                    if leftover >= data_len {
                        // we already read enough data to load this account
                        let data = &bytes.0[next..(next + data_len)];
                        let stored_size = aligned_stored_size(data_len);
                        let account = StoredAccountMeta {
                            meta,
                            account_meta,
                            data,
                            offset,
                            stored_size,
                        };
                        callback(account);
                        reader.consume(stored_size);
                        // restore default required buffer size
                        min_buf_len = STORE_META_OVERHEAD;
                    } else {
                        // repeat loop with required buffer size holding whole account data
                        min_buf_len = STORE_META_OVERHEAD + data_len;
                    }
                }
            }
        }
        Ok(())
    }

    /// Scans accounts with StoredAccountMeta
    ///
    /// Only intended to be called by agave-store-tool.
    /// Refer to `scan_accounts_stored_meta` for further documentation.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn scan_accounts_stored_meta_for_store_tool(
        &self,
        callback: impl for<'local> FnMut(StoredAccountMeta<'local>),
    ) -> Result<()> {
        let mut reader = new_scan_accounts_reader();
        self.scan_accounts_stored_meta(&mut reader, callback)
    }

    /// Calculate the amount of storage required for an account with the passed
    /// in data_len
    pub(crate) fn calculate_stored_size(data_len: usize) -> usize {
        aligned_stored_size(data_len)
    }

    /// for each offset in `sorted_offsets`, get the the amount of data stored in the account.
    pub(crate) fn get_account_data_lens(&self, sorted_offsets: &[usize]) -> Vec<usize> {
        // self.len() is an atomic load, so only do it once
        let self_len = self.len();
        let mut account_sizes = Vec::with_capacity(sorted_offsets.len());
        match &self.backing {
            AppendVecFileBacking::Mmap(mmap) => {
                let slice = self.get_valid_slice_from_mmap(mmap);
                for &offset in sorted_offsets {
                    let Some((stored_meta, _)) = Self::get_type::<StoredMeta>(slice, offset) else {
                        break;
                    };
                    let next = Self::next_account_offset(offset, stored_meta);
                    if next.offset_to_end_of_data > self_len {
                        // data doesn't fit, so don't include
                        break;
                    }
                    account_sizes.push(stored_meta.data_len as usize);
                }
            }
            AppendVecFileBacking::File(file) => {
                let mut buffer = [MaybeUninit::<u8>::uninit(); mem::size_of::<StoredMeta>()];
                for &offset in sorted_offsets {
                    // SAFETY: `read_into_buffer` will only write to uninitialized memory.
                    let Some(bytes_read) = read_into_buffer(file, self_len, offset, unsafe {
                        slice::from_raw_parts_mut(
                            buffer.as_mut_ptr() as *mut u8,
                            mem::size_of::<StoredMeta>(),
                        )
                    })
                    .ok() else {
                        break;
                    };
                    // SAFETY: we only read the initialized portion.
                    let bytes = ValidSlice(unsafe {
                        slice::from_raw_parts(buffer.as_ptr() as *const u8, bytes_read)
                    });
                    let Some((stored_meta, _)) = Self::get_type::<StoredMeta>(bytes, 0) else {
                        break;
                    };
                    let next = Self::next_account_offset(offset, stored_meta);
                    if next.offset_to_end_of_data > self_len {
                        // data doesn't fit, so don't include
                        break;
                    }
                    account_sizes.push(stored_meta.data_len as usize);
                }
            }
        }
        account_sizes
    }

    /// iterate over all pubkeys and call `callback`.
    /// no references have to be maintained/returned from an iterator function.
    /// This fn can operate on a batch of data at once.
    pub fn scan_pubkeys(&self, mut callback: impl FnMut(&Pubkey)) -> Result<()> {
        self.scan_stored_accounts_no_data(|account| {
            callback(account.pubkey());
        })
    }

    /// Iterate over all accounts and call `callback` with the fixed sized portion of each account.
    fn scan_stored_accounts_no_data(
        &self,
        mut callback: impl FnMut(StoredAccountNoData),
    ) -> Result<()> {
        let self_len = self.len();
        match &self.backing {
            AppendVecFileBacking::Mmap(mmap) => {
                let mut offset = 0;
                let slice = self.get_valid_slice_from_mmap(mmap);
                loop {
                    let Some((stored_meta, next)) = Self::get_type::<StoredMeta>(slice, offset)
                    else {
                        break;
                    };
                    let Some((account_meta, _)) = Self::get_type::<AccountMeta>(slice, next) else {
                        break;
                    };
                    if account_meta.lamports == 0 && stored_meta.pubkey == Pubkey::default() {
                        // we passed the last useful account
                        break;
                    }
                    let Some(stored_size) = stored_size_checked(stored_meta.data_len as usize)
                    else {
                        break;
                    };
                    if offset + stored_size > self_len {
                        break;
                    }
                    let stored_size = u64_align!(stored_size);
                    callback(StoredAccountNoData {
                        meta: stored_meta,
                        account_meta,
                        offset,
                        stored_size,
                    });
                    offset += stored_size;
                }
            }
            AppendVecFileBacking::File(file) => {
                // Heuristic observed in benchmarking that maintains a reasonable balance between syscalls and data waste
                const BUFFER_SIZE: usize = PAGE_SIZE * 4;
                let mut reader = BufferedReader::<BUFFER_SIZE>::new().with_file(file, self_len);
                const REQUIRED_READ_LEN: usize =
                    mem::size_of::<StoredMeta>() + mem::size_of::<AccountMeta>();
                loop {
                    let offset = reader.get_file_offset();
                    let bytes = match reader.fill_buf_required(REQUIRED_READ_LEN) {
                        Ok([]) => break,
                        Ok(bytes) => ValidSlice::new(bytes),
                        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
                        Err(err) => return Err(AppendVecError::Io(err)),
                    };
                    let (stored_meta, next) = Self::get_type::<StoredMeta>(bytes, 0).unwrap();
                    let (account_meta, _) = Self::get_type::<AccountMeta>(bytes, next).unwrap();
                    if account_meta.lamports == 0 && stored_meta.pubkey == Pubkey::default() {
                        // we passed the last useful account
                        break;
                    }
                    let Some(stored_size) = stored_size_checked(stored_meta.data_len as usize)
                    else {
                        break;
                    };
                    if offset + stored_size > self_len {
                        break;
                    }
                    let stored_size = u64_align!(stored_size);
                    callback(StoredAccountNoData {
                        meta: stored_meta,
                        account_meta,
                        offset,
                        stored_size,
                    });
                    reader.consume(stored_size);
                }
            }
        }
        Ok(())
    }

    /// Copy each account metadata, account and hash to the internal buffer.
    /// If there is no room to write the first entry, None is returned.
    /// Otherwise, returns the starting offset of each account metadata.
    /// Plus, the final return value is the offset where the next entry would be appended.
    /// So, return.len() is 1 + (number of accounts written)
    /// After each account is appended, the internal `current_len` is updated
    /// and will be available to other threads.
    pub fn append_accounts<'a>(
        &self,
        accounts: &impl StorableAccounts<'a>,
        skip: usize,
    ) -> Option<StoredAccountsInfo> {
        let _lock = self.read_write_state.append_guard();
        let mut offset = self.len();
        let len = accounts.len();
        // Here we have `len - skip` number of accounts.  The +1 extra capacity
        // is for storing the aligned offset of the last-plus-one entry,
        // which is used to compute the size of the last stored account.
        let offsets_len = len - skip + 1;
        let mut offsets = Vec::with_capacity(offsets_len);
        let mut stop = false;
        for i in skip..len {
            if stop {
                break;
            }
            accounts.account_default_if_zero_lamport(i, |account| {
                let account_meta = AccountMeta {
                    lamports: account.lamports(),
                    owner: *account.owner(),
                    rent_epoch: account.rent_epoch(),
                    executable: account.executable(),
                };

                let stored_meta = StoredMeta {
                    pubkey: *account.pubkey(),
                    data_len: account.data().len() as u64,
                    write_version_obsolete: 0,
                };
                let stored_meta_ptr = ptr::from_ref(&stored_meta).cast();
                let account_meta_ptr = ptr::from_ref(&account_meta).cast();
                let hash_ptr = ObsoleteAccountHash::ZEROED.0.as_ptr();
                let data_ptr = account.data().as_ptr();
                let ptrs = [
                    (stored_meta_ptr, mem::size_of::<StoredMeta>()),
                    (account_meta_ptr, mem::size_of::<AccountMeta>()),
                    (hash_ptr, mem::size_of::<ObsoleteAccountHash>()),
                    (data_ptr, stored_meta.data_len as usize),
                ];
                if let Some(start_offset) = self
                    .append_ptrs_locked(&mut offset, &ptrs)
                    .expect("must append data to append_vec")
                {
                    offsets.push(start_offset)
                } else {
                    stop = true;
                }
            });
        }

        if !offsets.is_empty() {
            // If we've actually written to the AppendVec, make sure we mark it as dirty.
            // This ensures we properly flush it later.
            let was_dirty = self.is_dirty.swap(true, Ordering::AcqRel);
            if !was_dirty {
                APPEND_VEC_STATS.files_dirty.fetch_add(1, Ordering::Relaxed);
            }
        }

        (!offsets.is_empty()).then(|| {
            // The last entry in the offsets needs to be the u64 aligned `offset`, because that's
            // where the *next* entry will begin to be stored.
            // This is used to compute the size of the last stored account; make sure to remove
            // it afterwards!
            offsets.push(u64_align!(offset));
            let size = offsets.windows(2).map(|offset| offset[1] - offset[0]).sum();
            offsets.pop();

            StoredAccountsInfo { offsets, size }
        })
    }

    /// Returns the way to access this accounts file when archiving
    pub(crate) fn internals_for_archive(&self) -> InternalsForArchive<'_> {
        match &self.backing {
            AppendVecFileBacking::File(_file) => InternalsForArchive::FileIo(self.path()),
            // note this returns the entire mmap slice, even bytes that we consider invalid
            AppendVecFileBacking::Mmap(mmap) => InternalsForArchive::Mmap(mmap),
        }
    }
}

/// Create a reusable buffered reader tuned for scanning storages with account data.
pub(crate) fn new_scan_accounts_reader<'a>() -> impl RequiredLenBufFileRead<'a> {
    // 128KiB covers a reasonably large distribution of typical account sizes.
    // In a recent sample, 99.98% of accounts' data lengths were less than or equal to 128KiB.
    const MIN_CAPACITY: usize = 1024 * 128;
    const MAX_CAPACITY: usize = STORE_META_OVERHEAD + MAX_PERMITTED_DATA_LENGTH as usize;
    const BUFFER_SIZE: usize = PAGE_SIZE * 8;
    BufReaderWithOverflow::new(
        BufferedReader::<BUFFER_SIZE>::new(),
        MIN_CAPACITY,
        MAX_CAPACITY,
    )
}

/// The per-account hash, stored in the AppendVec.
///
/// This field is now obsolete, but it still lives in the file format.
#[derive(Debug)]
struct ObsoleteAccountHash([u8; 32]);

impl ObsoleteAccountHash {
    /// The constant of all zeroes, to be stored in the file.
    const ZEROED: Self = Self([0; 32]);
}

#[cfg(test)]
pub mod tests {
    use {
        super::{test_utils::*, *},
        assert_matches::assert_matches,
        memoffset::offset_of,
        rand::{prelude::*, rng},
        rand_chacha::ChaChaRng,
        solana_account::{accounts_equal, Account, AccountSharedData},
        solana_clock::Slot,
        std::{mem::ManuallyDrop, time::Instant},
        test_case::{test_case, test_matrix},
    };

    impl AppendVec {
        fn append_account_test(&self, data: &(Pubkey, AccountSharedData)) -> Option<usize> {
            let slot_ignored = Slot::MAX;
            let accounts = [(&data.0, &data.1)];
            let slice = &accounts[..];
            let storable_accounts = (slot_ignored, slice);

            self.append_accounts(&storable_accounts, 0)
                .map(|res| res.offsets[0])
        }
    }

    // Hash is [u8; 32], which has no alignment
    static_assertions::assert_eq_align!(u64, StoredMeta, AccountMeta);

    // Offset of the first account's `data_len` field.
    const ACCOUNT_0_DATA_LEN_OFFSET: u64 = core::mem::offset_of!(StoredMeta, data_len) as u64;

    #[test]
    fn test_account_meta_default() {
        let def1 = AccountMeta::default();
        let def2 = AccountMeta::from(&Account::default());
        assert_eq!(&def1, &def2);
        let def2 = AccountMeta::from(&AccountSharedData::default());
        assert_eq!(&def1, &def2);
        let def2 = AccountMeta::from(Some(&AccountSharedData::default()));
        assert_eq!(&def1, &def2);
        let none: Option<&AccountSharedData> = None;
        let def2 = AccountMeta::from(none);
        assert_eq!(&def1, &def2);
    }

    #[test]
    fn test_account_meta_non_default() {
        let def1 = AccountMeta {
            lamports: 1,
            owner: Pubkey::new_unique(),
            executable: true,
            rent_epoch: 3,
        };
        let def2_account = Account {
            lamports: def1.lamports,
            owner: def1.owner,
            executable: def1.executable,
            rent_epoch: def1.rent_epoch,
            data: Vec::new(),
        };
        let def2 = AccountMeta::from(&def2_account);
        assert_eq!(&def1, &def2);
        let def2 = AccountMeta::from(&AccountSharedData::from(def2_account.clone()));
        assert_eq!(&def1, &def2);
        let def2 = AccountMeta::from(Some(&AccountSharedData::from(def2_account)));
        assert_eq!(&def1, &def2);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    #[should_panic(expected = "FileSizeTooSmall(0)")]
    fn test_append_vec_new_bad_size(storage_access: StorageAccess) {
        let path = get_append_vec_path("test_append_vec_new_bad_size");
        let _av = AppendVec::new(&path.path, true, 0, storage_access);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_append_vec_new_from_file_bad_size(storage_access: StorageAccess) {
        let file = get_append_vec_path("test_append_vec_new_from_file_bad_size");
        let path = &file.path;

        let _data = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .expect("create a test file for mmap");

        let result = AppendVec::new_from_file(path, 0, storage_access);
        assert_matches!(result, Err(ref message) if message.to_string().contains("too small file size 0 for AppendVec"));
    }

    #[test]
    fn test_append_vec_sanitize_len_and_size_too_small() {
        const LEN: usize = 0;
        const SIZE: usize = 0;
        let result = AppendVec::sanitize_len_and_size(LEN, SIZE);
        assert_matches!(result, Err(ref message) if message.to_string().contains("too small file size 0 for AppendVec"));
    }

    #[test]
    fn test_append_vec_sanitize_len_and_size_maximum() {
        const LEN: usize = 0;
        const SIZE: usize = 16 * 1024 * 1024 * 1024;
        let result = AppendVec::sanitize_len_and_size(LEN, SIZE);
        assert_matches!(result, Ok(_));
    }

    #[test]
    fn test_append_vec_sanitize_len_and_size_too_large() {
        const LEN: usize = 0;
        const SIZE: usize = 16 * 1024 * 1024 * 1024 + 1;
        let result = AppendVec::sanitize_len_and_size(LEN, SIZE);
        assert_matches!(result, Err(ref message) if message.to_string().contains("too large file size 17179869185 for AppendVec"));
    }

    #[test]
    fn test_append_vec_sanitize_len_and_size_full_and_same_as_current_len() {
        const LEN: usize = 1024 * 1024;
        const SIZE: usize = 1024 * 1024;
        let result = AppendVec::sanitize_len_and_size(LEN, SIZE);
        assert_matches!(result, Ok(_));
    }

    #[test]
    fn test_append_vec_sanitize_len_and_size_larger_current_len() {
        const LEN: usize = 1024 * 1024 + 1;
        const SIZE: usize = 1024 * 1024;
        let result = AppendVec::sanitize_len_and_size(LEN, SIZE);
        assert_matches!(result, Err(ref message) if message.to_string().contains("is larger than file size (1048576)"));
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_append_vec_one(storage_access: StorageAccess) {
        let path = get_append_vec_path("test_append");
        let av = AppendVec::new(&path.path, true, 1024 * 1024, storage_access);
        let account = create_test_account(0);
        let index = av.append_account_test(&account).unwrap();
        assert_eq!(av.get_account_test(index).unwrap(), account);
        truncate_and_test(av, index);
    }

    /// truncate `av` and make sure that we fail to get an account. This verifies that the eof
    /// code is working correctly.
    fn truncate_and_test(av: AppendVec, index: usize) {
        // truncate the hash, 1 byte at a time
        let hash_size = std::mem::size_of::<ObsoleteAccountHash>();
        for _ in 0..hash_size {
            av.current_len.fetch_sub(1, Ordering::Relaxed);
            assert_eq!(av.get_account_test(index), None);
        }
        // truncate 1 byte into the AccountMeta
        av.current_len.fetch_sub(1, Ordering::Relaxed);
        assert_eq!(av.get_account_test(index), None);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_append_vec_one_with_data(storage_access: StorageAccess) {
        let path = get_append_vec_path("test_append");
        let av = AppendVec::new(&path.path, true, 1024 * 1024, storage_access);
        let data_len = 1;
        let account = create_test_account(data_len);
        let index = av.append_account_test(&account).unwrap();
        // make the append vec 1 byte too short. we should get `None` since the append vec was truncated
        assert_eq!(
            STORE_META_OVERHEAD + data_len,
            av.current_len.load(Ordering::Relaxed)
        );
        assert_eq!(av.get_account_test(index).unwrap(), account);
        truncate_and_test(av, index);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_remaining_bytes(storage_access: StorageAccess) {
        let path = get_append_vec_path("test_append");
        let sz = 1024 * 1024;
        let sz64 = sz as u64;
        let av = AppendVec::new(&path.path, true, sz, storage_access);
        assert_eq!(av.capacity(), sz64);
        assert_eq!(av.remaining_bytes(), sz64);

        // append first account, an u64 aligned account (136 bytes)
        let mut av_len = 0;
        let account = create_test_account(0);
        av.append_account_test(&account).unwrap();
        av_len += STORE_META_OVERHEAD;
        assert_eq!(av.capacity(), sz64);
        assert_eq!(av.remaining_bytes(), sz64 - (STORE_META_OVERHEAD as u64));
        assert_eq!(av.len(), av_len);

        // append second account, a *not* u64 aligned account (137 bytes)
        let account = create_test_account(1);
        let account_storage_len = STORE_META_OVERHEAD + 1;
        av_len += account_storage_len;
        av.append_account_test(&account).unwrap();
        assert_eq!(av.capacity(), sz64);
        assert_eq!(av.len(), av_len);
        let alignment_bytes = u64_align!(av_len) - av_len; // bytes used for alignment (7 bytes)
        assert_eq!(alignment_bytes, 7);
        assert_eq!(av.remaining_bytes(), sz64 - u64_align!(av_len) as u64);

        // append third account, a *not* u64 aligned account (137 bytes)
        let account = create_test_account(1);
        av.append_account_test(&account).unwrap();
        let account_storage_len = STORE_META_OVERHEAD + 1;
        av_len += alignment_bytes; // bytes used for alignment at the end of previous account
        av_len += account_storage_len;
        assert_eq!(av.capacity(), sz64);
        assert_eq!(av.len(), av_len);
        assert_eq!(av.remaining_bytes(), sz64 - u64_align!(av_len) as u64);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_append_vec_data(storage_access: StorageAccess) {
        let path = get_append_vec_path("test_append_data");
        let av = AppendVec::new(&path.path, true, 1024 * 1024, storage_access);
        let account = create_test_account(5);
        let index = av.append_account_test(&account).unwrap();
        assert_eq!(av.get_account_test(index).unwrap(), account);
        let account1 = create_test_account(6);
        let index1 = av.append_account_test(&account1).unwrap();
        assert_eq!(av.get_account_test(index).unwrap(), account);
        assert_eq!(av.get_account_test(index1).unwrap(), account1);
    }

    impl AppendVec {
        /// return how many accounts in the storage
        fn accounts_count(&self) -> usize {
            let mut count = 0;
            self.scan_stored_accounts_no_data(|_| {
                count += 1;
            })
            .expect("must scan accounts storage");
            count
        }
    }

    /// Generate a random append vec with the given number of accounts.
    ///
    /// This provides the accounts and pubkeys used to populate the append vec. As such,
    /// it may be used to test the correctness of reading back the accounts from the append vec.
    ///
    /// It also ensures the following:
    /// - A `MAX_PERMITTED_DATA_LENGTH` and 64KiB account are present. This can be useful for exercising
    ///   implementation details that are sensitive to the size of the account. For example, `scan_accounts_stored_meta`
    ///   will need to use a heap-allocated buffer when the account data size is greater than its stack buffer.
    /// - Accounts have randomized data, lamports, and owner.
    fn rand_exhaustive_append_vec(
        num_accounts: usize,
    ) -> (
        ManuallyDrop<AppendVec>,
        StoredAccountsInfo,
        Vec<(Pubkey, AccountSharedData)>,
        TempFile,
    ) {
        let mut rng = rng();
        let mut create_account = |data_len: usize| -> (Pubkey, AccountSharedData) {
            let pubkey = Pubkey::new_from_array(rng.random());
            let owner = Pubkey::new_from_array(rng.random());
            let mut account = AccountSharedData::new(rng.random(), data_len, &owner);
            // Ensure we actually have some unique data to compare against when checking correctness
            let data = std::iter::from_fn(|| Some(rng.random::<u8>()))
                .take(data_len)
                .collect::<Vec<_>>();
            account.set_data(data);
            (pubkey, account)
        };

        let mut test_accounts = Vec::with_capacity(num_accounts);
        let mut file_size = 0;
        let special_file_interval = num_accounts / 8;
        for i in 0..num_accounts {
            let data_len = match i {
                // Create several spread out accounts with varying sizes:
                // for (x / special_file_interval) in 0..7 range
                x if x % special_file_interval == 0 => {
                    // mult increases in 0 to 3 range twice
                    let mult = (x / special_file_interval) % 4;
                    // and data_len goes over 0..MAX_PERMITTED_DATA_LENGTH range also twice
                    mult * (MAX_PERMITTED_DATA_LENGTH as usize) / 3
                }
                // Otherwise use a reasonably small account to avoid long test times
                x => x % 256,
            };
            let account = create_account(data_len);
            let size = aligned_stored_size(account.1.data().len());
            file_size += size;
            test_accounts.push(account);
        }

        let path = get_append_vec_path("test_scan_accounts_stored_meta_correctness");
        let av = ManuallyDrop::new(AppendVec::new(
            &path.path,
            true,
            file_size,
            #[allow(deprecated)]
            StorageAccess::Mmap,
        ));
        let slot = 42;
        let stored_accounts_info = av
            .append_accounts(&(slot, test_accounts.as_slice()), 0)
            .unwrap();
        av.flush().unwrap();
        (av, stored_accounts_info, test_accounts, path)
    }

    /// Test that scanning accounts correctly reads back all accounts that were written.
    #[test]
    fn test_scan_accounts_correctness() {
        let num_accounts = 100;
        let (av_mmap, _, test_accounts, path) = rand_exhaustive_append_vec(num_accounts);
        let av_file = AppendVec::new_from_file(&path.path, av_mmap.len(), StorageAccess::File)
            .unwrap()
            .0;
        let mut reader = new_scan_accounts_reader();
        for av in [&av_mmap, &av_file] {
            let mut index = 0;
            av.scan_accounts_stored_meta(&mut reader, |v| {
                let (pubkey, account) = &test_accounts[index];
                let recovered = create_account_shared_data(&v);
                assert_eq!(&recovered, account);
                assert_eq!(v.pubkey(), pubkey);
                index += 1;
            })
            .expect("must scan accounts storage");
            assert_eq!(index, num_accounts);
        }
        for av in [&av_mmap, &av_file] {
            let mut index = 0;
            av.scan_stored_accounts_no_data(|stored_account| {
                let (pubkey, account) = &test_accounts[index];
                assert_eq!(stored_account.pubkey(), pubkey);
                assert_eq!(stored_account.lamports(), account.lamports());
                assert_eq!(stored_account.owner(), account.owner());
                assert_eq!(stored_account.data_len(), account.data().len() as u64);
                assert_eq!(stored_account.executable(), account.executable());
                assert_eq!(stored_account.rent_epoch(), account.rent_epoch());
                index += 1;
            })
            .expect("must scan accounts storage");
            assert_eq!(index, num_accounts);
        }
    }

    /// Test that scanning accounts correctly handles useless accounts.
    #[test]
    fn test_scan_useless_accounts() {
        let num_accounts = 33;
        let num_new_accounts = num_accounts - 2;
        let (av_mmap, stored_accounts_info, test_accounts, path) =
            rand_exhaustive_append_vec(num_accounts);
        let av_current_len = av_mmap.len();

        // Rewrite the append vec to mark account at num_new_accounts as useless.
        // This will also "hide" any accounts later in the file.
        if let AppendVecFileBacking::Mmap(mmap) = &av_mmap.backing {
            let slice = av_mmap.get_valid_slice_from_mmap(mmap);
            let mut stored_meta_offset = stored_accounts_info.offsets[num_new_accounts];
            let (stored_meta, mut account_meta_offset) =
                AppendVec::get_type::<StoredMeta>(slice, stored_meta_offset).unwrap();
            let (account_meta, _next) =
                AppendVec::get_type::<AccountMeta>(slice, account_meta_offset).unwrap();
            assert_eq!(stored_meta.pubkey, test_accounts[num_new_accounts].0);

            let new_stored_meta = StoredMeta {
                pubkey: Pubkey::default(),
                ..stored_meta.clone()
            };
            let new_account_meta = AccountMeta {
                lamports: 0,
                ..account_meta.clone()
            };
            av_mmap
                .append_ptr(
                    &mut stored_meta_offset,
                    ptr::from_ref(&new_stored_meta).cast(),
                    size_of::<StoredMeta>(),
                )
                .unwrap();
            av_mmap
                .append_ptr(
                    &mut account_meta_offset,
                    ptr::from_ref(&new_account_meta).cast(),
                    size_of::<AccountMeta>(),
                )
                .unwrap();
            av_mmap.flush().unwrap();
        } else {
            panic!("append vec must be mmap");
        }

        let av_file =
            AppendVec::new_from_file_unchecked(&path.path, av_current_len, StorageAccess::File)
                .unwrap();
        let mut reader = new_scan_accounts_reader();
        for av in [&av_mmap, &av_file] {
            let mut index = 0;
            av.scan_accounts_stored_meta(&mut reader, |stored_account| {
                let (pubkey, account) = &test_accounts[index];
                let recovered = create_account_shared_data(&stored_account);
                assert_eq!(stored_account.pubkey(), pubkey);
                assert_eq!(recovered, *account);
                index += 1;
            })
            .expect("must scan accounts storage");
            assert_eq!(index, num_new_accounts);
        }
        for av in [&av_mmap, &av_file] {
            let mut index = 0;
            av.scan_stored_accounts_no_data(|stored_account| {
                let (pubkey, account) = &test_accounts[index];
                assert_eq!(stored_account.pubkey(), pubkey);
                assert_eq!(stored_account.lamports(), account.lamports());
                assert_eq!(stored_account.owner(), account.owner());
                assert_eq!(stored_account.data_len(), account.data().len() as u64);
                assert_eq!(stored_account.executable(), account.executable());
                assert_eq!(stored_account.rent_epoch(), account.rent_epoch());
                index += 1;
            })
            .expect("must scan accounts storage");
            assert_eq!(index, num_new_accounts);
        }
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_append_vec_append_many(storage_access: StorageAccess) {
        let path = get_append_vec_path("test_append_many");
        let av = AppendVec::new(&path.path, true, 1024 * 1024, storage_access);
        let size = 1000;
        let mut indexes = vec![];
        let now = Instant::now();
        let mut sizes = vec![];
        for sample in 0..size {
            // sample + 1 is so sample = 0 won't be used.
            // sample = 0 produces default account with default pubkey
            let account = create_test_account(sample + 1);
            sizes.push(aligned_stored_size(account.1.data().len()));
            let pos = av.append_account_test(&account).unwrap();
            assert_eq!(av.get_account_test(pos).unwrap(), account);
            indexes.push(pos);
            let stored_size = av
                .get_account_data_lens(indexes.as_slice())
                .iter()
                .map(|len| AppendVec::calculate_stored_size(*len))
                .sum::<usize>();
            assert_eq!(sizes.iter().sum::<usize>(), stored_size);
        }
        trace!("append time: {} ms", now.elapsed().as_millis());

        let now = Instant::now();
        for _ in 0..size {
            let sample = rng().random_range(0..indexes.len());
            let account = create_test_account(sample + 1);
            assert_eq!(av.get_account_test(indexes[sample]).unwrap(), account);
        }
        trace!("random read time: {} ms", now.elapsed().as_millis());
        assert_eq!(indexes.len(), size);
        assert_eq!(indexes[0], 0);
        assert_eq!(av.accounts_count(), size);

        let mut reader = new_scan_accounts_reader();

        let mut sample = 0;
        let now = Instant::now();
        av.scan_accounts_stored_meta(&mut reader, |v| {
            let account = create_test_account(sample + 1);
            let recovered = create_account_shared_data(&v);
            assert_eq!(recovered, account.1);
            sample += 1;
        })
        .expect("must scan accounts storage");
        trace!("sequential read time: {} ms", now.elapsed().as_millis());
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_new_from_file_crafted_zero_lamport_account(storage_access: StorageAccess) {
        // This test verifies that when we sanitize on load, that we fail sanitizing if we load an account with zero lamports that does not have all default value fields.
        // This test writes an account with zero lamports, but with 3 bytes of data. On load, it asserts that load fails.
        // It used to be possible to use the append vec api to write an account to an append vec with zero lamports, but with non-default values for other account fields.
        // This will no longer be possible. Thus, to implement the write portion of this test would require additional test-only parameters to public apis or otherwise duplicating code paths.
        // So, the sanitizing on load behavior can be tested by capturing [u8] that would be created if such a write was possible (as it used to be).
        // The contents of [u8] written by an append vec cannot easily or reasonably change frequently since it has released a long time.
        /*
            agave_logger::setup();
            // uncomment this code to generate the invalid append vec that will fail on load
            let file = get_append_vec_path("test_append");
            let path = &file.path;
            let mut av = AppendVec::new(path, true, 256);
            av.set_no_remove_on_drop();

            let pubkey = solana_pubkey::new_rand();
            let owner = Pubkey::default();
            let data_len = 3_u64;
            let mut account = AccountSharedData::new(0, data_len as usize, &owner);
            account.set_data(b"abc".to_vec());
            let stored_meta = StoredMeta {
                write_version: 0,
                pubkey,
                data_len,
            };
            let account_with_meta = (stored_meta, account);
            let index = av.append_account_test(&account_with_meta).unwrap();
            assert_eq!(av.get_account_test(index).unwrap(), account_with_meta);

            av.flush().unwrap();
            let accounts_len = av.len();
            drop(av);
            // read file and log out as [u8]
            use std::fs::File;
            use std::io::BufReader;
            use std::io::Read;
            let f = File::open(path).unwrap();
            let mut reader = BufReader::new(f);
            let mut buffer = Vec::new();
            reader.read_to_end(&mut buffer).unwrap();
            error!("{:?}", buffer);
        */

        // create an invalid append vec file using known bytes
        let file = get_append_vec_path("test_append_bytes");
        let path = &file.path;

        let accounts_len = 139;
        {
            let append_vec_data = [
                0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 192, 118, 150, 1, 185, 209, 118,
                82, 154, 222, 172, 202, 110, 26, 218, 140, 143, 96, 61, 43, 212, 73, 203, 7, 190,
                88, 80, 222, 110, 114, 67, 254, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 97, 98, 99, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];

            let f = std::fs::File::create(path).unwrap();
            let mut writer = std::io::BufWriter::new(f);
            writer.write_all(append_vec_data.as_slice()).unwrap();
        }

        let result = AppendVec::new_from_file(path, accounts_len, storage_access);
        assert_matches!(result, Err(ref message) if message.to_string().contains("incorrect layout/length/data"));
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_new_from_file_crafted_data_len(storage_access: StorageAccess) {
        let file = get_append_vec_path("test_new_from_file_crafted_data_len");
        let path = &file.path;
        let accounts_len = {
            // wrap AppendVec in ManuallyDrop to ensure we do not remove the backing file when dropped
            let av = ManuallyDrop::new(AppendVec::new(path, true, 1024 * 1024, storage_access));

            av.append_account_test(&create_test_account(10)).unwrap();
            av.flush().unwrap();
            av.len()
        };

        // Assert that the file is currently valid.
        {
            let av =
                ManuallyDrop::new(AppendVec::new_from_file(path, accounts_len, storage_access));
            assert!(av.is_ok());
        }

        // Manually manipulate the `data_len` bytes of the first account.
        {
            let crafted_data_len = 1u64;

            let mut file = OpenOptions::new().write(true).open(path).unwrap();
            file.seek(SeekFrom::Start(ACCOUNT_0_DATA_LEN_OFFSET))
                .unwrap();
            file.write_all(&crafted_data_len.to_ne_bytes()).unwrap();
            file.flush().unwrap();
        }

        let result = AppendVec::new_from_file(path, accounts_len, storage_access);
        assert_matches!(result, Err(ref message) if message.to_string().contains("incorrect layout/length/data"));
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_append_vec_reset(storage_access: StorageAccess) {
        let file = get_append_vec_path("test_append_vec_reset");
        let path = &file.path;
        let av = AppendVec::new(path, true, 1024 * 1024, storage_access);
        av.append_account_test(&create_test_account(10)).unwrap();

        assert!(!av.is_empty());
        av.reset();
        assert_eq!(av.len(), 0);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_append_vec_flush(storage_access: StorageAccess) {
        let file = get_append_vec_path("test_append_vec_flush");
        let path = &file.path;
        let accounts_len = {
            // wrap AppendVec in ManuallyDrop to ensure we do not remove the backing file when dropped
            let av = ManuallyDrop::new(AppendVec::new(path, true, 1024 * 1024, storage_access));
            av.append_account_test(&create_test_account(10)).unwrap();
            av.len()
        };

        let (av, num_account) =
            AppendVec::new_from_file(path, accounts_len, storage_access).unwrap();
        av.flush().unwrap();
        assert_eq!(num_account, 1);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_append_vec_reopen_as_readonly(storage_access: StorageAccess) {
        let file = get_append_vec_path("test_append_vec_flush");
        let path = &file.path;
        let accounts_len = {
            let av = AppendVec::new(path, true, 1024 * 1024, storage_access);
            av.append_account_test(&create_test_account(10)).unwrap();
            // wrap AppendVec in ManuallyDrop to ensure we do not remove the backing file when dropped
            let ro_av = ManuallyDrop::new(
                av.reopen_as_readonly_file_io()
                    .expect("appendable AppendVec should always re-open as read-only"),
            );
            ro_av.len()
        };

        let (av, _) = AppendVec::new_from_file(path, accounts_len, storage_access).unwrap();
        let reopen = av.reopen_as_readonly_file_io();
        // even if AppendVec is already read-only, but uses mmap, it should reopen as file_io
        if storage_access == StorageAccess::File {
            assert!(reopen.is_none());
        } else {
            assert!(reopen.is_some());
        }
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_new_from_file_too_large_data_len(storage_access: StorageAccess) {
        let file = get_append_vec_path("test_new_from_file_too_large_data_len");
        let path = &file.path;
        let accounts_len = {
            // wrap AppendVec in ManuallyDrop to ensure we do not remove the backing file when dropped
            let av = ManuallyDrop::new(AppendVec::new(path, true, 1024 * 1024, storage_access));

            av.append_account_test(&create_test_account(10)).unwrap();

            av.flush().unwrap();
            av.len()
        };

        // Assert that the file is currently valid.
        {
            let av =
                ManuallyDrop::new(AppendVec::new_from_file(path, accounts_len, storage_access));
            assert!(av.is_ok());
        }

        // Manually manipulate the `data_len` bytes of the first account.
        {
            let too_large_data_len = u64::MAX;

            let mut file = OpenOptions::new().write(true).open(path).unwrap();
            file.seek(SeekFrom::Start(ACCOUNT_0_DATA_LEN_OFFSET))
                .unwrap();
            file.write_all(&too_large_data_len.to_ne_bytes()).unwrap();
            file.flush().unwrap();
        }

        let result = AppendVec::new_from_file(path, accounts_len, storage_access);
        assert_matches!(result, Err(ref message) if message.to_string().contains("incorrect layout/length/data"));
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_new_from_file_crafted_executable(storage_access: StorageAccess) {
        let file = get_append_vec_path("test_new_from_crafted_executable");
        let path = &file.path;

        // Write a valid append vec file.
        let accounts_len = {
            // wrap AppendVec in ManuallyDrop to ensure we do not remove the backing file when dropped
            let av = ManuallyDrop::new(AppendVec::new(path, true, 1024 * 1024, storage_access));
            av.append_account_test(&create_test_account(10)).unwrap();
            let offset_1 = {
                let mut executable_account = create_test_account(10);
                executable_account.1.set_executable(true);
                av.append_account_test(&executable_account).unwrap()
            };

            // reload accounts
            // ensure false is 0u8 and true is 1u8 actually
            av.get_stored_account_no_data_callback(0, |account| {
                assert_eq!(*account.ref_executable_byte(), 0);
            })
            .unwrap();
            av.get_stored_account_no_data_callback(offset_1, |account| {
                assert_eq!(*account.ref_executable_byte(), 1);
            })
            .unwrap();

            av.flush().unwrap();
            av.len()
        };

        // Assert that the file is currently valid.
        {
            let av =
                ManuallyDrop::new(AppendVec::new_from_file(path, accounts_len, storage_access));
            assert!(av.is_ok());
        }

        // Manually manipulate the `executable` byte of the first account.
        {
            const ACCOUNT_0_EXECUTABLE_OFFSET: u64 = (core::mem::size_of::<StoredMeta>()
                + core::mem::offset_of!(AccountMeta, executable))
                as u64;
            let crafted_executable = u8::MAX - 1;

            let mut file = OpenOptions::new().write(true).open(path).unwrap();
            file.seek(SeekFrom::Start(ACCOUNT_0_EXECUTABLE_OFFSET))
                .unwrap();
            file.write_all(&[crafted_executable]).unwrap();
            file.flush().unwrap();
        }

        let result = AppendVec::new_from_file(path, accounts_len, storage_access);
        assert_matches!(result, Err(ref message) if message.to_string().contains("incorrect layout/length/data"));
    }

    #[test]
    fn test_type_layout() {
        assert_eq!(offset_of!(StoredMeta, write_version_obsolete), 0x00);
        assert_eq!(offset_of!(StoredMeta, data_len), 0x08);
        assert_eq!(offset_of!(StoredMeta, pubkey), 0x10);
        assert_eq!(mem::size_of::<StoredMeta>(), 0x30);

        assert_eq!(offset_of!(AccountMeta, lamports), 0x00);
        assert_eq!(offset_of!(AccountMeta, rent_epoch), 0x08);
        assert_eq!(offset_of!(AccountMeta, owner), 0x10);
        assert_eq!(offset_of!(AccountMeta, executable), 0x30);
        assert_eq!(mem::size_of::<AccountMeta>(), 0x38);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_get_account_shared_data_from_truncated_file(storage_access: StorageAccess) {
        let file = get_append_vec_path("test_get_account_shared_data_from_truncated_file");
        let path = &file.path;

        {
            // Set up a test account with data_len larger than PAGE_SIZE (i.e.
            // AppendVec internal buffer size is PAGESIZE).
            let data_len: usize = 2 * PAGE_SIZE;
            let account = create_test_account_with(data_len);
            // wrap AppendVec in ManuallyDrop to ensure we do not remove the backing file when dropped
            let av = ManuallyDrop::new(AppendVec::new(
                path,
                true,
                aligned_stored_size(data_len),
                storage_access,
            ));
            av.append_account_test(&account).unwrap();
            av.flush().unwrap();
        }

        // Truncate the AppendVec to PAGESIZE. This will cause get_account* to fail to load the account.
        let truncated_accounts_len: usize = PAGE_SIZE;
        let av = AppendVec::new_from_file_unchecked(path, truncated_accounts_len, storage_access)
            .unwrap();
        let account = av.get_account_shared_data(0);
        assert!(account.is_none()); // Expect None to be returned.

        let result = av.get_stored_account_meta_callback(0, |_| true);
        assert!(result.is_none()); // Expect None to be returned.
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_get_account_sizes(storage_access: StorageAccess) {
        const NUM_ACCOUNTS: usize = 37;
        let pubkeys: Vec<_> = std::iter::repeat_with(Pubkey::new_unique)
            .take(NUM_ACCOUNTS)
            .collect();

        let mut rng = rng();
        let mut accounts = Vec::with_capacity(pubkeys.len());
        let mut stored_sizes = Vec::with_capacity(pubkeys.len());
        for _ in &pubkeys {
            let lamports = rng.random();
            let data_len = rng.random_range(0..MAX_PERMITTED_DATA_LENGTH) as usize;
            let account = AccountSharedData::new(lamports, data_len, &Pubkey::default());
            accounts.push(account);
            stored_sizes.push(aligned_stored_size(data_len));
        }
        let accounts = accounts;
        let stored_sizes = stored_sizes;
        let total_stored_size = stored_sizes.iter().sum();

        let temp_file = get_append_vec_path("test_get_account_sizes");
        let account_offsets = {
            let append_vec =
                AppendVec::new(&temp_file.path, true, total_stored_size, storage_access);
            // wrap AppendVec in ManuallyDrop to ensure we do not remove the backing file when dropped
            let append_vec = ManuallyDrop::new(append_vec);
            let slot = 77; // the specific slot does not matter
            let storable_accounts: Vec<_> = std::iter::zip(&pubkeys, &accounts).collect();
            let stored_accounts_info = append_vec
                .append_accounts(&(slot, storable_accounts.as_slice()), 0)
                .unwrap();
            append_vec.flush().unwrap();
            stored_accounts_info.offsets
        };

        // now open the append vec with the given storage access method
        // then get the account sizes to ensure they are correct
        let (append_vec, _) =
            AppendVec::new_from_file(&temp_file.path, total_stored_size, storage_access).unwrap();

        let account_sizes = append_vec
            .get_account_data_lens(account_offsets.as_slice())
            .iter()
            .map(|len| AppendVec::calculate_stored_size(*len))
            .sum::<usize>();
        assert_eq!(account_sizes, total_stored_size);
    }

    /// A helper function for testing different scenario for scan_*.
    ///
    /// `modify_fn` is used to (optionally) modify the append vec before checks are performed.
    /// `check_fn` performs the check for the scan.
    fn test_scan_helper(
        storage_access: StorageAccess,
        modify_fn: impl Fn(&PathBuf, usize) -> usize,
        check_fn: impl Fn(&AppendVec, &[Pubkey], &[usize], &[AccountSharedData]),
    ) {
        const NUM_ACCOUNTS: usize = 37;
        let pubkeys: Vec<_> = std::iter::repeat_with(solana_pubkey::new_rand)
            .take(NUM_ACCOUNTS)
            .collect();

        let mut rng = ChaChaRng::seed_from_u64(1337);
        let mut accounts = Vec::with_capacity(pubkeys.len());
        let mut total_stored_size = 0;
        for _ in &pubkeys {
            let lamports = rng.random();
            let data_len = rng.random_range(0..MAX_PERMITTED_DATA_LENGTH) as usize;
            let account = AccountSharedData::new(lamports, data_len, &Pubkey::default());
            accounts.push(account);
            total_stored_size += aligned_stored_size(data_len);
        }
        let accounts = accounts;
        let total_stored_size = total_stored_size;

        let temp_file = get_append_vec_path("test_scan");
        let account_offsets = {
            // wrap AppendVec in ManuallyDrop to ensure we do not remove the backing file when dropped
            let append_vec = ManuallyDrop::new(AppendVec::new(
                &temp_file.path,
                true,
                total_stored_size,
                storage_access,
            ));
            let slot = 42; // the specific slot does not matter
            let storable_accounts: Vec<_> = std::iter::zip(&pubkeys, &accounts).collect();
            let stored_accounts_info = append_vec
                .append_accounts(&(slot, storable_accounts.as_slice()), 0)
                .unwrap();
            append_vec.flush().unwrap();
            stored_accounts_info.offsets
        };

        let total_stored_size = modify_fn(&temp_file.path, total_stored_size);
        // now open the append vec with the given storage access method
        // then perform the scan and check it is correct
        let append_vec = ManuallyDrop::new(
            AppendVec::new_from_file_unchecked(&temp_file.path, total_stored_size, storage_access)
                .unwrap(),
        );

        check_fn(&append_vec, &pubkeys, &account_offsets, &accounts);
    }

    /// A helper fn to test `scan_pubkeys`.
    fn test_scan_pubkeys_helper(
        storage_access: StorageAccess,
        modify_fn: impl Fn(&PathBuf, usize) -> usize,
    ) {
        test_scan_helper(
            storage_access,
            modify_fn,
            |append_vec, pubkeys, _account_offsets, _accounts| {
                let mut i = 0;
                append_vec
                    .scan_pubkeys(|pubkey| {
                        assert_eq!(pubkey, pubkeys.get(i).unwrap());
                        i += 1;
                    })
                    .expect("must scan accounts storage");
                assert_eq!(i, pubkeys.len());
            },
        )
    }

    /// Test `scan_pubkey` for a valid account storage.
    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_scan_pubkeys(storage_access: StorageAccess) {
        test_scan_pubkeys_helper(storage_access, |_, size| size);
    }

    /// Test `scan_pubkey` for storage with incomplete account meta data.
    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_scan_pubkeys_incomplete_data(storage_access: StorageAccess) {
        test_scan_pubkeys_helper(storage_access, |path, size| {
            // Append 1 byte of data at the end of the storage file to simulate
            // incomplete account's meta data.
            let mut f = OpenOptions::new()
                .read(true)
                .append(true)
                .open(path)
                .unwrap();
            f.write_all(&[0xFF]).unwrap();
            size + 1
        });
    }

    /// Test `scan_pubkey` for storage which is missing the last account data
    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_scan_pubkeys_missing_account_data(storage_access: StorageAccess) {
        test_scan_pubkeys_helper(storage_access, |path, size| {
            let fake_stored_meta = StoredMeta {
                write_version_obsolete: 0,
                data_len: 100,
                pubkey: solana_pubkey::new_rand(),
            };
            let fake_account_meta = AccountMeta {
                lamports: 100,
                rent_epoch: 10,
                owner: solana_pubkey::new_rand(),
                executable: false,
            };

            let stored_meta_slice: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    (&fake_stored_meta as *const StoredMeta) as *const u8,
                    mem::size_of::<StoredMeta>(),
                )
            };
            let account_meta_slice: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    (&fake_account_meta as *const AccountMeta) as *const u8,
                    mem::size_of::<AccountMeta>(),
                )
            };

            let mut f = OpenOptions::new()
                .read(true)
                .append(true)
                .open(path)
                .unwrap();

            f.write_all(stored_meta_slice).unwrap();
            f.write_all(account_meta_slice).unwrap();

            size + mem::size_of::<StoredMeta>() + mem::size_of::<AccountMeta>()
        });
    }

    /// A helper fn to test scan_stored_accounts_no_data
    fn test_scan_stored_accounts_no_data_helper(
        storage_access: StorageAccess,
        modify_fn: impl Fn(&PathBuf, usize) -> usize,
    ) {
        test_scan_helper(
            storage_access,
            modify_fn,
            |append_vec, pubkeys, account_offsets, accounts| {
                let mut i = 0;
                append_vec
                    .scan_stored_accounts_no_data(|stored_account| {
                        let pubkey = pubkeys.get(i).unwrap();
                        let account = accounts.get(i).unwrap();
                        let offset = account_offsets.get(i).unwrap();

                        assert_eq!(
                            stored_account.stored_size,
                            aligned_stored_size(account.data().len()),
                        );
                        assert_eq!(stored_account.offset(), *offset);
                        assert_eq!(stored_account.pubkey(), pubkey);
                        assert_eq!(stored_account.lamports(), account.lamports());
                        assert_eq!(stored_account.data_len(), account.data().len() as u64);

                        i += 1;
                    })
                    .expect("must scan accounts storage");
                assert_eq!(i, accounts.len());
            },
        )
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_scan_stored_accounts_no_data(storage_access: StorageAccess) {
        test_scan_stored_accounts_no_data_helper(storage_access, |_, size| size);
    }

    /// Test `scan_stored_accounts_no_data` for storage with incomplete account meta data.
    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_scan_stored_accounts_no_data_incomplete_data(storage_access: StorageAccess) {
        test_scan_stored_accounts_no_data_helper(storage_access, |path, size| {
            // Append 1 byte of data at the end of the storage file to simulate
            // incomplete account's meta data.
            let mut f = OpenOptions::new()
                .read(true)
                .append(true)
                .open(path)
                .unwrap();
            f.write_all(&[0xFF]).unwrap();
            size + 1
        });
    }

    /// Test `scan_stored_accounts_no_data` for storage which is missing the last account data
    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_scan_stored_accounts_no_data_missing_account_data(storage_access: StorageAccess) {
        test_scan_stored_accounts_no_data_helper(storage_access, |path, size| {
            let fake_stored_meta = StoredMeta {
                write_version_obsolete: 0,
                data_len: 100,
                pubkey: solana_pubkey::new_rand(),
            };
            let fake_account_meta = AccountMeta {
                lamports: 100,
                rent_epoch: 10,
                owner: solana_pubkey::new_rand(),
                executable: false,
            };

            let stored_meta_slice: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    (&fake_stored_meta as *const StoredMeta) as *const u8,
                    mem::size_of::<StoredMeta>(),
                )
            };
            let account_meta_slice: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    (&fake_account_meta as *const AccountMeta) as *const u8,
                    mem::size_of::<AccountMeta>(),
                )
            };

            let mut f = OpenOptions::new()
                .read(true)
                .append(true)
                .open(path)
                .unwrap();

            f.write_all(stored_meta_slice).unwrap();
            f.write_all(account_meta_slice).unwrap();

            size + mem::size_of::<StoredMeta>() + mem::size_of::<AccountMeta>()
        });
    }

    /// A helper fn to test scan_accounts_stored_meta
    ///
    /// `modify_fn` is used to (optionally) modify the append vec before checks are performed.
    fn test_scan_accounts_stored_meta_helper(
        storage_access: StorageAccess,
        modify_fn: impl Fn(&PathBuf, usize) -> usize,
    ) {
        test_scan_helper(
            storage_access,
            modify_fn,
            |append_vec, pubkeys, account_offsets, accounts| {
                let mut reader = new_scan_accounts_reader();
                let mut i = 0;
                append_vec
                    .scan_accounts_stored_meta(&mut reader, |stored_account| {
                        let pubkey = pubkeys.get(i).unwrap();
                        let offset = account_offsets.get(i).unwrap();
                        let account = accounts.get(i).unwrap();

                        assert_eq!(stored_account.pubkey(), pubkey);
                        assert_eq!(stored_account.offset(), *offset);
                        assert!(accounts_equal(&stored_account, account));

                        i += 1;
                    })
                    .expect("must scan accounts storage");
                assert_eq!(i, accounts.len());
            },
        )
    }

    /// Test `scan_accounts_stored_meta` for a normal/good storage.
    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_scan_accounts_stored_meta(storage_access: StorageAccess) {
        test_scan_accounts_stored_meta_helper(storage_access, |_, size| size);
    }

    /// Test `scan_accounts_stored_meta` for a storage with incomplete account meta data.
    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_scan_accounts_stored_meta_incomplete_meta_data(storage_access: StorageAccess) {
        test_scan_accounts_stored_meta_helper(storage_access, |path, size| {
            // Append 1 byte of data at the end of the storage file to simulate
            // incomplete account's meta data.
            let mut f = OpenOptions::new()
                .read(true)
                .append(true)
                .open(path)
                .unwrap();
            f.write_all(&[0xFF]).unwrap();
            size + 1
        });
    }

    /// Test `scan_accounts_stored_meta` for a storage that is missing the last account data.
    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_scan_accounts_stored_meta_missing_account_data(storage_access: StorageAccess) {
        test_scan_accounts_stored_meta_helper(storage_access, |path, size| {
            let fake_stored_meta = StoredMeta {
                write_version_obsolete: 0,
                data_len: 100,
                pubkey: solana_pubkey::new_rand(),
            };
            let fake_account_meta = AccountMeta {
                lamports: 100,
                rent_epoch: 10,
                owner: solana_pubkey::new_rand(),
                executable: false,
            };

            let stored_meta_slice: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    (&fake_stored_meta as *const StoredMeta) as *const u8,
                    mem::size_of::<StoredMeta>(),
                )
            };
            let account_meta_slice: &[u8] = unsafe {
                std::slice::from_raw_parts(
                    (&fake_account_meta as *const AccountMeta) as *const u8,
                    mem::size_of::<AccountMeta>(),
                )
            };

            let mut f = OpenOptions::new()
                .read(true)
                .append(true)
                .open(path)
                .unwrap();

            f.write_all(stored_meta_slice).unwrap();
            f.write_all(account_meta_slice).unwrap();

            size + mem::size_of::<StoredMeta>() + mem::size_of::<AccountMeta>()
        });
    }

    // Test to make sure that `is_dirty` is tracked properly
    // * `reopen_as_readonly()` moves `is_dirty`
    // * `flush()` clears `is_dirty`
    #[test_matrix([false, true], [#[allow(deprecated)] StorageAccess::Mmap, StorageAccess::File])]
    fn test_is_dirty(begins_dirty: bool, storage_access: StorageAccess) {
        let file = get_append_vec_path("test_is_dirty");

        let mut av1 = AppendVec::new(&file.path, true, 1024 * 1024, storage_access);
        // don't delete the file when the AppendVec is dropped (let TempFile do it)
        *av1.remove_file_on_drop.get_mut() = false;

        // ensure the append vec begins not dirty
        assert!(!*av1.is_dirty.get_mut());

        if begins_dirty {
            av1.append_account_test(&create_test_account(10)).unwrap();
        }
        assert_eq!(*av1.is_dirty.get_mut(), begins_dirty);

        let mut av2 = av1.reopen_as_readonly_file_io().unwrap();
        // don't delete the file when the AppendVec is dropped (let TempFile do it)
        *av2.remove_file_on_drop.get_mut() = false;

        // ensure `is_dirty` is moved
        assert!(!*av1.is_dirty.get_mut());
        assert_eq!(*av2.is_dirty.get_mut(), begins_dirty);

        // ensure we can flush the new append vec
        assert!(av2.flush().is_ok());
        // and now should not be dirty
        assert!(!*av2.is_dirty.get_mut());

        // ensure we can flush the old append vec too
        assert!(av1.flush().is_ok());
        // and now should not be dirty
        assert!(!*av1.is_dirty.get_mut());
    }
}
