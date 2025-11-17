//! trait for abstracting underlying storage of pubkey and account pairs to be written
use {
    crate::{
        account_storage::stored_account_info::StoredAccountInfo,
        accounts_db::{AccountFromStorage, AccountStorageEntry, AccountsDb},
        is_zero_lamport::IsZeroLamport,
        utils::create_account_shared_data,
    },
    solana_account::{AccountSharedData, ReadableAccount},
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    std::{
        cmp::Ordering,
        sync::{Arc, RwLock},
    },
};

/// hold a ref to an account to store. The account could be represented in memory a few different ways
#[derive(Debug, Copy, Clone)]
pub enum AccountForStorage<'a> {
    AddressAndAccount((&'a Pubkey, &'a AccountSharedData)),
    StoredAccountInfo(&'a StoredAccountInfo<'a>),
}

impl<'a> From<(&'a Pubkey, &'a AccountSharedData)> for AccountForStorage<'a> {
    fn from(source: (&'a Pubkey, &'a AccountSharedData)) -> Self {
        Self::AddressAndAccount(source)
    }
}

impl<'a> From<&'a StoredAccountInfo<'a>> for AccountForStorage<'a> {
    fn from(source: &'a StoredAccountInfo<'a>) -> Self {
        Self::StoredAccountInfo(source)
    }
}

impl IsZeroLamport for AccountForStorage<'_> {
    fn is_zero_lamport(&self) -> bool {
        self.lamports() == 0
    }
}

impl<'a> AccountForStorage<'a> {
    pub fn pubkey(&self) -> &'a Pubkey {
        match self {
            AccountForStorage::AddressAndAccount((pubkey, _account)) => pubkey,
            AccountForStorage::StoredAccountInfo(account) => account.pubkey(),
        }
    }

    pub fn take_account(&self) -> AccountSharedData {
        match self {
            AccountForStorage::AddressAndAccount((_pubkey, account)) => (*account).clone(),
            AccountForStorage::StoredAccountInfo(account) => create_account_shared_data(*account),
        }
    }
}

impl ReadableAccount for AccountForStorage<'_> {
    fn lamports(&self) -> u64 {
        match self {
            AccountForStorage::AddressAndAccount((_pubkey, account)) => account.lamports(),
            AccountForStorage::StoredAccountInfo(account) => account.lamports(),
        }
    }
    fn data(&self) -> &[u8] {
        match self {
            AccountForStorage::AddressAndAccount((_pubkey, account)) => account.data(),
            AccountForStorage::StoredAccountInfo(account) => account.data(),
        }
    }
    fn owner(&self) -> &Pubkey {
        match self {
            AccountForStorage::AddressAndAccount((_pubkey, account)) => account.owner(),
            AccountForStorage::StoredAccountInfo(account) => account.owner(),
        }
    }
    fn executable(&self) -> bool {
        match self {
            AccountForStorage::AddressAndAccount((_pubkey, account)) => account.executable(),
            AccountForStorage::StoredAccountInfo(account) => account.executable(),
        }
    }
    fn rent_epoch(&self) -> Epoch {
        match self {
            AccountForStorage::AddressAndAccount((_pubkey, account)) => account.rent_epoch(),
            AccountForStorage::StoredAccountInfo(account) => account.rent_epoch(),
        }
    }
    fn to_account_shared_data(&self) -> AccountSharedData {
        self.take_account()
    }
}

static DEFAULT_ACCOUNT_SHARED_DATA: std::sync::LazyLock<AccountSharedData> =
    std::sync::LazyLock::new(AccountSharedData::default);

#[derive(Default, Debug)]
pub struct StorableAccountsCacher {
    slot: Slot,
    storage: Option<Arc<AccountStorageEntry>>,
}

/// abstract access to pubkey, account, slot, target_slot of either:
/// a. (slot, &[&Pubkey, &ReadableAccount])
/// b. (slot, &[Pubkey, ReadableAccount])
/// c. (slot, &[&Pubkey, &ReadableAccount, Slot]) (we will use this later)
/// This trait avoids having to allocate redundant data when there is a duplicated slot parameter.
/// All legacy callers do not have a unique slot per account to store.
pub trait StorableAccounts<'a>: Sync {
    /// account at 'index'
    fn account<Ret>(
        &self,
        index: usize,
        callback: impl for<'local> FnMut(AccountForStorage<'local>) -> Ret,
    ) -> Ret;
    /// whether account at 'index' has zero lamports
    fn is_zero_lamport(&self, index: usize) -> bool;
    /// data length of account at 'index'
    fn data_len(&self, index: usize) -> usize;
    /// pubkey of account at 'index'
    fn pubkey(&self, index: usize) -> &Pubkey;
    /// None if account is zero lamports
    fn account_default_if_zero_lamport<Ret>(
        &self,
        index: usize,
        mut callback: impl for<'local> FnMut(AccountForStorage<'local>) -> Ret,
    ) -> Ret {
        // Calling `self.account` may be expensive if backed by disk storage.
        // Check if the account is zero lamports first.
        if self.is_zero_lamport(index) {
            callback(AccountForStorage::AddressAndAccount((
                self.pubkey(index),
                &DEFAULT_ACCOUNT_SHARED_DATA,
            )))
        } else {
            self.account(index, callback)
        }
    }
    // current slot for account at 'index'
    fn slot(&self, index: usize) -> Slot;
    /// slot that all accounts are to be written to
    fn target_slot(&self) -> Slot;
    /// true if no accounts to write
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// # accounts to write
    fn len(&self) -> usize;
    /// are there accounts from multiple slots
    /// only used for an assert
    fn contains_multiple_slots(&self) -> bool {
        false
    }
}

impl<'a: 'b, 'b> StorableAccounts<'a> for (Slot, &'b [(&'a Pubkey, &'a AccountSharedData)]) {
    fn account<Ret>(
        &self,
        index: usize,
        mut callback: impl for<'local> FnMut(AccountForStorage<'local>) -> Ret,
    ) -> Ret {
        callback((self.1[index].0, self.1[index].1).into())
    }
    fn is_zero_lamport(&self, index: usize) -> bool {
        self.1[index].1.is_zero_lamport()
    }
    fn data_len(&self, index: usize) -> usize {
        self.1[index].1.data().len()
    }
    fn pubkey(&self, index: usize) -> &Pubkey {
        self.1[index].0
    }
    fn slot(&self, _index: usize) -> Slot {
        // per-index slot is not unique per slot when per-account slot is not included in the source data
        self.target_slot()
    }
    fn target_slot(&self) -> Slot {
        self.0
    }
    fn len(&self) -> usize {
        self.1.len()
    }
}

impl<'a: 'b, 'b> StorableAccounts<'a> for (Slot, &'b [(Pubkey, AccountSharedData)]) {
    fn account<Ret>(
        &self,
        index: usize,
        mut callback: impl for<'local> FnMut(AccountForStorage<'local>) -> Ret,
    ) -> Ret {
        callback((&self.1[index].0, &self.1[index].1).into())
    }
    fn is_zero_lamport(&self, index: usize) -> bool {
        self.1[index].1.is_zero_lamport()
    }
    fn data_len(&self, index: usize) -> usize {
        self.1[index].1.data().len()
    }
    fn pubkey(&self, index: usize) -> &Pubkey {
        &self.1[index].0
    }
    fn slot(&self, _index: usize) -> Slot {
        // per-index slot is not unique per slot when per-account slot is not included in the source data
        self.target_slot()
    }
    fn target_slot(&self) -> Slot {
        self.0
    }
    fn len(&self) -> usize {
        self.1.len()
    }
}

/// holds slices of accounts being moved FROM a common source slot to 'target_slot'
pub struct StorableAccountsBySlot<'a> {
    target_slot: Slot,
    /// each element is (source slot, accounts moving FROM source slot)
    slots_and_accounts: &'a [(Slot, &'a [&'a AccountFromStorage])],

    /// This is calculated based off slots_and_accounts.
    /// cumulative offset of all account slices prior to this one
    /// starting_offsets[0] is the starting offset of slots_and_accounts[1]
    /// The starting offset of slots_and_accounts[0] is always 0
    starting_offsets_for_slots_accounts_slice: Vec<usize>,
    /// true if there is more than 1 slot represented in slots_and_accounts
    contains_multiple_slots: bool,
    /// total len of all accounts, across all slots_and_accounts
    len: usize,
    db: &'a AccountsDb,
    /// remember the last storage we looked up for a given slot
    cached_storage: RwLock<StorableAccountsCacher>,
}

impl<'a> StorableAccountsBySlot<'a> {
    /// each element of slots_and_accounts is (source slot, accounts moving FROM source slot)
    pub fn new(
        target_slot: Slot,
        slots_and_accounts: &'a [(Slot, &'a [&'a AccountFromStorage])],
        db: &'a AccountsDb,
    ) -> Self {
        let mut cumulative_len = 0usize;
        let mut starting_offsets = Vec::with_capacity(slots_and_accounts.len());
        let first_slot = slots_and_accounts
            .first()
            .map(|(slot, _)| *slot)
            .unwrap_or_default();
        let mut contains_multiple_slots = false;
        for (slot, accounts) in slots_and_accounts {
            cumulative_len = cumulative_len.saturating_add(accounts.len());
            starting_offsets.push(cumulative_len);
            contains_multiple_slots |= &first_slot != slot;
        }
        Self {
            target_slot,
            slots_and_accounts,
            starting_offsets_for_slots_accounts_slice: starting_offsets,
            contains_multiple_slots,
            len: cumulative_len,
            db,
            cached_storage: RwLock::default(),
        }
    }

    /// given an overall index for all accounts in self: return
    /// (slots_and_accounts index, index within those accounts)
    /// This implementation is optimized for performance by using binary search
    /// on the starting_offsets based on the assumption that the
    /// starting_offsets are always sorted.
    fn find_internal_index(&self, index: usize) -> (usize, usize) {
        // special case for when there is only one slot - just return the first index without searching.
        // This happens when we are just shrinking a single slot storage, which happens very often.
        if !self.contains_multiple_slots {
            return (0, index);
        }
        let upper_bound = self
            .starting_offsets_for_slots_accounts_slice
            .binary_search_by(|offset| match offset.cmp(&index) {
                Ordering::Equal => Ordering::Less,
                ord => ord,
            });
        match upper_bound {
            Ok(offset_index) => unreachable!("we shouldn't reach here: {}", offset_index),
            Err(offset_index) => {
                let prior_offset = if offset_index > 0 {
                    self.starting_offsets_for_slots_accounts_slice[offset_index - 1]
                } else {
                    0
                };
                (offset_index, index - prior_offset)
            }
        }
    }
}

impl<'a> StorableAccounts<'a> for StorableAccountsBySlot<'a> {
    fn account<Ret>(
        &self,
        index: usize,
        mut callback: impl for<'local> FnMut(AccountForStorage<'local>) -> Ret,
    ) -> Ret {
        let indexes = self.find_internal_index(index);
        let slot = self.slots_and_accounts[indexes.0].0;
        let data = self.slots_and_accounts[indexes.0].1[indexes.1];
        let offset = data.index_info.offset();
        let mut call_callback = |storage: &AccountStorageEntry| {
            storage
                .accounts
                .get_stored_account_callback(offset, |account| callback((&account).into()))
                .expect("account has to exist to be able to store it")
        };
        {
            let reader = self.cached_storage.read().unwrap();
            if reader.slot == slot {
                if let Some(storage) = reader.storage.as_ref() {
                    return call_callback(storage);
                }
            }
        }
        // cache doesn't contain a storage for this slot, so lookup storage in db.
        // note we do not use file id here. We just want the normal unshrunk storage for this slot.
        let storage = self
            .db
            .storage
            .get_slot_storage_entry_shrinking_in_progress_ok(slot)
            .expect("source slot has to have a storage to be able to store accounts");
        let ret = call_callback(&storage);
        let mut writer = self.cached_storage.write().unwrap();
        writer.slot = slot;
        writer.storage = Some(storage);
        ret
    }
    fn is_zero_lamport(&self, index: usize) -> bool {
        let indexes = self.find_internal_index(index);
        self.slots_and_accounts[indexes.0].1[indexes.1].is_zero_lamport()
    }
    fn data_len(&self, index: usize) -> usize {
        let indexes = self.find_internal_index(index);
        self.slots_and_accounts[indexes.0].1[indexes.1].data_len()
    }
    fn pubkey(&self, index: usize) -> &Pubkey {
        let indexes = self.find_internal_index(index);
        self.slots_and_accounts[indexes.0].1[indexes.1].pubkey()
    }
    fn slot(&self, index: usize) -> Slot {
        let indexes = self.find_internal_index(index);
        self.slots_and_accounts[indexes.0].0
    }
    fn target_slot(&self) -> Slot {
        self.target_slot
    }
    fn len(&self) -> usize {
        self.len
    }
    fn contains_multiple_slots(&self) -> bool {
        self.contains_multiple_slots
    }
}

#[cfg(test)]
pub mod tests {
    use {
        super::*,
        crate::{
            account_info::{AccountInfo, StorageLocation},
            accounts_db::{get_temp_accounts_paths, AccountStorageEntry},
            accounts_file::AccountsFileProvider,
        },
        rand::Rng,
        solana_account::{accounts_equal, AccountSharedData, WritableAccount},
        std::sync::Arc,
    };

    impl StorableAccountsBySlot<'_> {
        /// given an overall index for all accounts in self:
        /// return (slots_and_accounts index, index within those accounts)
        /// This is the baseline unoptimized implementation. It is not used in the validator. It
        /// is used for testing an optimized version - `find_internal_index`, in the actual implementation.
        fn find_internal_index_loop(&self, index: usize) -> (usize, usize) {
            // search offsets for the accounts slice that contains 'index'.
            // This could be a binary search.
            for (offset_index, next_offset) in self
                .starting_offsets_for_slots_accounts_slice
                .iter()
                .enumerate()
            {
                if next_offset > &index {
                    // offset of prior entry
                    let prior_offset = if offset_index > 0 {
                        self.starting_offsets_for_slots_accounts_slice
                            [offset_index.saturating_sub(1)]
                    } else {
                        0
                    };
                    return (offset_index, index - prior_offset);
                }
            }
            panic!("failed");
        }
    }

    /// this is used in the test for generation of storages
    /// this is no longer used in the validator.
    /// It is very tricky to get these right. There are already tests for this. It is likely worth it to leave this here for a while until everything has settled.
    impl<'a> StorableAccounts<'a> for (Slot, &'a [&'a StoredAccountInfo<'a>]) {
        fn account<Ret>(
            &self,
            index: usize,
            mut callback: impl for<'local> FnMut(AccountForStorage<'local>) -> Ret,
        ) -> Ret {
            let stored_account_info = self.1[index];
            let account_for_storage = AccountForStorage::StoredAccountInfo(stored_account_info);
            callback(account_for_storage)
        }
        fn is_zero_lamport(&self, index: usize) -> bool {
            self.1[index].is_zero_lamport()
        }
        fn data_len(&self, index: usize) -> usize {
            self.1[index].data.len()
        }
        fn pubkey(&self, index: usize) -> &Pubkey {
            self.1[index].pubkey()
        }
        fn slot(&self, _index: usize) -> Slot {
            // per-index slot is not unique per slot when per-account slot is not included in the source data
            self.0
        }
        fn target_slot(&self) -> Slot {
            self.0
        }
        fn len(&self) -> usize {
            self.1.len()
        }
    }

    /// this is no longer used. It is very tricky to get these right. There are already tests for this. It is likely worth it to leave this here for a while until everything has settled.
    impl<'a, T: ReadableAccount + Sync> StorableAccounts<'a> for (Slot, &'a [&'a (Pubkey, T)])
    where
        AccountForStorage<'a>: From<(&'a Pubkey, &'a T)>,
    {
        fn account<Ret>(
            &self,
            index: usize,
            mut callback: impl for<'local> FnMut(AccountForStorage<'local>) -> Ret,
        ) -> Ret {
            callback((&self.1[index].0, &self.1[index].1).into())
        }
        fn is_zero_lamport(&self, index: usize) -> bool {
            self.1[index].1.lamports() == 0
        }
        fn data_len(&self, index: usize) -> usize {
            self.1[index].1.data().len()
        }
        fn pubkey(&self, index: usize) -> &Pubkey {
            &self.1[index].0
        }
        fn slot(&self, _index: usize) -> Slot {
            // per-index slot is not unique per slot when per-account slot is not included in the source data
            self.target_slot()
        }
        fn target_slot(&self) -> Slot {
            self.0
        }
        fn len(&self) -> usize {
            self.1.len()
        }
    }

    /// this is no longer used. It is very tricky to get these right. There are already tests for this. It is likely worth it to leave this here for a while until everything has settled.
    /// this tuple contains a single different source slot that applies to all accounts
    /// accounts are StoredAccountInfo
    impl<'a> StorableAccounts<'a> for (Slot, &'a [&'a StoredAccountInfo<'a>], Slot) {
        fn account<Ret>(
            &self,
            index: usize,
            mut callback: impl for<'local> FnMut(AccountForStorage<'local>) -> Ret,
        ) -> Ret {
            let stored_account_info = self.1[index];
            let account_for_storage = AccountForStorage::StoredAccountInfo(stored_account_info);
            callback(account_for_storage)
        }
        fn is_zero_lamport(&self, index: usize) -> bool {
            self.1[index].is_zero_lamport()
        }
        fn data_len(&self, index: usize) -> usize {
            self.1[index].data.len()
        }
        fn pubkey(&self, index: usize) -> &Pubkey {
            self.1[index].pubkey()
        }
        fn slot(&self, _index: usize) -> Slot {
            // same other slot for all accounts
            self.2
        }
        fn target_slot(&self) -> Slot {
            self.0
        }
        fn len(&self) -> usize {
            self.1.len()
        }
    }

    fn compare<'a>(a: &impl StorableAccounts<'a>, b: &impl StorableAccounts<'a>) {
        assert_eq!(a.target_slot(), b.target_slot());
        assert_eq!(a.len(), b.len());
        assert_eq!(a.is_empty(), b.is_empty());
        (0..a.len()).for_each(|i| {
            b.account(i, |account| {
                a.account(i, |account_a| {
                    assert_eq!(account_a.pubkey(), account.pubkey());
                    assert!(accounts_equal(&account_a, &account));
                });
            });
        })
    }

    #[test]
    fn test_contains_multiple_slots() {
        let db = AccountsDb::new_single_for_tests();
        let slot = 0;
        let storage_id = 0; // does not matter
        let offset = 0; // does not matter
        let account_from_storage = AccountFromStorage {
            index_info: AccountInfo::new(
                StorageLocation::AppendVec(storage_id, offset),
                false, // does not matter
            ),
            data_len: 7, // does not matter
            pubkey: Pubkey::new_unique(),
        };

        let accounts = [&account_from_storage, &account_from_storage];
        let accounts2 = [(slot, &accounts[..])];
        let test3 = StorableAccountsBySlot::new(slot, &accounts2[..], &db);
        assert!(!test3.contains_multiple_slots());
    }

    #[test]
    fn test_storable_accounts() {
        let max_slots = 3_u64;
        for target_slot in 0..max_slots {
            for entries in 0..2 {
                for starting_slot in 0..max_slots {
                    let db = AccountsDb::new_single_for_tests();
                    let mut raw = Vec::new();
                    let mut raw2 = Vec::new();
                    let mut raw4 = Vec::new();
                    for entry in 0..entries {
                        let pk = Pubkey::from([entry; 32]);
                        let account = AccountSharedData::create(
                            (entry as u64) * starting_slot,
                            Vec::default(),
                            Pubkey::default(),
                            false,
                            0,
                        );

                        raw.push((pk, account.clone(), starting_slot % max_slots));
                    }
                    for entry in 0..entries {
                        let raw = &raw[entry as usize];
                        raw2.push(StoredAccountInfo {
                            pubkey: &raw.0,
                            lamports: raw.1.lamports(),
                            owner: raw.1.owner(),
                            data: raw.1.data(),
                            executable: raw.1.executable(),
                            rent_epoch: raw.1.rent_epoch(),
                        });
                        raw4.push((raw.0, raw.1.clone()));
                    }
                    let raw2_accounts_from_storage: Vec<_> = raw2
                        .iter()
                        .map(|account| {
                            let storage_id = 0; // does not matter
                            let offset = 0; // does not matter
                            AccountFromStorage {
                                index_info: AccountInfo::new(
                                    StorageLocation::AppendVec(storage_id, offset),
                                    account.is_zero_lamport(),
                                ),
                                data_len: account.data.len() as u64,
                                pubkey: *account.pubkey,
                            }
                        })
                        .collect();

                    let mut two = Vec::new();
                    let mut three = Vec::new();
                    let mut three_accounts_from_storage_byval = Vec::new();
                    let mut four_pubkey_and_account_value = Vec::new();
                    raw.iter()
                        .zip(
                            raw2.iter()
                                .zip(raw4.iter().zip(raw2_accounts_from_storage.iter())),
                        )
                        .for_each(|(raw, (raw2, (raw4, raw2_accounts_from_storage)))| {
                            two.push((&raw.0, &raw.1)); // 2 item tuple
                            three.push(raw2);
                            three_accounts_from_storage_byval.push(*raw2_accounts_from_storage);
                            four_pubkey_and_account_value.push(raw4);
                        });
                    let test2 = (target_slot, &two[..]);
                    let test4 = (target_slot, &four_pubkey_and_account_value[..]);

                    let source_slot = starting_slot % max_slots;

                    let storage = setup_sample_storage(&db, source_slot);
                    // store the accounts so they can be looked up later in `db`
                    if let Some(offsets) = storage
                        .accounts
                        .write_accounts(&(source_slot, &three[..]), 0)
                    {
                        three_accounts_from_storage_byval
                            .iter_mut()
                            .zip(offsets.offsets.iter())
                            .for_each(|(account, offset)| {
                                account.index_info = AccountInfo::new(
                                    StorageLocation::AppendVec(0, *offset),
                                    account.is_zero_lamport(),
                                )
                            });
                    }
                    let three_accounts_from_storage =
                        three_accounts_from_storage_byval.iter().collect::<Vec<_>>();

                    let accounts_with_slots = vec![(source_slot, &three_accounts_from_storage[..])];
                    let test3 = StorableAccountsBySlot::new(target_slot, &accounts_with_slots, &db);
                    let old_slot = starting_slot;
                    let for_slice = [(old_slot, &three_accounts_from_storage[..])];
                    let test_moving_slots2 =
                        StorableAccountsBySlot::new(target_slot, &for_slice, &db);
                    compare(&test2, &test3);
                    compare(&test2, &test4);
                    compare(&test2, &test_moving_slots2);
                    for (i, raw) in raw.iter().enumerate() {
                        test3.account(i, |account| {
                            assert_eq!(raw.0, *account.pubkey());
                            assert!(accounts_equal(&raw.1, &account));
                        });
                        assert_eq!(raw.2, test3.slot(i));
                        assert_eq!(target_slot, test4.slot(i));
                        assert_eq!(target_slot, test2.slot(i));
                        assert_eq!(old_slot, test_moving_slots2.slot(i));
                    }
                    assert_eq!(target_slot, test3.target_slot());
                    assert_eq!(target_slot, test4.target_slot());
                    assert_eq!(target_slot, test_moving_slots2.target_slot());
                    assert!(!test2.contains_multiple_slots());
                    assert!(!test4.contains_multiple_slots());
                    assert_eq!(test3.contains_multiple_slots(), entries > 1);
                }
            }
        }
    }

    fn setup_sample_storage(db: &AccountsDb, slot: Slot) -> Arc<AccountStorageEntry> {
        let id = 2;
        let file_size = 10_000;
        let (_temp_dirs, paths) = get_temp_accounts_paths(1).unwrap();
        let data = AccountStorageEntry::new(
            &paths[0],
            slot,
            id,
            file_size,
            AccountsFileProvider::AppendVec,
            db.storage_access(),
        );
        let storage = Arc::new(data);
        db.storage.insert(slot, storage.clone());
        storage
    }

    #[test]
    fn test_storable_accounts_by_slot() {
        for entries in 0..6 {
            let mut raw = Vec::new();
            let mut raw2 = Vec::new();
            for entry in 0..entries {
                let pk = Pubkey::from([entry; 32]);
                let account = AccountSharedData::create(
                    entry as u64,
                    Vec::default(),
                    Pubkey::default(),
                    false,
                    0,
                );
                raw.push((pk, account.clone()));
            }

            for entry in 0..entries {
                let raw = &raw[entry as usize];
                raw2.push(StoredAccountInfo {
                    pubkey: &raw.0,
                    lamports: raw.1.lamports(),
                    owner: raw.1.owner(),
                    data: raw.1.data(),
                    executable: raw.1.executable(),
                    rent_epoch: raw.1.rent_epoch(),
                });
            }

            let raw2_accounts_from_storage: Vec<_> = raw2
                .iter()
                .map(|account| {
                    let storage_id = 0; // does not matter
                    let offset = 0; // does not matter
                    AccountFromStorage {
                        index_info: AccountInfo::new(
                            StorageLocation::AppendVec(storage_id, offset),
                            account.is_zero_lamport(),
                        ),
                        data_len: account.data.len() as u64,
                        pubkey: *account.pubkey,
                    }
                })
                .collect();
            let raw2_refs = raw2.iter().collect::<Vec<_>>();

            // enumerate through permutations of # entries (ie. accounts) in each slot. Each one is 0..=entries.
            for entries0 in 0..=entries {
                let remaining1 = entries.saturating_sub(entries0);
                for entries1 in 0..=remaining1 {
                    let remaining2 = entries.saturating_sub(entries0 + entries1);
                    for entries2 in 0..=remaining2 {
                        let db = AccountsDb::new_single_for_tests();
                        let remaining3 = entries.saturating_sub(entries0 + entries1 + entries2);
                        let entries_by_level = [entries0, entries1, entries2, remaining3];
                        let mut overall_index = 0;
                        let mut expected_slots = Vec::default();
                        let slots_and_accounts_byval = entries_by_level
                            .iter()
                            .enumerate()
                            .filter_map(|(slot, count)| {
                                let slot = slot as Slot;
                                let count = *count as usize;
                                (overall_index < raw2.len()).then(|| {
                                    let range = overall_index..(overall_index + count);
                                    let mut result =
                                        raw2_accounts_from_storage[range.clone()].to_vec();
                                    // store the accounts so they can be looked up later in `db`
                                    let storage = setup_sample_storage(&db, slot);
                                    if let Some(offsets) = storage
                                        .accounts
                                        .write_accounts(&(slot, &raw2_refs[range.clone()]), 0)
                                    {
                                        result.iter_mut().zip(offsets.offsets.iter()).for_each(
                                            |(account, offset)| {
                                                account.index_info = AccountInfo::new(
                                                    StorageLocation::AppendVec(0, *offset),
                                                    account.is_zero_lamport(),
                                                )
                                            },
                                        );
                                    }

                                    range.for_each(|_| expected_slots.push(slot));
                                    overall_index += count;
                                    (slot, result)
                                })
                            })
                            .collect::<Vec<_>>();
                        let slots_and_accounts_ref1 = slots_and_accounts_byval
                            .iter()
                            .map(|(slot, accounts)| (*slot, accounts.iter().collect::<Vec<_>>()))
                            .collect::<Vec<_>>();
                        let slots_and_accounts = slots_and_accounts_ref1
                            .iter()
                            .map(|(slot, accounts)| (*slot, &accounts[..]))
                            .collect::<Vec<_>>();

                        let storable =
                            StorableAccountsBySlot::new(99, &slots_and_accounts[..], &db);
                        assert_eq!(99, storable.target_slot());
                        assert_eq!(entries0 != entries, storable.contains_multiple_slots());
                        (0..entries).for_each(|index| {
                            let index = index as usize;
                            let mut called = false;
                            storable.account(index, |account| {
                                called = true;
                                assert!(accounts_equal(&account, &raw2[index]));
                                assert_eq!(account.pubkey(), raw2[index].pubkey());
                            });
                            assert!(called);
                            assert_eq!(storable.slot(index), expected_slots[index]);
                        })
                    }
                }
            }
        }
    }

    #[test]
    fn test_find_internal_index() {
        let db = AccountsDb::new_single_for_tests();
        let storage_id = 0; // does not matter
        let offset = 0; // does not matter
        let account = AccountSharedData::default();
        let account_from_storage = AccountFromStorage {
            index_info: AccountInfo::new(
                StorageLocation::AppendVec(storage_id, offset),
                account.is_zero_lamport(),
            ),
            data_len: account.data().len() as u64,
            pubkey: Pubkey::new_unique(),
        };

        let mut slot_accounts = Vec::new();
        let mut all_accounts = Vec::new();
        let mut total = 0;
        let num_slots = 10_u64;
        // generate accounts for 10 slots
        // each slot has a random number of accounts, between 1 and 10
        for _slot in 0..num_slots {
            // generate random accounts per slot
            let n = rand::rng().random_range(1..10);
            total += n;
            let accounts = (0..n).map(|_| &account_from_storage).collect::<Vec<_>>();
            all_accounts.push(accounts);
        }
        for slot in 0..num_slots {
            slot_accounts.push((slot, &all_accounts[slot as usize][..]));
        }
        let storable_accounts = StorableAccountsBySlot::new(0, &slot_accounts[..], &db);
        // check that the optimized version is correct by comparing it to the unoptimized version
        for i in 0..total {
            let (slot_index, account_index) = storable_accounts.find_internal_index_loop(i);
            let (slot_index2, account_index2) = storable_accounts.find_internal_index(i);
            assert_eq!(slot_index, slot_index2);
            assert_eq!(account_index, account_index2);
        }
    }
}
