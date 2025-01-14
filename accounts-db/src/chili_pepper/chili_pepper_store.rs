#![allow(dead_code)]
use {
    super::chili_pepper_mutator_thread::{
        ChiliPepperMutatorThread, ChiliPepperMutatorThreadCommand,
    },
    crate::ancestors::Ancestors,
    crossbeam_channel::{unbounded, Sender},
    itertools::Itertools,
    redb::{
        backends::InMemoryBackend, Database, Durability, Error, Key, ReadableTableMetadata,
        TableDefinition, TableStats, TypeName, Value,
    },
    solana_sdk::{clock::Slot, pubkey::Pubkey},
    std::{
        borrow::Borrow,
        cmp::Ordering,
        collections::{HashMap, HashSet},
        fmt::Debug,
        io, mem,
        path::{Path, PathBuf},
        sync::{Arc, Mutex},
    },
};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct PubkeySlot<'a>(&'a Pubkey, Slot);

impl PubkeySlot<'_> {
    pub fn pubkey(&self) -> &Pubkey {
        self.0
    }

    pub fn slot(&self) -> Slot {
        self.1
    }

    pub fn new<'a>(pubkey: &'a Pubkey, slot: Slot) -> PubkeySlot<'a> {
        PubkeySlot(pubkey, slot)
    }
}

impl<'a> Value for PubkeySlot<'a> {
    type SelfType<'b> = PubkeySlot<'b>
    where
        Self: 'b;
    type AsBytes<'b> = Vec<u8>
    where
        Self: 'b;

    fn fixed_width() -> Option<usize> {
        Some(40)
    }

    fn from_bytes<'b>(data: &'b [u8]) -> PubkeySlot<'b>
    where
        Self: 'b,
    {
        let (pubkey_data, slot_data) = data.split_at(32);
        let pubkey = bytemuck::from_bytes::<Pubkey>(pubkey_data);
        let slot = u64::from_le_bytes(slot_data.try_into().unwrap());
        PubkeySlot(&pubkey, slot)
    }

    fn as_bytes<'b, 'c: 'b>(value: &'b Self::SelfType<'c>) -> Vec<u8>
    where
        Self: 'c,
    {
        let mut result = Vec::with_capacity(40);
        result.extend_from_slice(value.0.as_ref());
        result.extend_from_slice(&value.1.to_le_bytes());
        result
    }

    fn type_name() -> TypeName {
        TypeName::new(&format!("PubkeySlot"))
    }
}

impl<'a> Key for PubkeySlot<'a> {
    fn compare(data1: &[u8], data2: &[u8]) -> Ordering {
        data1.cmp(data2)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct DBPubkey<'a>(&'a Pubkey);

impl<'a> Value for DBPubkey<'a> {
    type SelfType<'b> = DBPubkey<'b>
    where
        Self: 'b;
    type AsBytes<'b> = Vec<u8>
    where
        Self: 'b;

    fn fixed_width() -> Option<usize> {
        Some(32)
    }

    fn from_bytes<'b>(data: &'b [u8]) -> DBPubkey<'b>
    where
        Self: 'b,
    {
        let pubkey = bytemuck::from_bytes::<Pubkey>(data);
        DBPubkey(&pubkey)
    }

    fn as_bytes<'b, 'c: 'b>(value: &'b Self::SelfType<'c>) -> Vec<u8>
    where
        Self: 'c,
    {
        let mut result = Vec::with_capacity(32);
        result.extend_from_slice(value.0.as_ref());
        result
    }

    fn type_name() -> TypeName {
        TypeName::new(&format!("Pubkey"))
    }
}

impl<'a> Key for DBPubkey<'a> {
    fn compare(data1: &[u8], data2: &[u8]) -> Ordering {
        data1.cmp(data2)
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct DBSlotList {
    list: Vec<(Slot, u64)>,
}

impl Value for DBSlotList {
    type SelfType<'b> = DBSlotList
        where Self: 'b;
    type AsBytes<'b> = Vec<u8>
        where Self: 'b;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
    where
        Self: 'a,
    {
        let mut list =
            Vec::with_capacity(data.len() / (mem::size_of::<Slot>() + mem::size_of::<u64>()));
        let mut data = data;
        while !data.is_empty() {
            let (slot_data, value_data) = data.split_at(mem::size_of::<Slot>());
            let slot = u64::from_le_bytes(slot_data.try_into().unwrap());
            let value = u64::from_le_bytes(value_data.try_into().unwrap());
            list.push((slot, value));
            data = &data[mem::size_of::<Slot>() + mem::size_of::<u64>()..];
        }
        DBSlotList { list }
    }

    fn as_bytes<'b, 'c: 'b>(value: &'b Self::SelfType<'c>) -> Vec<u8> {
        let mut result =
            Vec::with_capacity(value.list.len() * (mem::size_of::<Slot>() + mem::size_of::<u64>()));
        for (slot, value) in value.list.iter() {
            result.extend_from_slice(&slot.to_le_bytes());
            result.extend_from_slice(&value.to_le_bytes());
        }
        result
    }

    fn type_name() -> TypeName {
        TypeName::new(&format!("SlotList"))
    }
}

trait ChiliPepperStoreTrait {
    fn get(&self, key: PubkeySlot) -> Result<Option<u64>, Error>;
    fn get_all_for_pubkey(&self, pubkey: &Pubkey) -> Result<Vec<(Slot, u64)>, Error>;
    fn load_for_pubkey_with_ancestors(
        &self,
        pubkey: &Pubkey,
        ancestors: Vec<Slot>,
    ) -> Result<Option<(Slot, u64)>, Error>;
    fn bulk_get_for_pubkeys<'a, I>(&self, pubkeys: I) -> Result<Vec<Vec<(Slot, u64)>>, Error>
    where
        I: IntoIterator<Item = &'a Pubkey>;
    fn insert(&self, key: PubkeySlot, value: u64) -> Result<(), Error>;
    fn remove(&self, key: PubkeySlot) -> Result<(), Error>;
    fn bulk_insert<'a, I>(&self, data: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = (PubkeySlot<'a>, u64)>;
    fn bulk_remove<'a, I>(&self, keys: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = PubkeySlot<'a>>;
    fn clean(&self, clean_slot: Slot, threshold_slot: u64) -> Result<(), Error>;
    fn create_savepoint(&self) -> Result<u64, Error>;
    fn remove_savepoint(&self, savepoint_id: u64) -> Result<bool, Error>;
    fn restore_savepoint(&self, savepoint_id: u64) -> Result<(), Error>;
    fn snapshot(&self, savepoint_id: u64, snapshot_path: impl AsRef<Path>) -> Result<(), Error>;
    fn add_dead_slot(&self, slot: Slot);
    fn add_uncleaned_pubkey(&self, pubkey: Pubkey);
}

const TABLE: TableDefinition<PubkeySlot, u64> = TableDefinition::new("chili_pepper");
const TABLE_V2: TableDefinition<DBPubkey, DBSlotList> = TableDefinition::new("chili_pepper_v2");

/// The amount of memory to use for the cache, in bytes.
/// 90% is used for the read cache, and 10% is used for the write cache.
#[cfg(not(test))]
const DEFAULT_CACHE_SIZE: usize = 10 * 1024 * 1024 * 1024; // 10GB for validator

#[cfg(test)]
const DEFAULT_CACHE_SIZE: usize = 1024 * 1024; // 1MB for test

#[derive(Debug)]
pub struct ChiliPepperStoreInner {
    db: Database,
    path: PathBuf,
    dead_slots: Mutex<HashSet<Slot>>,
    uncleaned_pubkeys: Mutex<HashSet<Pubkey>>,
}

impl ChiliPepperStoreInner {
    pub fn new_with_path(path: impl AsRef<Path>) -> Result<Self, Error> {
        let db = Database::builder()
            .set_cache_size(1024 * 1024)
            .create(path.as_ref())
            .unwrap();

        // Only for testing. This doesn't support snapshots.
        // let db = Database::builder()
        //     .create_with_backend(InMemoryBackend::new())
        //     .unwrap();

        Ok(Self {
            db,
            path: path.as_ref().to_path_buf(),
            dead_slots: Mutex::new(HashSet::new()),
            uncleaned_pubkeys: Mutex::new(HashSet::new()),
        })
    }

    pub fn open_with_path(path: impl AsRef<Path>) -> Result<Self, Error> {
        let db = Database::open(path.as_ref())?;
        Ok(Self {
            db,
            path: path.as_ref().to_path_buf(),
            dead_slots: Mutex::new(HashSet::new()),
            uncleaned_pubkeys: Mutex::new(HashSet::new()),
        })
    }

    pub fn new(db: Database) -> Self {
        Self {
            db,
            path: PathBuf::new(),
            dead_slots: Mutex::new(HashSet::new()),
            uncleaned_pubkeys: Mutex::new(HashSet::new()),
        }
    }

    pub fn get_db(&self) -> &Database {
        &self.db
    }

    pub fn stat(&self) -> Result<TableStats, Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TABLE)?;
        table.stats().map_err(Error::from)
    }

    pub fn len(&self) -> Result<u64, Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table::<PubkeySlot, u64>(TABLE)?;
        table.len().map_err(Error::from)
    }

    pub fn get(&self, key: PubkeySlot) -> Result<Option<u64>, Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table::<PubkeySlot, u64>(TABLE)?;
        let iter = table.get(&key)?;
        let result = iter.map(|iter| iter.value());
        Ok(result)
    }

    /// Get all the (slot, chili_pepper_value) for a given pubkey.
    /// The result is sorted by slot.
    /// Returns an empty vector if the pubkey is not found.
    pub fn get_all_for_pubkey(&self, pubkey: &Pubkey) -> Result<Vec<(Slot, u64)>, Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table::<PubkeySlot, u64>(TABLE)?;
        let result = table.range(PubkeySlot(pubkey, 0)..PubkeySlot(pubkey, u64::MAX))?;

        let mut v = Vec::new();

        for iter in result {
            let (key, value) = iter?;
            let slot = key.value().1;
            let value = value.value();
            v.push((slot, value));
        }
        Ok(v)
    }

    pub fn load_for_pubkey_with_ancestors(
        &self,
        pubkey: &Pubkey,
        ancestors: &Ancestors,
    ) -> Result<Option<(Slot, u64)>, Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table::<PubkeySlot, u64>(TABLE)?;
        let result = table.range(PubkeySlot(pubkey, 0)..PubkeySlot(pubkey, u64::MAX))?;

        for iter in result.rev() {
            let (key, value) = iter?;
            let slot = key.value().1;
            if ancestors.contains_key(&slot) {
                return Ok(Some((slot, value.value())));
            }

            if slot < ancestors.min_slot() {
                return Ok(Some((slot, value.value())));
            }
        }
        Ok(None)
    }

    /// Get all the (slot, chili_pepper_value) for a given list of pubkeys.
    pub fn bulk_get_for_pubkeys<'a, I>(&self, pubkeys: I) -> Result<Vec<Vec<(Slot, u64)>>, Error>
    where
        I: IntoIterator<Item = &'a Pubkey>,
    {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table::<PubkeySlot, u64>(TABLE)?;

        let mut result = vec![];

        for pubkey in pubkeys {
            let pubkey_slot = PubkeySlot(pubkey, 0);
            let iter = table.range(pubkey_slot..PubkeySlot(pubkey, u64::MAX))?;

            let mut v = Vec::new();

            for iter in iter {
                let (key, value) = iter?;
                let slot = key.value().1;
                let value = value.value();
                v.push((slot, value));
            }
            result.push(v);
        }
        Ok(result)
    }

    pub fn insert(&self, key: PubkeySlot, value: u64) -> Result<(), Error> {
        //self.uncleaned_pubkeys.lock().unwrap().insert(*key.0);

        let mut write_txn = self.db.begin_write()?;
        write_txn.set_durability(Durability::None); // don't persisted to disk for better performance
        {
            let mut table = write_txn.open_table::<PubkeySlot, u64>(TABLE)?;
            table.insert(key, value)?;
        }
        write_txn.commit().map_err(Error::from)
    }

    pub fn remove(&self, key: PubkeySlot) -> Result<(), Error> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table::<PubkeySlot, u64>(TABLE)?;
            table.remove(key)?;
        }
        write_txn.commit().map_err(Error::from)
    }

    pub fn bulk_insert<'a, I>(&self, data: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = (PubkeySlot<'a>, u64)>,
    {
        let mut write_txn = self.db.begin_write()?;
        write_txn.set_durability(Durability::None); // don't persisted to disk for better performance
        {
            let mut table = write_txn.open_table::<PubkeySlot, u64>(TABLE)?;
            for (key, value) in data {
                //self.uncleaned_pubkeys.lock().unwrap().insert(*key.0);
                table.insert(key, value.borrow())?;
            }
        }
        write_txn.commit().map_err(Error::from)
    }

    pub fn bulk_remove<'a, I>(&self, keys: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = PubkeySlot<'a>>,
    {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table::<PubkeySlot, u64>(TABLE)?;
            for key in keys {
                table.remove(key)?;
            }
        }
        write_txn.commit().map_err(Error::from)
    }

    pub fn clean(&self, clean_slot: Slot, threshold_slot: u64) -> Result<(), Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table::<PubkeySlot, u64>(TABLE)?;

        let mut lock = self.dead_slots.lock().unwrap();
        let dead_slots = std::mem::take(&mut *lock);
        drop(lock);

        let mut lock = self.uncleaned_pubkeys.lock().unwrap();
        let uncleaned_pubkeys = std::mem::take(&mut *lock);
        drop(lock);

        // TODO: optimize this to hash_map to save keys
        let mut to_remove = vec![];

        for pubkey in uncleaned_pubkeys.iter().sorted() {
            let pubkey_slot = PubkeySlot(pubkey, 0);
            let iter = table.range(pubkey_slot..PubkeySlot(pubkey, u64::MAX))?;

            let mut found_one_before_clean_slot = false;
            for iter in iter.rev() {
                let (key, _) = iter?;
                let pubkey = key.value().0;
                let slot = key.value().1;

                if dead_slots.contains(&slot) {
                    to_remove.push((*pubkey, slot));
                    continue;
                }
                if slot > clean_slot {
                    continue;
                }

                if slot < threshold_slot {
                    to_remove.push((*pubkey, slot));
                    continue;
                }

                if slot <= clean_slot {
                    if found_one_before_clean_slot {
                        to_remove.push((*pubkey, slot));
                    } else {
                        found_one_before_clean_slot = true;
                    }
                }
            }
        }

        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table::<PubkeySlot, u64>(TABLE)?;
            for key in to_remove {
                table.remove(PubkeySlot(&key.0, key.1))?;
            }
        }
        write_txn.commit().map_err(Error::from)
    }

    pub fn create_savepoint(&self) -> Result<u64, Error> {
        let txn = self.db.begin_write()?;
        let savepoint_id = txn.persistent_savepoint()?;
        txn.commit()?;
        Ok(savepoint_id)
    }

    pub fn remove_savepoint(&self, savepoint_id: u64) -> Result<bool, Error> {
        let txn = self.db.begin_write()?;
        let result = txn.delete_persistent_savepoint(savepoint_id)?;
        txn.commit()?;
        Ok(result)
    }

    pub fn restore_savepoint(&self, savepoint_id: u64) -> Result<(), Error> {
        let mut txn = self.db.begin_write()?;
        let savepoint = txn.get_persistent_savepoint(savepoint_id)?;
        txn.restore_savepoint(&savepoint)?;
        txn.commit()?;
        Ok(())
    }

    pub fn snapshot(
        &self,
        savepoint_id: u64,
        snapshot_path: impl AsRef<Path>,
    ) -> Result<(), Error> {
        assert!(self.path.exists(), "db file not exists");
        std::fs::copy(&self.path, &snapshot_path).expect("copy db file success");
        let db = Database::open(snapshot_path.as_ref()).expect("open db success");
        let mut txn = db.begin_write()?;
        let savepoint = txn.get_persistent_savepoint(savepoint_id)?;
        txn.restore_savepoint(&savepoint)?;
        txn.commit()?;

        let txn = self.db.begin_write()?;
        txn.delete_persistent_savepoint(savepoint_id)?;
        txn.commit()?;

        Ok(())
    }

    pub fn add_dead_slot(&self, slot: Slot) {
        let mut dead_slots = self.dead_slots.lock().unwrap();
        dead_slots.insert(slot);
    }

    pub fn add_uncleaned_pubkey(&self, pubkey: Pubkey) {
        let mut uncleaned_pubkeys = self.uncleaned_pubkeys.lock().unwrap();
        uncleaned_pubkeys.insert(pubkey);
    }
}

#[derive(Debug)]
pub struct ChiliPepperStoreInnerV2 {
    db: Database,
    path: PathBuf,
    dead_slots: Mutex<HashSet<Slot>>,
    uncleaned_pubkeys: Mutex<HashSet<Pubkey>>,
}

impl ChiliPepperStoreInnerV2 {
    pub fn new_with_path(path: impl AsRef<Path>) -> Result<Self, Error> {
        let db = Database::builder()
            .set_cache_size(1024 * 1024)
            .create(path.as_ref())
            .unwrap();

        // Only for testing. This doesn't support snapshots.
        // let db = Database::builder()
        //     .create_with_backend(InMemoryBackend::new())
        //     .unwrap();

        Ok(Self {
            db,
            path: path.as_ref().to_path_buf(),
            dead_slots: Mutex::new(HashSet::new()),
            uncleaned_pubkeys: Mutex::new(HashSet::new()),
        })
    }

    pub fn open_with_path(path: impl AsRef<Path>) -> Result<Self, Error> {
        let db = Database::open(path.as_ref())?;
        Ok(Self {
            db,
            path: path.as_ref().to_path_buf(),
            dead_slots: Mutex::new(HashSet::new()),
            uncleaned_pubkeys: Mutex::new(HashSet::new()),
        })
    }

    pub fn new(db: Database) -> Self {
        Self {
            db,
            path: PathBuf::new(),
            dead_slots: Mutex::new(HashSet::new()),
            uncleaned_pubkeys: Mutex::new(HashSet::new()),
        }
    }

    pub fn get_db(&self) -> &Database {
        &self.db
    }

    pub fn stat(&self) -> Result<TableStats, Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table(TABLE_V2)?;
        table.stats().map_err(Error::from)
    }

    pub fn len(&self) -> Result<u64, Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table::<DBPubkey, DBSlotList>(TABLE_V2)?;
        table.len().map_err(Error::from)
    }

    pub fn get(&self, key: PubkeySlot) -> Result<Option<u64>, Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table::<DBPubkey, DBSlotList>(TABLE_V2)?;
        let iter = table.get(DBPubkey(key.0))?;
        let result = iter
            .map(|iter| {
                let list = iter.value();
                list.list
                    .iter()
                    .find(|(slot, _)| *slot == key.1)
                    .map(|(_, value)| *value)
            })
            .flatten();
        Ok(result)
    }

    /// Get all the (slot, chili_pepper_value) for a given pubkey.
    /// The result is sorted by slot.
    /// Returns an empty vector if the pubkey is not found.
    pub fn get_all_for_pubkey(&self, pubkey: &Pubkey) -> Result<Vec<(Slot, u64)>, Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table::<DBPubkey, DBSlotList>(TABLE_V2)?;
        let list = table
            .get(DBPubkey(pubkey))?
            .map(|iter| {
                let list = iter.value();
                list.list
                    .iter()
                    .map(|(slot, value)| (*slot, *value))
                    .collect()
            })
            .unwrap_or_else(|| vec![]);
        Ok(list)
    }

    pub fn load_for_pubkey_with_ancestors(
        &self,
        pubkey: &Pubkey,
        ancestors: &Ancestors,
    ) -> Result<Option<(Slot, u64)>, Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table::<DBPubkey, DBSlotList>(TABLE_V2)?;
        let found = table
            .get(DBPubkey(pubkey))?
            .map(|iter| {
                let list = iter.value();

                for (slot, value) in list.list.iter().rev() {
                    if ancestors.contains_key(&slot) {
                        return Some((*slot, *value));
                    }

                    if *slot < ancestors.min_slot() {
                        return Some((*slot, *value));
                    }
                }
                None
            })
            .flatten();

        Ok(found)
    }

    pub fn insert(&self, key: PubkeySlot, value: u64) -> Result<(), Error> {
        //self.uncleaned_pubkeys.lock().unwrap().insert(*key.0);

        let mut write_txn = self.db.begin_write()?;
        write_txn.set_durability(Durability::None); // don't persisted to disk for better performance
        {
            let mut table = write_txn.open_table::<DBPubkey, DBSlotList>(TABLE_V2)?;
            todo!();
            //table.insert(key, value)?;
        }
        write_txn.commit().map_err(Error::from)
    }

    pub fn remove(&self, key: PubkeySlot) -> Result<(), Error> {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table::<DBPubkey, DBSlotList>(TABLE_V2)?;
            todo!();
            //table.remove(key)?;
        }
        write_txn.commit().map_err(Error::from)
    }

    pub fn bulk_insert<'a, I>(&self, data: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = (PubkeySlot<'a>, u64)>,
    {
        let mut write_txn = self.db.begin_write()?;
        write_txn.set_durability(Durability::None); // don't persisted to disk for better performance
        {
            let mut table = write_txn.open_table::<DBPubkey, DBSlotList>(TABLE_V2)?;
            for (key, value) in data {
                todo!();
                //self.uncleaned_pubkeys.lock().unwrap().insert(*key.0);
                //table.insert(key, value.borrow())?;
            }
        }
        write_txn.commit().map_err(Error::from)
    }

    pub fn bulk_remove<'a, I>(&self, keys: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = PubkeySlot<'a>>,
    {
        let write_txn = self.db.begin_write()?;
        {
            let mut table = write_txn.open_table::<DBPubkey, DBSlotList>(TABLE_V2)?;
            for key in keys {
                todo!();
                //table.remove(key)?;
            }
        }
        write_txn.commit().map_err(Error::from)
    }

    pub fn clean(&self, clean_slot: Slot, threshold_slot: u64) -> Result<(), Error> {
        let read_txn = self.db.begin_read()?;
        let table = read_txn.open_table::<DBPubkey, DBSlotList>(TABLE_V2)?;
        todo!();

        // let mut lock = self.dead_slots.lock().unwrap();
        // let dead_slots = std::mem::take(&mut *lock);
        // drop(lock);

        // let mut lock = self.uncleaned_pubkeys.lock().unwrap();
        // let uncleaned_pubkeys = std::mem::take(&mut *lock);
        // drop(lock);

        // // TODO: optimize this to hash_map to save keys
        // let mut to_remove = vec![];

        // for pubkey in uncleaned_pubkeys.iter().sorted() {
        //     let pubkey_slot = PubkeySlot(pubkey, 0);
        //     let iter = table.range(pubkey_slot..PubkeySlot(pubkey, u64::MAX))?;

        //     let mut found_one_before_clean_slot = false;
        //     for iter in iter.rev() {
        //         let (key, _) = iter?;
        //         let pubkey = key.value().0;
        //         let slot = key.value().1;

        //         if dead_slots.contains(&slot) {
        //             to_remove.push((*pubkey, slot));
        //             continue;
        //         }
        //         if slot > clean_slot {
        //             continue;
        //         }

        //         if slot < threshold_slot {
        //             to_remove.push((*pubkey, slot));
        //             continue;
        //         }

        //         if slot <= clean_slot {
        //             if found_one_before_clean_slot {
        //                 to_remove.push((*pubkey, slot));
        //             } else {
        //                 found_one_before_clean_slot = true;
        //             }
        //         }
        //     }
        // }

        // let write_txn = self.db.begin_write()?;
        // {
        //     let mut table = write_txn.open_table::<PubkeySlot, u64>(TABLE)?;
        //     for key in to_remove {
        //         table.remove(PubkeySlot(&key.0, key.1))?;
        //     }
        // }
        // write_txn.commit().map_err(Error::from)
    }

    pub fn create_savepoint(&self) -> Result<u64, Error> {
        let txn = self.db.begin_write()?;
        let savepoint_id = txn.persistent_savepoint()?;
        txn.commit()?;
        Ok(savepoint_id)
    }

    pub fn remove_savepoint(&self, savepoint_id: u64) -> Result<bool, Error> {
        let txn = self.db.begin_write()?;
        let result = txn.delete_persistent_savepoint(savepoint_id)?;
        txn.commit()?;
        Ok(result)
    }

    pub fn restore_savepoint(&self, savepoint_id: u64) -> Result<(), Error> {
        let mut txn = self.db.begin_write()?;
        let savepoint = txn.get_persistent_savepoint(savepoint_id)?;
        txn.restore_savepoint(&savepoint)?;
        txn.commit()?;
        Ok(())
    }

    pub fn snapshot(
        &self,
        savepoint_id: u64,
        snapshot_path: impl AsRef<Path>,
    ) -> Result<(), Error> {
        assert!(self.path.exists(), "db file not exists");
        std::fs::copy(&self.path, &snapshot_path).expect("copy db file success");
        let db = Database::open(snapshot_path.as_ref()).expect("open db success");
        let mut txn = db.begin_write()?;
        let savepoint = txn.get_persistent_savepoint(savepoint_id)?;
        txn.restore_savepoint(&savepoint)?;
        txn.commit()?;

        let txn = self.db.begin_write()?;
        txn.delete_persistent_savepoint(savepoint_id)?;
        txn.commit()?;

        Ok(())
    }

    pub fn add_dead_slot(&self, slot: Slot) {
        let mut dead_slots = self.dead_slots.lock().unwrap();
        dead_slots.insert(slot);
    }

    pub fn add_uncleaned_pubkey(&self, pubkey: Pubkey) {
        let mut uncleaned_pubkeys = self.uncleaned_pubkeys.lock().unwrap();
        uncleaned_pubkeys.insert(pubkey);
    }
}

#[derive(Debug)]
pub struct ChiliPepperStore {
    pub store: Arc<ChiliPepperStoreInner>,
    sender: Sender<ChiliPepperMutatorThreadCommand>,
    thread: ChiliPepperMutatorThread,
}

impl ChiliPepperStore {
    pub fn new_with_path(
        path: impl AsRef<Path>,
        exit: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Self, Error> {
        let (sender, receiver) = unbounded();
        let store = Arc::new(ChiliPepperStoreInner::new_with_path(path)?);
        let thread = ChiliPepperMutatorThread::new(receiver, store.clone(), exit.clone());

        Ok(Self {
            store,
            sender,
            thread,
        })
    }

    pub fn open_with_path(
        path: impl AsRef<Path>,
        exit: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<Self, Error> {
        let (sender, receiver) = unbounded();
        let store = Arc::new(ChiliPepperStoreInner::open_with_path(path)?);
        let thread = ChiliPepperMutatorThread::new(receiver, store.clone(), exit.clone());

        Ok(Self {
            store,
            sender,
            thread,
        })
    }

    pub fn send(&self, command: ChiliPepperMutatorThreadCommand) -> Result<(), Error> {
        self.sender
            .send(command)
            .map_err(|e| Error::from(io::Error::new(io::ErrorKind::Other, e)))
    }

    fn get_all_for_pubkey(&self, pubkey: &Pubkey) -> Result<Vec<(Slot, u64)>, Error> {
        self.store.get_all_for_pubkey(pubkey)
    }

    pub fn load_with_ancestors(
        &self,
        pubkey: &Pubkey,
        ancestors: &Ancestors,
    ) -> Result<Option<(Slot, u64)>, Error> {
        self.store.load_for_pubkey_with_ancestors(pubkey, ancestors)
    }

    pub fn add_dead_slot(&self, slot: Slot) {
        self.store.add_dead_slot(slot);
    }

    pub fn add_uncleaned_pubkey(&self, pubkey: Pubkey) {
        self.store.add_uncleaned_pubkey(pubkey);
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    #[test]
    fn test_with_path() {
        let tmpfile = tempfile::NamedTempFile::new_in("/tmp").unwrap();
        let path = tmpfile.path().to_path_buf();
        let store = ChiliPepperStoreInner::new_with_path(&path).expect("create db success");

        let pk = Pubkey::from([1_u8; 32]);
        let some_key = PubkeySlot(&pk, 42);
        let some_value = 163;
        store.insert(some_key, some_value).unwrap();
        assert_eq!(store.len().unwrap(), 1);

        drop(store);

        let store = ChiliPepperStoreInner::open_with_path(&path).expect("open db success");
        assert_eq!(store.len().unwrap(), 1);
        assert_eq!(store.get(some_key).unwrap().unwrap(), some_value);
    }

    #[test]
    fn test_multi_keys() {
        let mut pks = vec![];
        for i in 1..=3 {
            pks.push(Pubkey::from([i; 32]));
        }
        let keys: Vec<_> = pks
            .iter()
            .zip(42..=44)
            .map(|(pk, v)| (PubkeySlot(pk, v)))
            .collect();

        let vals = vec![163, 164, 165];

        let tmpfile = tempfile::NamedTempFile::new_in("/tmp").unwrap();

        let db = Database::create(tmpfile.path()).expect("create db success");
        let store = ChiliPepperStoreInner::new(db);

        for (key, value) in keys.iter().zip(vals.iter()) {
            store.insert(*key, *value).unwrap();
        }
        assert_eq!(store.len().unwrap(), 3);

        for (key, value) in keys.iter().zip(vals.iter()) {
            assert_eq!(store.get(*key).unwrap().unwrap(), *value);
        }

        for key in keys.iter() {
            store.remove(*key).unwrap();
        }
        assert_eq!(store.len().unwrap(), 0);

        for key in keys.iter() {
            assert!(store.get(*key).unwrap().is_none());
        }
    }

    #[test]
    fn test_same_key_range() {
        let pk1 = Pubkey::from([1_u8; 32]);
        let pk2 = Pubkey::from([2_u8; 32]);

        let some_key = PubkeySlot(&pk1, 42);
        let some_key2 = PubkeySlot(&pk1, 43);
        let some_key3 = PubkeySlot(&pk1, 55);
        let some_key4 = PubkeySlot(&pk2, 55);

        let some_value = 163;
        let some_value2 = 164;
        let some_value3 = 165;
        let some_value4 = 166;

        let tmpfile = tempfile::NamedTempFile::new_in("/tmp").unwrap();
        let db = Database::create(tmpfile.path()).expect("create db success");
        let store = ChiliPepperStoreInner::new(db);

        store.insert(some_key, some_value).unwrap();
        store.insert(some_key2, some_value2).unwrap();
        store.insert(some_key3, some_value3).unwrap();
        store.insert(some_key4, some_value4).unwrap();
        assert_eq!(store.len().unwrap(), 4);

        let result = store.get_all_for_pubkey(&pk1).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].0, 42);
        assert_eq!(result[1].0, 43);
        assert_eq!(result[2].0, 55);
    }

    #[test]
    fn test_load_with_ancestor() {
        let pk1 = Pubkey::from([1_u8; 32]);

        let some_key = PubkeySlot(&pk1, 42);
        let some_key2 = PubkeySlot(&pk1, 43);
        let some_key3 = PubkeySlot(&pk1, 44);
        let some_key4 = PubkeySlot(&pk1, 45);

        let some_value = 163;
        let some_value2 = 164;
        let some_value3 = 165;
        let some_value4 = 166;

        let tmpfile = tempfile::NamedTempFile::new_in("/tmp").unwrap();
        let db = Database::create(tmpfile.path()).expect("create db success");
        let store = ChiliPepperStoreInner::new(db);

        store.insert(some_key, some_value).unwrap();
        store.insert(some_key2, some_value2).unwrap();
        store.insert(some_key3, some_value3).unwrap();
        store.insert(some_key4, some_value4).unwrap();
        assert_eq!(store.len().unwrap(), 4);

        let mut ancestors = Ancestors::default();
        ancestors.insert(42, 0);

        let result = store
            .load_for_pubkey_with_ancestors(&pk1, &ancestors)
            .unwrap();
        assert_eq!(result.unwrap(), (42, some_value));

        ancestors.insert(43, 0);
        ancestors.insert(45, 0);
        let result = store
            .load_for_pubkey_with_ancestors(&pk1, &ancestors)
            .unwrap();
        assert_eq!(result.unwrap(), (45, some_value4));

        let mut ancestors = Ancestors::default();
        ancestors.insert(100, 0);
        let result = store
            .load_for_pubkey_with_ancestors(&pk1, &ancestors)
            .unwrap();
        assert_eq!(result.unwrap(), (45, some_value4));
    }

    #[test]
    fn test_bulk_insert_remove_get() {
        let mut pks = vec![];
        for i in 1..=3 {
            pks.push(Pubkey::from([i; 32]));
        }
        let keys: Vec<_> = pks
            .iter()
            .zip(42..=44)
            .map(|(pk, v)| (PubkeySlot(pk, v)))
            .collect();

        let vals = vec![163, 164, 165];

        let to_insert = keys
            .iter()
            .copied()
            .zip(vals.iter().copied())
            .collect::<Vec<_>>();

        let tmpfile = tempfile::NamedTempFile::new_in("/tmp").unwrap();

        let db = Database::create(tmpfile.path()).expect("create db success");
        let store = ChiliPepperStoreInner::new(db);

        store.bulk_insert(to_insert.into_iter()).unwrap();

        assert_eq!(store.len().unwrap(), 3);
        for i in 0..3 {
            assert_eq!(store.get(keys[i]).unwrap().unwrap(), vals[i]);
        }

        let result = store.bulk_get_for_pubkeys(&pks).unwrap();
        assert_eq!(result.len(), 3);
        let expected = (42..=44).zip(vals.iter().copied()).collect::<Vec<_>>();
        for i in 0..3 {
            assert_eq!(result[i].len(), 1);
            assert_eq!(result[i][0], expected[i]);
        }

        store.bulk_remove(keys.iter().copied()).unwrap();
        assert_eq!(store.len().unwrap(), 0);
        for i in 0..3 {
            assert!(store.get(keys[i]).unwrap().is_none());
        }
        let result = store.bulk_get_for_pubkeys(&pks).unwrap();
        assert_eq!(result.len(), 3);
        for i in 0..3 {
            assert_eq!(result[i].len(), 0);
        }
    }

    #[test]
    fn test_transaction() {
        let pk1 = Pubkey::from([1_u8; 32]);

        let some_key = PubkeySlot(&pk1, 42);

        let some_value = 163;

        let tmpfile = tempfile::NamedTempFile::new_in("/tmp").unwrap();

        let db = Database::create(tmpfile.path()).expect("create db success");

        let write_txn = db.begin_write().expect("begin write success");
        {
            let mut table = write_txn.open_table::<PubkeySlot, u64>(TABLE).unwrap();
            table.insert(some_key, some_value.borrow()).unwrap();
        }
        write_txn.commit().expect("commit success");

        let read_txn = db.begin_read().expect("read begin success");
        {
            let table = read_txn.open_table::<PubkeySlot, u64>(TABLE).unwrap();
            let len = table.len().unwrap();
            assert_eq!(len, 1);
        }

        let write_txn = db.begin_write().expect("begin write success");
        {
            let mut table = write_txn.open_table::<PubkeySlot, u64>(TABLE).unwrap();
            table.remove(some_key).unwrap();

            // read_txn still read the old data
            let read_txn = db.begin_read().expect("read begin success");
            {
                let table = read_txn.open_table::<PubkeySlot, u64>(TABLE).unwrap();
                let len = table.len().unwrap();
                assert_eq!(len, 1);
            }
            {
                let table = read_txn.open_table::<PubkeySlot, u64>(TABLE).unwrap();
                let iter = table.get(&some_key).unwrap();
                assert_eq!(iter.unwrap().value(), some_value);
            }
        }
        write_txn.commit().expect("commit success");

        // old read_txn is still valid and read old data.
        {
            let table = read_txn.open_table::<PubkeySlot, u64>(TABLE).unwrap();
            let len = table.len().unwrap();
            assert_eq!(len, 1);
        }

        {
            let table = read_txn.open_table::<PubkeySlot, u64>(TABLE).unwrap();
            let iter = table.get(&some_key).unwrap();
            assert_eq!(iter.unwrap().value(), some_value);
        }

        // new read_txn read the new data.
        let read_txn = db.begin_read().expect("read begin success");
        {
            let table = read_txn.open_table::<PubkeySlot, u64>(TABLE).unwrap();
            let len = table.len().unwrap();
            assert_eq!(len, 0);
        }

        {
            let table = read_txn.open_table::<PubkeySlot, u64>(TABLE).unwrap();
            let iter = table.get(&some_key).unwrap();
            assert!(iter.is_none());
        }
    }

    #[test]
    fn test_stat() {
        let pk1 = Pubkey::from([1_u8; 32]);
        let pk2 = Pubkey::from([2_u8; 32]);
        let pk3 = Pubkey::from([3_u8; 32]);

        let some_key = PubkeySlot(&pk1, 42);
        let some_key2 = PubkeySlot(&pk2, 43);
        let some_key3 = PubkeySlot(&pk3, 44);

        let some_value = 163;
        let some_value2 = 164;
        let some_value3 = 165;

        let tmpfile = tempfile::NamedTempFile::new_in("/tmp").unwrap();

        let db = Database::create(tmpfile.path()).expect("create db success");
        let store = ChiliPepperStoreInner::new(db);

        store.insert(some_key, some_value).unwrap();
        store.insert(some_key2, some_value2).unwrap();
        store.insert(some_key3, some_value3).unwrap();
        assert_eq!(store.len().unwrap(), 3);

        let stat = store.stat().unwrap();
        assert_eq!(stat.tree_height(), 1);
        assert_eq!(stat.leaf_pages(), 1);
        assert_eq!(stat.branch_pages(), 0);
        assert_eq!(stat.stored_bytes(), 144);
        assert_eq!(stat.metadata_bytes(), 4);
        assert_eq!(stat.fragmented_bytes(), 3948);
    }

    #[test]
    fn test_clean() {
        let mut pks = vec![];
        for i in 0..10 {
            pks.push(Pubkey::from([i; 32]));
        }

        let data: Vec<_> = pks
            .iter()
            .zip(100..110)
            .map(|(pk, v)| (PubkeySlot(pk, 42), v))
            .collect();
        let tmpfile = tempfile::NamedTempFile::new_in("/tmp").unwrap();

        let db = Database::create(tmpfile.path()).expect("create db success");
        let store = ChiliPepperStoreInner::new(db);

        store.bulk_insert(data.into_iter()).unwrap();
        assert_eq!(store.len().unwrap(), 10);

        for pk in pks {
            store.add_uncleaned_pubkey(pk);
        }
        store.add_dead_slot(42);
        store.clean(43, 40).unwrap();
        assert_eq!(store.len().unwrap(), 0);
    }

    #[test]
    fn test_savepoint() {
        let pk1 = Pubkey::from([1_u8; 32]);
        let pk2 = Pubkey::from([2_u8; 32]);
        let pk3 = Pubkey::from([3_u8; 32]);

        let some_key = PubkeySlot(&pk1, 42);
        let some_key2 = PubkeySlot(&pk2, 43);
        let some_key3 = PubkeySlot(&pk3, 44);

        let some_value = 163;
        let some_value2 = 164;
        let some_value3 = 165;

        let tmpfile = tempfile::NamedTempFile::new_in("/tmp").unwrap();

        let db = Database::create(tmpfile.path()).expect("create db success");
        let store = ChiliPepperStoreInner::new(db);

        store.insert(some_key, some_value).unwrap();
        let savepoint_id = store.create_savepoint().unwrap();

        store.insert(some_key2, some_value2).unwrap();
        store.insert(some_key3, some_value3).unwrap();
        assert_eq!(store.len().unwrap(), 3);

        store.restore_savepoint(savepoint_id).unwrap();
        assert_eq!(store.len().unwrap(), 1);
        assert_eq!(store.get(some_key).unwrap().unwrap(), some_value);
        assert!(store.get(some_key2).unwrap().is_none());
        assert!(store.get(some_key3).unwrap().is_none());
    }

    #[test]
    fn test_snapshot() {
        let pk1 = Pubkey::from([1_u8; 32]);
        let pk2 = Pubkey::from([2_u8; 32]);
        let pk3 = Pubkey::from([3_u8; 32]);

        let some_key = PubkeySlot(&pk1, 42);
        let some_key2 = PubkeySlot(&pk2, 43);
        let some_key3 = PubkeySlot(&pk3, 44);

        let some_value = 163;
        let some_value2 = 164;
        let some_value3 = 165;

        let tmpfile = tempfile::NamedTempFile::new_in("/tmp").unwrap();

        let store =
            ChiliPepperStoreInner::new_with_path(tmpfile.path()).expect("create db success");
        store.insert(some_key, some_value).unwrap();
        let savepoint_id = store.create_savepoint().unwrap();

        store.insert(some_key2, some_value2).unwrap();
        store.insert(some_key3, some_value3).unwrap();
        assert_eq!(store.len().unwrap(), 3);

        let snapshot_path = tmpfile.path().with_extension("snapshot");
        store.snapshot(savepoint_id, &snapshot_path).unwrap();

        let db2 = Database::open(&snapshot_path).expect("open db success");
        let store2 = ChiliPepperStoreInner::new(db2);

        assert_eq!(store2.len().unwrap(), 1);
        assert_eq!(store2.get(some_key).unwrap().unwrap(), some_value);
        assert!(store2.get(some_key2).unwrap().is_none());
        assert!(store2.get(some_key3).unwrap().is_none());

        std::fs::remove_file(snapshot_path).expect("delete snapshot file success");
    }
}
