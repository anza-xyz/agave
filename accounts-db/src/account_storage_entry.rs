use {
    crate::{
        account_info::Offset,
        account_storage::stored_account_info::{StoredAccountInfo, StoredAccountInfoWithoutData},
        accounts_db::AccountsFileId,
        accounts_file::{AccountsFile, AccountsFileError, AccountsFileProvider},
        obsolete_accounts::ObsoleteAccounts,
    },
    agave_fs::buffered_reader::RequiredLenBufFileRead,
    solana_clock::Slot,
    solana_nohash_hasher::IntSet,
    std::{
        path::Path,
        sync::{
            RwLock, RwLockReadGuard,
            atomic::{AtomicUsize, Ordering},
        },
    },
};

/// Persistent storage structure holding the accounts
#[derive(Debug)]
pub struct AccountStorageEntry {
    id: AccountsFileId,

    slot: Slot,

    /// storage holding the accounts
    pub accounts: AccountsFile,

    /// The number of alive accounts in this storage
    pub(crate) num_alive_accounts: AtomicUsize,

    pub(crate) num_alive_bytes: AtomicUsize,

    /// offsets to zero-lamport accounts that have been removed from the accounts index entirely
    /// (a tombstone — carried forward to this storage by shrink). The index has no slot_list entry
    /// pointing at them; their bytes are retained only so an incremental snapshot taken after the
    /// latest full snapshot still observes the zero-lamport account and propagates the deletion.
    /// Shrink uses this list to recognize tombstone entries without needing to scan the index.
    tombstone_offsets: RwLock<IntSet<Offset>>,

    /// Obsolete Accounts. These are accounts that are still present in the storage
    /// but should be ignored during rebuild. They have been removed
    /// from the accounts index, so they will not be picked up by scan.
    /// Slot is the slot at which the account is no longer needed.
    /// Two scenarios cause an account entry to be marked obsolete
    /// 1. The account was rewritten to a newer slot
    /// 2. The account was set to zero lamports and is older than the last
    ///    full snapshot. In this case, slot is set to the snapshot slot
    pub(crate) obsolete_accounts: RwLock<ObsoleteAccounts>,
}

impl AccountStorageEntry {
    pub fn new(
        path: &Path,
        slot: Slot,
        id: AccountsFileId,
        file_size: u64,
        provider: AccountsFileProvider,
    ) -> Self {
        let tail = AccountsFile::file_name(slot, id);
        let path = Path::new(path).join(tail);
        let accounts = provider.new_writable(path, file_size);

        Self {
            id,
            slot,
            accounts,
            num_alive_accounts: AtomicUsize::new(0),
            num_alive_bytes: AtomicUsize::new(0),
            tombstone_offsets: RwLock::default(),
            obsolete_accounts: RwLock::default(),
        }
    }

    /// open a new instance of the storage that is readonly
    pub(crate) fn reopen_as_readonly(&self) -> Option<Self> {
        self.accounts.reopen_as_readonly().map(|accounts| Self {
            id: self.id,
            slot: self.slot,
            num_alive_accounts: AtomicUsize::new(self.count()),
            num_alive_bytes: AtomicUsize::new(self.alive_bytes()),
            accounts,
            tombstone_offsets: RwLock::new(self.tombstone_offsets.read().unwrap().clone()),
            obsolete_accounts: RwLock::new(self.obsolete_accounts.read().unwrap().clone()),
        })
    }

    pub fn new_existing(
        slot: Slot,
        id: AccountsFileId,
        accounts: AccountsFile,
        obsolete_accounts: ObsoleteAccounts,
    ) -> Self {
        Self {
            id,
            slot,
            accounts,
            num_alive_accounts: AtomicUsize::new(0),
            num_alive_bytes: AtomicUsize::new(0),
            tombstone_offsets: RwLock::default(),
            obsolete_accounts: RwLock::new(obsolete_accounts),
        }
    }

    /// Returns the number of alive accounts in this storage
    pub fn count(&self) -> usize {
        self.num_alive_accounts.load(Ordering::Acquire)
    }

    pub fn alive_bytes(&self) -> usize {
        self.num_alive_bytes.load(Ordering::Acquire)
    }

    /// Returns the accounts that were marked obsolete as of the passed in slot
    /// or earlier. Returned data includes the slots that the accounts were marked
    /// obsolete at
    pub fn obsolete_accounts_for_snapshots(&self, slot: Slot) -> ObsoleteAccounts {
        self.obsolete_accounts_read_lock()
            .obsolete_accounts_for_snapshots(slot)
    }

    /// Locks obsolete accounts with a read lock and returns the the accounts with the guard
    pub(crate) fn obsolete_accounts_read_lock(&self) -> RwLockReadGuard<'_, ObsoleteAccounts> {
        self.obsolete_accounts.read().unwrap()
    }

    /// Returns the number of bytes that were marked obsolete as of the passed
    /// in slot or earlier. If slot is None, then slot will be assumed to be the
    /// max root, and all obsolete bytes will be returned.
    pub fn get_obsolete_bytes(&self, slot: Option<Slot>) -> usize {
        let obsolete_bytes: usize = self
            .obsolete_accounts_read_lock()
            .filter_obsolete_accounts(slot)
            .map(|(offset, data_len)| {
                self.accounts
                    .calculate_stored_size(data_len)
                    .min(self.accounts.len() - offset as usize)
            })
            .sum();
        obsolete_bytes
    }

    /// Batch-insert tombstone offsets, taking the offsets lock once.
    /// Returns the number of offsets inserted.
    pub(crate) fn batch_insert_tombstone_offsets(
        &self,
        offsets: impl IntoIterator<Item = Offset>,
    ) -> usize {
        let mut tombstone_offsets = self.tombstone_offsets.write().unwrap();
        let mut num_inserted = 0;
        for offset in offsets {
            if tombstone_offsets.insert(offset) {
                num_inserted += 1;
            }
        }
        num_inserted
    }

    /// Locks the tombstone offset set with a read lock and returns it with the guard.
    pub(crate) fn tombstone_offsets_read_lock(&self) -> RwLockReadGuard<'_, IntSet<Offset>> {
        self.tombstone_offsets.read().unwrap()
    }

    /// Number of tombstone offsets in the storage.
    pub(crate) fn num_tombstones(&self) -> usize {
        self.tombstone_offsets.read().unwrap().len()
    }

    /// True if every alive account in this storage is a tombstone. Such a storage holds no live
    /// index entries (tombstones were removed from the index when created), so it is fully dead.
    pub(crate) fn has_only_tombstones(&self) -> bool {
        let num_tombstones = self.num_tombstones();
        num_tombstones > 0 && self.count() == num_tombstones
    }

    /// Return the "alive_bytes" minus the bytes of this storage's tombstones
    /// (zero-lamport accounts already purged from the index).
    pub(crate) fn alive_bytes_exclude_zero_lamport_accounts(&self) -> usize {
        let zero_lamport_dead_bytes = self
            .accounts
            .dead_bytes_due_to_zero_lamport_accounts(self.num_tombstones());
        self.alive_bytes().saturating_sub(zero_lamport_dead_bytes)
    }

    /// Returns the number of bytes used in this storage
    pub fn written_bytes(&self) -> u64 {
        self.accounts.len() as u64
    }

    pub fn has_accounts(&self) -> bool {
        self.count() > 0
    }

    pub fn slot(&self) -> Slot {
        self.slot
    }

    pub fn id(&self) -> AccountsFileId {
        self.id
    }

    pub fn flush(&self) -> Result<(), AccountsFileError> {
        self.accounts.flush()
    }

    /// Detach the on-disk file from this storage's lifetime; see
    /// [`AccountsFile::disable_remove_on_drop`].
    pub fn disable_remove_on_drop(&self) {
        self.accounts.disable_remove_on_drop();
    }

    pub(crate) fn add_accounts(&self, num_accounts: usize, num_bytes: usize) {
        self.num_alive_accounts
            .fetch_add(num_accounts, Ordering::Release);
        self.num_alive_bytes.fetch_add(num_bytes, Ordering::Release);
    }

    /// Removes `num_bytes` and `num_accounts` from the storage,
    /// and returns the remaining number of accounts.
    pub(crate) fn remove_accounts(&self, num_bytes: usize, num_accounts: usize) -> usize {
        let prev_num_alive_bytes = self.num_alive_bytes.fetch_sub(num_bytes, Ordering::Release);
        let prev_num_alive_accounts = self
            .num_alive_accounts
            .fetch_sub(num_accounts, Ordering::Release);

        // enforce invariant that we're not removing too many bytes or accounts
        assert!(
            num_bytes <= prev_num_alive_bytes && num_accounts <= prev_num_alive_accounts,
            "Too many bytes or accounts removed from storage! slot: {}, id: {}, initial num alive \
             bytes: {prev_num_alive_bytes}, initial num alive accounts: \
             {prev_num_alive_accounts}, num bytes removed: {num_bytes}, num accounts removed: \
             {num_accounts}",
            self.slot,
            self.id,
        );

        // SAFETY: subtraction is safe since we just asserted num_accounts <= prev_num_accounts
        prev_num_alive_accounts - num_accounts
    }

    /// Collect the offsets that should be excluded from scans
    fn excluded_offsets(&self) -> IntSet<Offset> {
        let mut offsets: IntSet<_> = self
            .obsolete_accounts_read_lock()
            .filter_obsolete_accounts(None)
            .map(|(offset, _)| offset)
            .collect();
        offsets.extend(self.tombstone_offsets_read_lock().iter().copied());
        offsets
    }

    /// Iterate over the alive accounts in this storage, excluding obsolete accounts and tombstones.
    /// The return value is the number of values excluded from the scan.
    pub(crate) fn scan_accounts<'a>(
        &'a self,
        reader: &mut impl RequiredLenBufFileRead<'a>,
        mut callback: impl for<'local> FnMut(Offset, StoredAccountInfo<'local>),
    ) -> Result<u64, AccountsFileError> {
        let excluded_offsets = self.excluded_offsets();
        let mut num_excluded = 0;
        self.accounts.scan_accounts(reader, |offset, account| {
            if excluded_offsets.contains(&offset) {
                num_excluded += 1;
                return;
            }
            callback(offset, account);
        })?;
        Ok(num_excluded)
    }

    /// Iterate over the alive accounts in this storage without reading data, excluding obsolete
    /// accounts and tombstones. The return value is the number of values excluded from the scan.
    pub(crate) fn scan_accounts_without_data(
        &self,
        mut callback: impl for<'local> FnMut(Offset, StoredAccountInfoWithoutData<'local>),
    ) -> Result<u64, AccountsFileError> {
        let excluded_offsets = self.excluded_offsets();
        let mut num_excluded = 0;
        self.accounts
            .scan_accounts_without_data(|offset, account| {
                if excluded_offsets.contains(&offset) {
                    num_excluded += 1;
                    return;
                }
                callback(offset, account);
            })?;
        Ok(num_excluded)
    }

    /// Returns the path to the underlying accounts storage file
    pub fn path(&self) -> &Path {
        self.accounts.path()
    }
}

#[cfg(test)]
impl AccountStorageEntry {
    // Function to modify the list in the account storage entry directly. Only intended for use in testing
    pub(crate) fn obsolete_accounts(&self) -> &RwLock<ObsoleteAccounts> {
        &self.obsolete_accounts
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*, crate::append_vec::new_scan_accounts_reader, solana_account::AccountSharedData,
        solana_pubkey::Pubkey, std::iter, tempfile::TempDir,
    };

    /// scan_accounts and scan_accounts_without_data each visit every account except those marked
    /// obsolete or recorded as a tombstone, and return the number of accounts excluded.
    #[test]
    fn test_scan_accounts_excludes_obsolete_and_tombstones() {
        let slot = 0;
        let temp_dir = TempDir::new().unwrap();
        let storage = AccountStorageEntry::new(
            temp_dir.path(),
            slot,
            0,
            1024 * 1024,
            AccountsFileProvider::AppendVec,
        );

        // Write five accounts and capture their offsets.
        let accounts: Vec<_> = iter::repeat_with(|| {
            (
                Pubkey::new_unique(),
                AccountSharedData::new(1, 10, &Pubkey::default()),
            )
        })
        .take(5)
        .collect();
        let offsets = storage
            .accounts
            .write_accounts(&(slot, &accounts[..]))
            .unwrap()
            .offsets;

        // Mark account 1 obsolete and record account 3 as a tombstone.
        let obsolete_offset = offsets[1];
        let tombstone_offset = offsets[3];
        let data_lens = storage.accounts.get_account_data_lens(&[obsolete_offset]);
        storage
            .obsolete_accounts()
            .write()
            .unwrap()
            .mark_accounts_obsolete(iter::once((obsolete_offset, data_lens[0])), slot);
        storage.batch_insert_tombstone_offsets([tombstone_offset]);

        // Scan and collect the accounts that were visited, in offset order.
        let mut reader = new_scan_accounts_reader();
        let mut visited = Vec::new();
        let num_excluded = storage
            .scan_accounts(&mut reader, |offset, account| {
                visited.push((offset, *account.pubkey()));
            })
            .unwrap();

        // Accounts 0, 2, and 4 are alive; 1 (obsolete) and 3 (tombstone) are excluded.
        assert_eq!(num_excluded, 2);
        let expected: Vec<_> = [0, 2, 4]
            .iter()
            .map(|&i| (offsets[i], accounts[i].0))
            .collect();
        assert_eq!(visited, expected);

        // scan_accounts_without_data excludes the same offsets from the same storage.
        let mut visited = Vec::new();
        let num_excluded = storage
            .scan_accounts_without_data(|offset, account| {
                visited.push((offset, *account.pubkey()));
            })
            .unwrap();
        assert_eq!(num_excluded, 2);
        assert_eq!(visited, expected);
    }
}
