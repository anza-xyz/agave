//! Manage the map of slot -> append vec

use {
    crate::accounts_db::{AccountStorageEntry, AccountsFileId},
    dashmap::DashMap,
    rand::seq::SliceRandom,
    rayon::iter::{IndexedParallelIterator, IntoParallelRefIterator, ParallelIterator},
    solana_clock::Slot,
    solana_nohash_hasher::IntMap,
    std::{
        ops::{Index, Range},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, RwLock,
        },
    },
};

pub mod stored_account_info;

pub type AccountStorageMap = DashMap<Slot, Arc<AccountStorageEntry>>;

#[derive(Default, Debug)]
pub struct AccountStorage {
    /// map from Slot -> the single append vec for the slot
    map: AccountStorageMap,
    /// while shrink is operating on a slot, there can be 2 append vecs active for that slot
    /// Once the index has been updated to only refer to the new append vec, the single entry for the slot in 'map' can be updated.
    /// Entries in 'shrink_in_progress_map' can be found by 'get_account_storage_entry'
    shrink_in_progress_map: RwLock<IntMap<Slot, Arc<AccountStorageEntry>>>,
}

impl AccountStorage {
    /// Return the append vec in 'slot' and with id='store_id'.
    /// can look in 'map' and 'shrink_in_progress_map' to find the specified append vec
    /// when shrinking begins, shrinking_in_progress is called.
    /// This fn looks in 'map' first, then in 'shrink_in_progress_map', then in 'map' again because
    /// 'shrinking_in_progress' first inserts the new append vec into 'shrink_in_progress_map'
    /// Then, when 'shrink_in_progress' is dropped,
    /// the old append vec is replaced in 'map' with the new append vec
    /// then the new append vec is dropped from 'shrink_in_progress_map'.
    /// So, it is possible for a race with this fn and dropping 'shrink_in_progress'.
    /// Callers to this function have 2 choices:
    /// 1. hold the account index read lock for the pubkey so that the account index entry cannot be changed prior to or during this call. (scans do this)
    /// 2. expect to be ready to start over and read the index again if this function returns None
    ///
    /// Operations like shrinking or write cache flushing may have updated the index between when the caller read the index and called this function to
    /// load from the append vec specified in the index.
    ///
    /// In practice, this fn will return the entry from the map in the very first lookup unless a shrink is in progress.
    /// The third lookup will only be called if a requesting thread exactly interposes itself between the 2 map manipulations in the drop of 'shrink_in_progress'.
    pub(crate) fn get_account_storage_entry(
        &self,
        slot: Slot,
        store_id: AccountsFileId,
    ) -> Option<Arc<AccountStorageEntry>> {
        let lookup_in_map = || {
            self.map.get(&slot).and_then(|entry| {
                (entry.value().id() == store_id).then_some(Arc::clone(entry.value()))
            })
        };

        lookup_in_map()
            .or_else(|| {
                self.shrink_in_progress_map
                    .read()
                    .unwrap()
                    .get(&slot)
                    .and_then(|entry| (entry.id() == store_id).then(|| Arc::clone(entry)))
            })
            .or_else(lookup_in_map)
    }

    /// returns true if shrink in progress is NOT active
    pub(crate) fn no_shrink_in_progress(&self) -> bool {
        self.shrink_in_progress_map.read().unwrap().is_empty()
    }

    /// return the append vec for 'slot' if it exists
    /// This is only ever called when shrink is not possibly running and there is a max of 1 append vec per slot.
    pub fn get_slot_storage_entry(&self, slot: Slot) -> Option<Arc<AccountStorageEntry>> {
        assert!(
            self.no_shrink_in_progress(),
            "shrink is in progress! slots: {:?}",
            self.shrink_in_progress_map.read().unwrap().keys(),
        );
        self.get_slot_storage_entry_shrinking_in_progress_ok(slot)
    }

    pub(super) fn all_storages(&self) -> Vec<Arc<AccountStorageEntry>> {
        assert!(
            self.no_shrink_in_progress(),
            "shrink is in progress! slots: {:?}",
            self.shrink_in_progress_map.read().unwrap().keys(),
        );
        self.map
            .iter()
            .map(|item| Arc::clone(item.value()))
            .collect()
    }

    pub(crate) fn replace_storage_with_equivalent(
        &self,
        slot: Slot,
        storage: Arc<AccountStorageEntry>,
    ) {
        assert_eq!(storage.slot(), slot);
        if let Some(mut existing_storage) = self.map.get_mut(&slot) {
            assert_eq!(slot, existing_storage.value().slot());
            *existing_storage.value_mut() = storage;
        }
    }

    /// return the append vec for 'slot' if it exists
    pub(crate) fn get_slot_storage_entry_shrinking_in_progress_ok(
        &self,
        slot: Slot,
    ) -> Option<Arc<AccountStorageEntry>> {
        self.map.get(&slot).map(|entry| Arc::clone(entry.value()))
    }

    pub(crate) fn all_slots(&self) -> Vec<Slot> {
        assert!(
            self.no_shrink_in_progress(),
            "shrink is in progress! slots: {:?}",
            self.shrink_in_progress_map.read().unwrap().keys(),
        );
        self.map.iter().map(|iter_item| *iter_item.key()).collect()
    }

    /// returns true if there is no entry for 'slot'
    #[cfg(test)]
    pub(crate) fn is_empty_entry(&self, slot: Slot) -> bool {
        assert!(
            self.no_shrink_in_progress(),
            "shrink is in progress! slots: {:?}",
            self.shrink_in_progress_map.read().unwrap().keys(),
        );
        self.map.get(&slot).is_none()
    }

    /// initialize the storage map to 'all_storages'
    pub fn initialize(&mut self, all_storages: AccountStorageMap) {
        assert!(self.map.is_empty());
        assert!(
            self.no_shrink_in_progress(),
            "shrink is in progress! slots: {:?}",
            self.shrink_in_progress_map.read().unwrap().keys(),
        );
        self.map = all_storages;
    }

    /// remove the append vec at 'slot'
    /// returns the current contents
    pub(crate) fn remove(
        &self,
        slot: &Slot,
        shrink_can_be_active: bool,
    ) -> Option<Arc<AccountStorageEntry>> {
        assert!(shrink_can_be_active || self.shrink_in_progress_map.read().unwrap().is_empty());
        self.map.remove(slot).map(|(_, storage)| storage)
    }

    /// iterate through all (slot, append-vec)
    pub(crate) fn iter(&self) -> AccountStorageIter<'_> {
        assert!(
            self.no_shrink_in_progress(),
            "shrink is in progress! slots: {:?}",
            self.shrink_in_progress_map.read().unwrap().keys(),
        );
        AccountStorageIter::new(self)
    }

    pub(crate) fn insert(&self, store: Arc<AccountStorageEntry>) {
        assert!(
            self.no_shrink_in_progress(),
            "shrink is in progress! slots: {:?}",
            self.shrink_in_progress_map.read().unwrap().keys(),
        );
        assert!(self.map.insert(store.slot, store).is_none());
    }

    /// called when shrinking begins on a slot and append vec.
    /// When 'ShrinkInProgress' is dropped by caller, the old store will be replaced with 'new_store' in the storage map.
    /// Fails if there are no existing stores at the slot.
    /// 'new_store' will be replacing the current store at 'slot' in 'map'
    /// So, insert 'new_store' into 'shrink_in_progress_map'.
    /// This allows tx processing loads to find the items in 'shrink_in_progress_map' after the index is updated and item is now located in 'new_store'.
    pub(crate) fn shrinking_in_progress(
        &self,
        slot: Slot,
        new_store: Arc<AccountStorageEntry>,
    ) -> ShrinkInProgress<'_> {
        let shrinking_store = Arc::clone(
            self.map
                .get(&slot)
                .expect("no pre-existing storage for shrinking slot")
                .value(),
        );

        // insert 'new_store' into 'shrink_in_progress_map'
        assert!(
            self.shrink_in_progress_map
                .write()
                .unwrap()
                .insert(slot, Arc::clone(&new_store))
                .is_none(),
            "duplicate call"
        );

        ShrinkInProgress {
            storage: self,
            slot,
            new_store,
            old_store: shrinking_store,
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns the (slot, storage) tuples where `predicate` returns `true`
    ///
    /// This function is useful when not all storages are desired,
    /// as storages are only Arc::cloned if they pass the predicate.
    ///
    /// # Panics
    ///
    /// Panics if `shrink` is in progress.
    pub fn get_if(
        &self,
        predicate: impl Fn(&Slot, &AccountStorageEntry) -> bool,
    ) -> Box<[(Slot, Arc<AccountStorageEntry>)]> {
        assert!(
            self.no_shrink_in_progress(),
            "shrink is in progress! slots: {:?}",
            self.shrink_in_progress_map.read().unwrap().keys(),
        );
        self.map
            .iter()
            .filter_map(|entry| {
                let slot = entry.key();
                let storage = entry.value();
                predicate(slot, storage).then(|| (*slot, Arc::clone(storage)))
            })
            .collect()
    }
}

/// iterate contents of AccountStorage without exposing internals
pub struct AccountStorageIter<'a> {
    iter: dashmap::iter::Iter<'a, Slot, Arc<AccountStorageEntry>>,
}

impl<'a> AccountStorageIter<'a> {
    pub fn new(storage: &'a AccountStorage) -> Self {
        Self {
            iter: storage.map.iter(),
        }
    }
}

impl Iterator for AccountStorageIter<'_> {
    type Item = (Slot, Arc<AccountStorageEntry>);

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(entry) = self.iter.next() {
            let slot = entry.key();
            let store = entry.value();
            return Some((*slot, Arc::clone(store)));
        }
        None
    }
}

/// exists while there is a shrink in progress
/// keeps track of the 'new_store' being created and the 'old_store' being replaced.
#[derive(Debug)]
pub struct ShrinkInProgress<'a> {
    storage: &'a AccountStorage,
    /// old store which will be shrunk and replaced
    old_store: Arc<AccountStorageEntry>,
    /// newly shrunk store with a subset of contents from 'old_store'
    new_store: Arc<AccountStorageEntry>,
    slot: Slot,
}

/// called when the shrink is no longer in progress. This means we can release the old append vec and update the map of slot -> append vec
impl Drop for ShrinkInProgress<'_> {
    fn drop(&mut self) {
        assert_eq!(
            self.storage
                .map
                .insert(self.slot, Arc::clone(&self.new_store))
                .map(|store| store.id()),
            Some(self.old_store.id())
        );

        // The new store can be removed from 'shrink_in_progress_map'
        assert!(self
            .storage
            .shrink_in_progress_map
            .write()
            .unwrap()
            .remove(&self.slot)
            .is_some());
    }
}

impl ShrinkInProgress<'_> {
    pub fn new_storage(&self) -> &Arc<AccountStorageEntry> {
        &self.new_store
    }
    pub(crate) fn old_storage(&self) -> &Arc<AccountStorageEntry> {
        &self.old_store
    }
}

/// Wrapper over slice of `Arc<AccountStorageEntry>` that provides an ordered access to storages.
///
/// A few strategies are available for ordering storages:
/// - `with_small_to_large_ratio`: interleaving small and large storage file sizes
/// - `with_random_order`: orders storages randomly
pub struct AccountStoragesOrderer<'a> {
    storages: &'a [Arc<AccountStorageEntry>],
    indices: Box<[usize]>,
}

impl<'a> AccountStoragesOrderer<'a> {
    /// Create balancing orderer that interleaves storages with small and large file sizes.
    ///
    /// Storages are returned in cycles based on `small_to_large_ratio` - `ratio.0` small storages
    /// preceding `ratio.1` large storages.
    pub fn with_small_to_large_ratio(
        storages: &'a [Arc<AccountStorageEntry>],
        small_to_large_ratio: (usize, usize),
    ) -> Self {
        let len_range = 0..storages.len();
        let mut indices: Vec<_> = len_range.clone().collect();
        indices.sort_unstable_by_key(|i| storages[*i].capacity());
        indices.iter_mut().for_each(|i| {
            *i = select_from_range_with_start_end_rates(len_range.clone(), *i, small_to_large_ratio)
        });
        Self {
            storages,
            indices: indices.into_boxed_slice(),
        }
    }

    /// Create randomizing orderer.
    pub fn with_random_order(storages: &'a [Arc<AccountStorageEntry>]) -> Self {
        let mut indices: Vec<usize> = (0..storages.len()).collect();
        indices.shuffle(&mut rand::rng());
        Self {
            storages,
            indices: indices.into_boxed_slice(),
        }
    }

    pub fn entries_len(&self) -> usize {
        self.indices.len()
    }

    /// Returns the original index, into the storages slice, at `position`
    ///
    /// # Panics
    ///
    /// Caller must ensure `position` is in range, else will panic.
    pub fn original_index(&'a self, position: usize) -> usize {
        self.indices[position]
    }

    pub fn iter(&'a self) -> impl ExactSizeIterator<Item = &'a AccountStorageEntry> + 'a {
        self.indices.iter().map(|i| self.storages[*i].as_ref())
    }

    pub fn par_iter(&'a self) -> impl IndexedParallelIterator<Item = &'a AccountStorageEntry> + 'a {
        self.indices.par_iter().map(|i| self.storages[*i].as_ref())
    }

    pub fn into_concurrent_consumer(self) -> AccountStoragesConcurrentConsumer<'a> {
        AccountStoragesConcurrentConsumer::new(self)
    }
}

impl Index<usize> for AccountStoragesOrderer<'_> {
    type Output = AccountStorageEntry;

    fn index(&self, position: usize) -> &Self::Output {
        // SAFETY: Caller must ensure `position` is in range.
        let original_index = self.original_index(position);
        // SAFETY: `original_index` must be valid here, so it is a valid index into `storages`.
        self.storages[original_index].as_ref()
    }
}

/// A thread-safe, lock-free iterator for consuming `AccountStorageEntry` values
/// from an `AccountStoragesOrderer` across multiple threads.
///
/// Unlike standard iterators, `AccountStoragesConcurrentConsumer`:
/// - Is **shared** between threads via references (`&self`), not moved.
/// - Allows safe, parallel consumption where each item is yielded at most once.
/// - Does **not** implement `Iterator` because it must take `&self` instead of `&mut self`.
pub struct AccountStoragesConcurrentConsumer<'a> {
    orderer: AccountStoragesOrderer<'a>,
    current_position: AtomicUsize,
}

impl<'a> AccountStoragesConcurrentConsumer<'a> {
    pub fn new(orderer: AccountStoragesOrderer<'a>) -> Self {
        Self {
            orderer,
            current_position: AtomicUsize::new(0),
        }
    }

    /// Takes the next `AccountStorageEntry` moving shared consume position
    /// until the end of the entries source is reached.
    pub fn next(&'a self) -> Option<NextItem<'a>> {
        let position = self.current_position.fetch_add(1, Ordering::Relaxed);
        if position < self.orderer.entries_len() {
            // SAFETY: We have ensured `position` is in range.
            let original_index = self.orderer.original_index(position);
            let storage = &self.orderer[position];
            Some(NextItem {
                position,
                original_index,
                storage,
            })
        } else {
            None
        }
    }
}

/// Value returned from calling `AccountStoragesConcurrentConsumer::next()`
#[derive(Debug)]
pub struct NextItem<'a> {
    /// The position through the orderer for this call to `next()`
    pub position: usize,
    /// The index into the original storages slice at this position
    pub original_index: usize,
    /// The storage itself
    pub storage: &'a AccountStorageEntry,
}

/// Select the `nth` (`0 <= nth < range.len()`) value from a `range`, choosing values alternately
/// from its start or end according to a `start_rate : end_rate` ratio.
///
/// For every `start_rate` values selected from the start, `end_rate` values are selected from the end.
/// The resulting sequence alternates in a balanced and interleaved fashion between the range's start and end.
/// ```
fn select_from_range_with_start_end_rates(
    range: Range<usize>,
    nth: usize,
    (start_rate, end_rate): (usize, usize),
) -> usize {
    let range_len = range.len();
    let cycle = start_rate + end_rate;
    let cycle_index = nth % cycle;
    let cycle_num = nth.checked_div(cycle).expect("rates sum must be positive");

    let index = if cycle_index < start_rate {
        cycle_num * start_rate + cycle_index
    } else {
        let end_index = cycle_num * end_rate + cycle_index - start_rate;
        range_len - end_index - 1
    };
    range.start + index
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        crate::accounts_file::{AccountsFileProvider, StorageAccess},
        std::{iter, path::Path},
        test_case::test_case,
    };

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_shrink_in_progress(storage_access: StorageAccess) {
        // test that we check in order map then shrink_in_progress_map
        let storage = AccountStorage::default();
        let slot = 0;
        let id = 0;
        // empty everything
        assert!(storage.get_account_storage_entry(slot, id).is_none());

        // add a map store
        let common_store_path = Path::new("");
        let store_file_size = 4000;
        let store_file_size2 = store_file_size * 2;
        // 2 append vecs with same id, but different sizes
        let entry = Arc::new(AccountStorageEntry::new(
            common_store_path,
            slot,
            id,
            store_file_size,
            AccountsFileProvider::AppendVec,
            storage_access,
        ));
        let entry2 = Arc::new(AccountStorageEntry::new(
            common_store_path,
            slot,
            id,
            store_file_size2,
            AccountsFileProvider::AppendVec,
            storage_access,
        ));
        storage.map.insert(slot, entry);

        // look in map
        assert_eq!(
            store_file_size,
            storage
                .get_account_storage_entry(slot, id)
                .map(|entry| entry.accounts.capacity())
                .unwrap_or_default()
        );

        // look in shrink_in_progress_map
        storage
            .shrink_in_progress_map
            .write()
            .unwrap()
            .insert(slot, entry2);

        // look in map
        assert_eq!(
            store_file_size,
            storage
                .get_account_storage_entry(slot, id)
                .map(|entry| entry.accounts.capacity())
                .unwrap_or_default()
        );

        // remove from map
        storage.map.remove(&slot).unwrap();

        // look in shrink_in_progress_map
        assert_eq!(
            store_file_size2,
            storage
                .get_account_storage_entry(slot, id)
                .map(|entry| entry.accounts.capacity())
                .unwrap_or_default()
        );
    }

    impl AccountStorage {
        fn get_test_storage_with_id(
            &self,
            id: AccountsFileId,
            storage_access: StorageAccess,
        ) -> Arc<AccountStorageEntry> {
            let slot = 0;
            // add a map store
            let common_store_path = Path::new("");
            let store_file_size = 4000;
            Arc::new(AccountStorageEntry::new(
                common_store_path,
                slot,
                id,
                store_file_size,
                AccountsFileProvider::AppendVec,
                storage_access,
            ))
        }
        fn get_test_storage(&self, storage_access: StorageAccess) -> Arc<AccountStorageEntry> {
            self.get_test_storage_with_id(0, storage_access)
        }
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    #[should_panic(expected = "shrink is in progress!")]
    fn test_get_slot_storage_entry_fail(storage_access: StorageAccess) {
        let storage = AccountStorage::default();
        storage
            .shrink_in_progress_map
            .write()
            .unwrap()
            .insert(0, storage.get_test_storage(storage_access));
        storage.get_slot_storage_entry(0);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    #[should_panic(expected = "shrink is in progress!")]
    fn test_all_slots_fail(storage_access: StorageAccess) {
        let storage = AccountStorage::default();
        storage
            .shrink_in_progress_map
            .write()
            .unwrap()
            .insert(0, storage.get_test_storage(storage_access));
        storage.all_slots();
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    #[should_panic(expected = "shrink is in progress!")]
    fn test_initialize_fail(storage_access: StorageAccess) {
        let mut storage = AccountStorage::default();
        storage
            .shrink_in_progress_map
            .write()
            .unwrap()
            .insert(0, storage.get_test_storage(storage_access));
        storage.initialize(AccountStorageMap::default());
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    #[should_panic(
        expected = "shrink_can_be_active || self.shrink_in_progress_map.read().unwrap().is_empty()"
    )]
    fn test_remove_fail(storage_access: StorageAccess) {
        let storage = AccountStorage::default();
        storage
            .shrink_in_progress_map
            .write()
            .unwrap()
            .insert(0, storage.get_test_storage(storage_access));
        storage.remove(&0, false);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    #[should_panic(expected = "shrink is in progress!")]
    fn test_iter_fail(storage_access: StorageAccess) {
        let storage = AccountStorage::default();
        storage
            .shrink_in_progress_map
            .write()
            .unwrap()
            .insert(0, storage.get_test_storage(storage_access));
        storage.iter();
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    #[should_panic(expected = "shrink is in progress!")]
    fn test_insert_fail(storage_access: StorageAccess) {
        let storage = AccountStorage::default();
        let sample = storage.get_test_storage(storage_access);
        storage
            .shrink_in_progress_map
            .write()
            .unwrap()
            .insert(0, sample.clone());
        storage.insert(sample);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    #[should_panic(expected = "duplicate call")]
    fn test_shrinking_in_progress_fail3(storage_access: StorageAccess) {
        // already entry in shrink_in_progress_map
        let storage = AccountStorage::default();
        let sample = storage.get_test_storage(storage_access);
        storage.map.insert(0, sample.clone());
        storage
            .shrink_in_progress_map
            .write()
            .unwrap()
            .insert(0, sample.clone());
        storage.shrinking_in_progress(0, sample);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    #[should_panic(expected = "duplicate call")]
    fn test_shrinking_in_progress_fail4(storage_access: StorageAccess) {
        // already called 'shrink_in_progress' on this slot and it is still active
        let storage = AccountStorage::default();
        let sample_to_shrink = storage.get_test_storage(storage_access);
        let sample = storage.get_test_storage(storage_access);
        storage.map.insert(0, sample_to_shrink);
        let _shrinking_in_progress = storage.shrinking_in_progress(0, sample.clone());
        storage.shrinking_in_progress(0, sample);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_shrinking_in_progress_second_call(storage_access: StorageAccess) {
        // already called 'shrink_in_progress' on this slot, but it finished, so we succeed
        // verify data structures during and after shrink and then with subsequent shrink call
        let storage = AccountStorage::default();
        let slot = 0;
        let id_to_shrink = 1;
        let id_shrunk = 0;
        let sample_to_shrink = storage.get_test_storage_with_id(id_to_shrink, storage_access);
        let sample = storage.get_test_storage(storage_access);
        storage.map.insert(slot, sample_to_shrink);
        let shrinking_in_progress = storage.shrinking_in_progress(slot, sample.clone());
        assert!(storage.map.contains_key(&slot));
        assert_eq!(id_to_shrink, storage.map.get(&slot).unwrap().id());
        assert_eq!(
            (slot, id_shrunk),
            storage
                .shrink_in_progress_map
                .read()
                .unwrap()
                .iter()
                .next()
                .map(|r| (*r.0, r.1.id()))
                .unwrap()
        );
        drop(shrinking_in_progress);
        assert!(storage.map.contains_key(&slot));
        assert_eq!(id_shrunk, storage.map.get(&slot).unwrap().id());
        assert!(storage.shrink_in_progress_map.read().unwrap().is_empty());
        storage.shrinking_in_progress(slot, sample);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    #[should_panic(expected = "no pre-existing storage for shrinking slot")]
    fn test_shrinking_in_progress_fail1(storage_access: StorageAccess) {
        // nothing in slot currently
        let storage = AccountStorage::default();
        let sample = storage.get_test_storage(storage_access);
        storage.shrinking_in_progress(0, sample);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    #[should_panic(expected = "no pre-existing storage for shrinking slot")]
    fn test_shrinking_in_progress_fail2(storage_access: StorageAccess) {
        // nothing in slot currently, but there is an empty map entry
        let storage = AccountStorage::default();
        let sample = storage.get_test_storage(storage_access);
        storage.shrinking_in_progress(0, sample);
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_missing(storage_access: StorageAccess) {
        // already called 'shrink_in_progress' on this slot, but it finished, so we succeed
        // verify data structures during and after shrink and then with subsequent shrink call
        let storage = AccountStorage::default();
        let sample = storage.get_test_storage(storage_access);
        let id = sample.id();
        let missing_id = 9999;
        let slot = sample.slot();
        // id is missing since not in maps at all
        assert!(storage.get_account_storage_entry(slot, id).is_none());
        // missing should always be missing
        assert!(storage
            .get_account_storage_entry(slot, missing_id)
            .is_none());
        storage.map.insert(slot, sample.clone());
        // id is found in map
        assert!(storage.get_account_storage_entry(slot, id).is_some());
        assert!(storage
            .get_account_storage_entry(slot, missing_id)
            .is_none());
        storage
            .shrink_in_progress_map
            .write()
            .unwrap()
            .insert(slot, Arc::clone(&sample));
        // id is found in map
        assert!(storage
            .get_account_storage_entry(slot, missing_id)
            .is_none());
        assert!(storage.get_account_storage_entry(slot, id).is_some());
        storage.map.remove(&slot);
        // id is found in shrink_in_progress_map
        assert!(storage
            .get_account_storage_entry(slot, missing_id)
            .is_none());
        assert!(storage.get_account_storage_entry(slot, id).is_some());
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_get_if(storage_access: StorageAccess) {
        let storage = AccountStorage::default();
        assert!(storage.get_if(|_, _| true).is_empty());

        // add some entries
        let ids = [123, 456, 789];
        for id in ids {
            let slot = id as Slot;
            let entry = AccountStorageEntry::new(
                Path::new(""),
                slot,
                id,
                5000,
                AccountsFileProvider::AppendVec,
                storage_access,
            );
            storage.map.insert(slot, entry.into());
        }

        // look 'em up
        for id in ids {
            let found = storage.get_if(|slot, _| *slot == id as Slot);
            assert!(found
                .iter()
                .map(|(slot, _)| *slot)
                .eq(iter::once(id as Slot)));
        }

        assert!(storage.get_if(|_, _| false).is_empty());
        assert_eq!(storage.get_if(|_, _| true).len(), ids.len());
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    #[should_panic(expected = "shrink is in progress!")]
    fn test_get_if_fail(storage_access: StorageAccess) {
        let storage = AccountStorage::default();
        storage
            .shrink_in_progress_map
            .write()
            .unwrap()
            .insert(0, storage.get_test_storage(storage_access));
        storage.get_if(|_, _| true);
    }

    #[test]
    fn test_select_range_with_start_end_rates() {
        let interleaved: Vec<_> = (0..10)
            .map(|i| select_from_range_with_start_end_rates(1..11, i, (2, 1)))
            .collect();
        assert_eq!(interleaved, vec![1, 2, 10, 3, 4, 9, 5, 6, 8, 7]);

        let interleaved: Vec<_> = (0..10)
            .map(|i| select_from_range_with_start_end_rates(1..11, i, (1, 1)))
            .collect();
        assert_eq!(interleaved, vec![1, 10, 2, 9, 3, 8, 4, 7, 5, 6]);

        let interleaved: Vec<_> = (0..9)
            .map(|i| select_from_range_with_start_end_rates(1..10, i, (2, 1)))
            .collect();
        assert_eq!(interleaved, vec![1, 2, 9, 3, 4, 8, 5, 6, 7]);

        let interleaved: Vec<_> = (0..9)
            .map(|i| select_from_range_with_start_end_rates(1..10, i, (1, 2)))
            .collect();
        assert_eq!(interleaved, vec![1, 9, 8, 2, 7, 6, 3, 5, 4]);

        let interleaved: Vec<_> = (0..13)
            .map(|i| select_from_range_with_start_end_rates(1..14, i, (2, 3)))
            .collect();
        assert_eq!(interleaved, vec![1, 2, 13, 12, 11, 3, 4, 10, 9, 8, 5, 6, 7]);
    }
}
