use {
    crate::{
        account_info::Offset,
        accounts_db::AccountsFileId,
        accounts_file::{AccountsFile, AccountsFileError, AccountsFileProvider, StorageAccess},
        obsolete_accounts::ObsoleteAccounts,
    },
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
    pub(crate) id: AccountsFileId,

    pub(crate) slot: Slot,

    /// storage holding the accounts
    pub accounts: AccountsFile,

    /// The number of alive accounts in this storage
    pub(crate) count: AtomicUsize,

    pub(crate) alive_bytes: AtomicUsize,

    /// offsets to accounts that are zero lamport single ref stored in this
    /// storage. These are still alive. But, shrink will be able to remove them.
    ///
    /// NOTE: It's possible that one of these zero lamport single ref accounts
    /// could be written in a new transaction (and later rooted & flushed) and a
    /// later clean runs and marks this account dead before this storage gets a
    /// chance to be shrunk, thus making the account dead in both "alive_bytes"
    /// and as a zero lamport single ref. If this happens, we will count this
    /// account as "dead" twice. However, this should be fine. It just makes
    /// shrink more likely to visit this storage.
    zero_lamport_single_ref_offsets: RwLock<IntSet<Offset>>,

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
        storage_access: StorageAccess,
    ) -> Self {
        let tail = AccountsFile::file_name(slot, id);
        let path = Path::new(path).join(tail);
        let accounts = provider.new_writable(path, file_size, storage_access);

        Self {
            id,
            slot,
            accounts,
            count: AtomicUsize::new(0),
            alive_bytes: AtomicUsize::new(0),
            zero_lamport_single_ref_offsets: RwLock::default(),
            obsolete_accounts: RwLock::default(),
        }
    }

    /// open a new instance of the storage that is readonly
    pub(crate) fn reopen_as_readonly(&self, storage_access: StorageAccess) -> Option<Self> {
        if storage_access != StorageAccess::File {
            // if we are only using mmap, then no reason to re-open
            return None;
        }

        self.accounts.reopen_as_readonly().map(|accounts| Self {
            id: self.id,
            slot: self.slot,
            count: AtomicUsize::new(self.count()),
            alive_bytes: AtomicUsize::new(self.alive_bytes()),
            accounts,
            zero_lamport_single_ref_offsets: RwLock::new(
                self.zero_lamport_single_ref_offsets.read().unwrap().clone(),
            ),
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
            count: AtomicUsize::new(0),
            alive_bytes: AtomicUsize::new(0),
            zero_lamport_single_ref_offsets: RwLock::default(),
            obsolete_accounts: RwLock::new(obsolete_accounts),
        }
    }

    /// Returns the number of alive accounts in this storage
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    pub fn alive_bytes(&self) -> usize {
        self.alive_bytes.load(Ordering::Acquire)
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
                    .min(self.accounts.len() - offset)
            })
            .sum();
        obsolete_bytes
    }

    /// Return true if offset is "new" and inserted successfully. Otherwise,
    /// return false if the offset exists already.
    pub(crate) fn insert_zero_lamport_single_ref_account_offset(&self, offset: usize) -> bool {
        let mut zero_lamport_single_ref_offsets =
            self.zero_lamport_single_ref_offsets.write().unwrap();
        zero_lamport_single_ref_offsets.insert(offset)
    }

    /// Insert offsets into the zero lamport single ref account offset set.
    /// Return the number of new offsets that were inserted.
    pub(crate) fn batch_insert_zero_lamport_single_ref_account_offsets(
        &self,
        offsets: &[Offset],
    ) -> u64 {
        let mut zero_lamport_single_ref_offsets =
            self.zero_lamport_single_ref_offsets.write().unwrap();
        let mut count = 0;
        for offset in offsets {
            if zero_lamport_single_ref_offsets.insert(*offset) {
                count += 1;
            }
        }
        count
    }

    /// Return the number of zero_lamport_single_ref accounts in the storage.
    pub(crate) fn num_zero_lamport_single_ref_accounts(&self) -> usize {
        self.zero_lamport_single_ref_offsets.read().unwrap().len()
    }

    /// Return the "alive_bytes" minus "zero_lamport_single_ref_accounts bytes".
    pub(crate) fn alive_bytes_exclude_zero_lamport_single_ref_accounts(&self) -> usize {
        let zero_lamport_dead_bytes = self
            .accounts
            .dead_bytes_due_to_zero_lamport_single_ref(self.num_zero_lamport_single_ref_accounts());
        self.alive_bytes().saturating_sub(zero_lamport_dead_bytes)
    }

    /// Returns the number of bytes used in this storage
    pub fn written_bytes(&self) -> u64 {
        self.accounts.len() as u64
    }

    /// Returns the number of bytes, not accounts, this storage can hold
    pub fn capacity(&self) -> u64 {
        self.accounts.capacity()
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

    pub(crate) fn add_accounts(&self, num_accounts: usize, num_bytes: usize) {
        self.count.fetch_add(num_accounts, Ordering::Release);
        self.alive_bytes.fetch_add(num_bytes, Ordering::Release);
    }

    /// Removes `num_bytes` and `num_accounts` from the storage,
    /// and returns the remaining number of accounts.
    pub(crate) fn remove_accounts(&self, num_bytes: usize, num_accounts: usize) -> usize {
        let prev_alive_bytes = self.alive_bytes.fetch_sub(num_bytes, Ordering::Release);
        let prev_count = self.count.fetch_sub(num_accounts, Ordering::Release);

        // enforce invariant that we're not removing too many bytes or accounts
        assert!(
            num_bytes <= prev_alive_bytes && num_accounts <= prev_count,
            "Too many bytes or accounts removed from storage! slot: {}, id: {}, initial alive \
             bytes: {prev_alive_bytes}, initial num accounts: {prev_count}, num bytes removed: \
             {num_bytes}, num accounts removed: {num_accounts}",
            self.slot,
            self.id,
        );

        // SAFETY: subtraction is safe since we just asserted num_accounts <= prev_num_accounts
        prev_count - num_accounts
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
