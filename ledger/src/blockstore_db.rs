pub use rocksdb::Direction as IteratorDirection;
use {
    crate::{
        blockstore::{
            column::{
                columns, Column, ColumnIndexDeprecation, ColumnName, ProtobufColumn, TypedColumn,
                DEPRECATED_PROGRAM_COSTS_COLUMN_NAME,
            },
            error::{BlockstoreError, Result},
        },
        blockstore_metrics::{
            maybe_enable_rocksdb_perf, report_rocksdb_read_perf, report_rocksdb_write_perf,
            BlockstoreRocksDbColumnFamilyMetrics, PerfSamplingStatus, PERF_METRIC_OP_NAME_GET,
            PERF_METRIC_OP_NAME_MULTI_GET, PERF_METRIC_OP_NAME_PUT,
            PERF_METRIC_OP_NAME_WRITE_BATCH,
        },
        blockstore_options::{AccessType, BlockstoreOptions, LedgerColumnOptions},
    },
    bincode::deserialize,
    log::*,
    prost::Message,
    rocksdb::{
        self,
        compaction_filter::CompactionFilter,
        compaction_filter_factory::{CompactionFilterContext, CompactionFilterFactory},
        properties as RocksProperties, ColumnFamily, ColumnFamilyDescriptor, CompactionDecision,
        DBCompressionType, DBIterator, DBPinnableSlice, DBRawIterator,
        IteratorMode as RocksIteratorMode, LiveFile, Options, WriteBatch as RWriteBatch, DB,
    },
    serde::de::DeserializeOwned,
    solana_clock::Slot,
    std::{
        collections::HashSet,
        ffi::{CStr, CString},
        fs,
        marker::PhantomData,
        num::NonZeroUsize,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        },
    },
};

const BLOCKSTORE_METRICS_ERROR: i64 = -1;

const MAX_WRITE_BUFFER_SIZE: u64 = 256 * 1024 * 1024; // 256MB

// SST files older than this value will be picked up for compaction. This value
// was chosen to be one day to strike a balance between storage getting
// reclaimed in a timely manner and the additional I/O that compaction incurs.
// For more details on this property, see
// https://github.com/facebook/rocksdb/blob/749b179c041347d150fa6721992ae8398b7d2b39/
//   include/rocksdb/advanced_options.h#L908C30-L908C30
const PERIODIC_COMPACTION_SECONDS: u64 = 60 * 60 * 24;

pub enum IteratorMode<Index> {
    Start,
    End,
    From(Index, IteratorDirection),
}

#[derive(Default, Clone, Debug)]
struct OldestSlot {
    slot: Arc<AtomicU64>,
    clean_slot_0: Arc<AtomicBool>,
}

impl OldestSlot {
    pub fn set(&self, oldest_slot: Slot) {
        // this is independently used for compaction_filter without any data dependency.
        // also, compaction_filters are created via its factories, creating short-lived copies of
        // this atomic value for the single job of compaction. So, Relaxed store can be justified
        // in total
        self.slot.store(oldest_slot, Ordering::Relaxed);
    }

    pub fn get(&self) -> Slot {
        // copy from the AtomicU64 as a general precaution so that the oldest_slot can not mutate
        // across single run of compaction for simpler reasoning although this isn't strict
        // requirement at the moment
        // also eventual propagation (very Relaxed) load is Ok, because compaction by nature doesn't
        // require strictly synchronized semantics in this regard
        self.slot.load(Ordering::Relaxed)
    }

    pub(crate) fn set_clean_slot_0(&self, clean_slot_0: bool) {
        self.clean_slot_0.store(clean_slot_0, Ordering::Relaxed);
    }

    pub(crate) fn get_clean_slot_0(&self) -> bool {
        self.clean_slot_0.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub(crate) struct Rocks {
    db: rocksdb::DB,
    path: PathBuf,
    access_type: AccessType,
    oldest_slot: OldestSlot,
    column_options: Arc<LedgerColumnOptions>,
    write_batch_perf_status: PerfSamplingStatus,
}

impl Rocks {
    pub(crate) fn open(path: PathBuf, options: BlockstoreOptions) -> Result<Rocks> {
        let recovery_mode = options.recovery_mode.clone();

        fs::create_dir_all(&path)?;

        // Use default database options
        let mut db_options = get_db_options(&options);
        if let Some(recovery_mode) = recovery_mode {
            db_options.set_wal_recovery_mode(recovery_mode.into());
        }
        let oldest_slot = OldestSlot::default();
        let cf_descriptors = Self::cf_descriptors(&path, &options, &oldest_slot);
        let column_options = Arc::from(options.column_options);

        // Open the database
        let mut db = match options.access_type {
            AccessType::Primary | AccessType::PrimaryForMaintenance => {
                DB::open_cf_descriptors(&db_options, &path, cf_descriptors)?
            }
            AccessType::ReadOnly => {
                info!(
                    "Opening Rocks with read only access. This additional access could \
                     temporarily degrade other accesses, such as by agave-validator"
                );
                let error_if_log_file_exists = false;
                DB::open_cf_descriptors_read_only(
                    &db_options,
                    &path,
                    cf_descriptors,
                    error_if_log_file_exists,
                )?
            }
        };

        // Delete the now unused program_costs column if it is present
        if db.cf_handle(DEPRECATED_PROGRAM_COSTS_COLUMN_NAME).is_some() {
            db.drop_cf(DEPRECATED_PROGRAM_COSTS_COLUMN_NAME)?;
        }

        let rocks = Rocks {
            db,
            path,
            access_type: options.access_type,
            oldest_slot,
            column_options,
            write_batch_perf_status: PerfSamplingStatus::default(),
        };

        rocks.configure_compaction();

        Ok(rocks)
    }

    /// Create the column family (CF) descriptors necessary to open the database.
    ///
    /// In order to open a RocksDB database with Primary access, all columns must be opened. So,
    /// in addition to creating descriptors for all of the expected columns, also create
    /// descriptors for columns that were discovered but are otherwise unknown to the software.
    ///
    /// One case where columns could be unknown is if a RocksDB database is modified with a newer
    /// software version that adds a new column, and then also opened with an older version that
    /// did not have knowledge of that new column.
    fn cf_descriptors(
        path: &Path,
        options: &BlockstoreOptions,
        oldest_slot: &OldestSlot,
    ) -> Vec<ColumnFamilyDescriptor> {
        let mut cf_descriptors = vec![
            new_cf_descriptor::<columns::SlotMeta>(options, oldest_slot),
            new_cf_descriptor::<columns::DeadSlots>(options, oldest_slot),
            new_cf_descriptor::<columns::DuplicateSlots>(options, oldest_slot),
            new_cf_descriptor::<columns::ErasureMeta>(options, oldest_slot),
            new_cf_descriptor::<columns::Orphans>(options, oldest_slot),
            new_cf_descriptor::<columns::BankHash>(options, oldest_slot),
            new_cf_descriptor::<columns::Root>(options, oldest_slot),
            new_cf_descriptor::<columns::Index>(options, oldest_slot),
            new_cf_descriptor::<columns::ShredData>(options, oldest_slot),
            new_cf_descriptor::<columns::ShredCode>(options, oldest_slot),
            new_cf_descriptor::<columns::TransactionStatus>(options, oldest_slot),
            new_cf_descriptor::<columns::AddressSignatures>(options, oldest_slot),
            new_cf_descriptor::<columns::TransactionMemos>(options, oldest_slot),
            new_cf_descriptor::<columns::TransactionStatusIndex>(options, oldest_slot),
            new_cf_descriptor::<columns::Rewards>(options, oldest_slot),
            new_cf_descriptor::<columns::Blocktime>(options, oldest_slot),
            new_cf_descriptor::<columns::PerfSamples>(options, oldest_slot),
            new_cf_descriptor::<columns::BlockHeight>(options, oldest_slot),
            new_cf_descriptor::<columns::OptimisticSlots>(options, oldest_slot),
            new_cf_descriptor::<columns::MerkleRootMeta>(options, oldest_slot),
        ];

        // When remaining columns are optional we can just return immediately here.
        if !must_open_all_column_families(&options.access_type) {
            return cf_descriptors;
        }

        // Attempt to detect the column families that are present. It is not a
        // fatal error if we cannot, for example, if the Blockstore is brand
        // new and will be created by the call to Rocks::open().
        let detected_cfs = match DB::list_cf(&Options::default(), path) {
            Ok(detected_cfs) => detected_cfs,
            Err(err) => {
                warn!("Unable to detect Rocks columns: {err:?}");
                vec![]
            }
        };
        // The default column is handled automatically, we don't need to create
        // a descriptor for it
        const DEFAULT_COLUMN_NAME: &str = "default";
        let known_cfs: HashSet<_> = cf_descriptors
            .iter()
            .map(|cf_descriptor| cf_descriptor.name().to_string())
            .chain(std::iter::once(DEFAULT_COLUMN_NAME.to_string()))
            .collect();
        detected_cfs.iter().for_each(|cf_name| {
            if !known_cfs.contains(cf_name.as_str()) {
                info!("Detected unknown column {cf_name}, opening column with basic options");
                // This version of the software was unaware of the column, so
                // it is fair to assume that we will not attempt to read or
                // write the column. So, set some bare bones settings to avoid
                // using extra resources on this unknown column.
                let mut options = Options::default();
                // Lower the default to avoid unnecessary allocations
                options.set_write_buffer_size(1024 * 1024);
                // Disable compactions to avoid any modifications to the column
                options.set_disable_auto_compactions(true);
                cf_descriptors.push(ColumnFamilyDescriptor::new(cf_name, options));
            }
        });

        cf_descriptors
    }

    const fn columns() -> [&'static str; 20] {
        [
            columns::ErasureMeta::NAME,
            columns::DeadSlots::NAME,
            columns::DuplicateSlots::NAME,
            columns::Index::NAME,
            columns::Orphans::NAME,
            columns::BankHash::NAME,
            columns::Root::NAME,
            columns::SlotMeta::NAME,
            columns::ShredData::NAME,
            columns::ShredCode::NAME,
            columns::TransactionStatus::NAME,
            columns::AddressSignatures::NAME,
            columns::TransactionMemos::NAME,
            columns::TransactionStatusIndex::NAME,
            columns::Rewards::NAME,
            columns::Blocktime::NAME,
            columns::PerfSamples::NAME,
            columns::BlockHeight::NAME,
            columns::OptimisticSlots::NAME,
            columns::MerkleRootMeta::NAME,
        ]
    }

    // Configure compaction on a per-column basis
    fn configure_compaction(&self) {
        // If compactions are disabled altogether, no need to tune values
        if should_disable_auto_compactions(&self.access_type) {
            info!(
                "Rocks's automatic compactions are disabled due to {:?} access",
                self.access_type
            );
            return;
        }

        // Some columns make use of rocksdb's compaction to help in cleaning
        // the database. See comments in should_enable_cf_compaction() for more
        // details on why some columns need compaction and why others do not.
        //
        // More specifically, periodic (automatic) compaction is used as
        // opposed to manual compaction requests on a range.
        // - Periodic compaction operates on individual files once the file
        //   has reached a certain (configurable) age. See comments at
        //   PERIODIC_COMPACTION_SECONDS for some more detail.
        // - Manual compaction operates on a range and could end up propagating
        //   through several files and/or levels of the db.
        //
        // Given that data is inserted into the db at a somewhat steady rate,
        // the age of the individual files will be fairly evently distributed
        // over time as well. Thus, the I/O to perform cleanup with periodic
        // compaction is also evenly distributed over time. On the other hand,
        // a manual compaction spanning a large numbers of files could cause
        // a sudden burst in I/O. Such a burst could potentially cause a write
        // stall in addition to negatively impacting other parts of the system.
        // Thus, the choice to use periodic compactions is fairly easy.
        for cf_name in Self::columns() {
            if should_enable_cf_compaction(cf_name) {
                let cf_handle = self.cf_handle(cf_name);
                self.db
                    .set_options_cf(
                        &cf_handle,
                        &[(
                            "periodic_compaction_seconds",
                            &PERIODIC_COMPACTION_SECONDS.to_string(),
                        )],
                    )
                    .unwrap();
            }
        }
    }

    pub(crate) fn column<C>(self: &Arc<Self>) -> LedgerColumn<C>
    where
        C: Column + ColumnName,
    {
        let column_options = Arc::clone(&self.column_options);
        LedgerColumn {
            backend: Arc::clone(self),
            column: PhantomData,
            column_options,
            read_perf_status: PerfSamplingStatus::default(),
            write_perf_status: PerfSamplingStatus::default(),
        }
    }

    pub(crate) fn destroy(path: &Path) -> Result<()> {
        DB::destroy(&Options::default(), path)?;

        Ok(())
    }

    pub(crate) fn cf_handle(&self, cf: &str) -> &ColumnFamily {
        self.db
            .cf_handle(cf)
            .expect("should never get an unknown column")
    }

    fn get_cf<K: AsRef<[u8]>>(&self, cf: &ColumnFamily, key: K) -> Result<Option<Vec<u8>>> {
        let opt = self.db.get_cf(cf, key)?;
        Ok(opt)
    }

    fn get_pinned_cf(
        &self,
        cf: &ColumnFamily,
        key: impl AsRef<[u8]>,
    ) -> Result<Option<DBPinnableSlice<'_>>> {
        let opt = self.db.get_pinned_cf(cf, key)?;
        Ok(opt)
    }

    fn put_cf<K: AsRef<[u8]>>(&self, cf: &ColumnFamily, key: K, value: &[u8]) -> Result<()> {
        self.db.put_cf(cf, key, value)?;
        Ok(())
    }

    fn multi_get_cf<'a, K, I>(
        &self,
        cf: &ColumnFamily,
        keys: I,
    ) -> impl Iterator<Item = Result<Option<DBPinnableSlice<'_>>>> + use<'_, K, I>
    where
        K: AsRef<[u8]> + 'a + ?Sized,
        I: IntoIterator<Item = &'a K>,
    {
        self.db
            .batched_multi_get_cf(cf, keys, /*sorted_input:*/ false)
            .into_iter()
            .map(|out| out.map_err(BlockstoreError::RocksDb))
    }

    fn delete_cf<K: AsRef<[u8]>>(&self, cf: &ColumnFamily, key: K) -> Result<()> {
        self.db.delete_cf(cf, key)?;
        Ok(())
    }

    /// Delete files whose slot range is within \[`from`, `to`\].
    fn delete_file_in_range_cf<K: AsRef<[u8]>>(
        &self,
        cf: &ColumnFamily,
        from_key: K,
        to_key: K,
    ) -> Result<()> {
        self.db.delete_file_in_range_cf(cf, from_key, to_key)?;
        Ok(())
    }

    pub(crate) fn iterator_cf(
        &self,
        cf: &ColumnFamily,
        iterator_mode: RocksIteratorMode,
    ) -> DBIterator<'_> {
        self.db.iterator_cf(cf, iterator_mode)
    }

    pub(crate) fn raw_iterator_cf(&self, cf: &ColumnFamily) -> Result<DBRawIterator<'_>> {
        Ok(self.db.raw_iterator_cf(cf))
    }

    pub(crate) fn batch(&self) -> Result<WriteBatch> {
        Ok(WriteBatch {
            write_batch: RWriteBatch::default(),
        })
    }

    pub(crate) fn write(&self, batch: WriteBatch) -> Result<()> {
        let op_start_instant = maybe_enable_rocksdb_perf(
            self.column_options.rocks_perf_sample_interval,
            &self.write_batch_perf_status,
        );
        let result = self.db.write(batch.write_batch);
        if let Some(op_start_instant) = op_start_instant {
            report_rocksdb_write_perf(
                PERF_METRIC_OP_NAME_WRITE_BATCH, // We use write_batch as cf_name for write batch.
                PERF_METRIC_OP_NAME_WRITE_BATCH, // op_name
                &op_start_instant.elapsed(),
                &self.column_options,
            );
        }
        match result {
            Ok(_) => Ok(()),
            Err(e) => Err(BlockstoreError::RocksDb(e)),
        }
    }

    pub(crate) fn is_primary_access(&self) -> bool {
        self.access_type == AccessType::Primary
            || self.access_type == AccessType::PrimaryForMaintenance
    }

    /// Retrieves the specified RocksDB integer property of the current
    /// column family.
    ///
    /// Full list of properties that return int values could be found
    /// [here](https://github.com/facebook/rocksdb/blob/08809f5e6cd9cc4bc3958dd4d59457ae78c76660/include/rocksdb/db.h#L654-L689).
    fn get_int_property_cf(&self, cf: &ColumnFamily, name: &'static std::ffi::CStr) -> Result<i64> {
        match self.db.property_int_value_cf(cf, name) {
            Ok(Some(value)) => Ok(value.try_into().unwrap()),
            Ok(None) => Ok(0),
            Err(e) => Err(BlockstoreError::RocksDb(e)),
        }
    }

    pub(crate) fn live_files_metadata(&self) -> Result<Vec<LiveFile>> {
        match self.db.live_files() {
            Ok(live_files) => Ok(live_files),
            Err(e) => Err(BlockstoreError::RocksDb(e)),
        }
    }

    pub(crate) fn storage_size(&self) -> Result<u64> {
        Ok(fs_extra::dir::get_size(&self.path)?)
    }

    pub(crate) fn set_oldest_slot(&self, oldest_slot: Slot) {
        self.oldest_slot.set(oldest_slot);
    }

    pub(crate) fn set_clean_slot_0(&self, clean_slot_0: bool) {
        self.oldest_slot.set_clean_slot_0(clean_slot_0);
    }
}

#[derive(Debug)]
pub struct LedgerColumn<C: Column + ColumnName> {
    backend: Arc<Rocks>,
    column: PhantomData<C>,
    pub column_options: Arc<LedgerColumnOptions>,
    read_perf_status: PerfSamplingStatus,
    write_perf_status: PerfSamplingStatus,
}

impl<C: Column + ColumnName> LedgerColumn<C> {
    pub fn submit_rocksdb_cf_metrics(&self) {
        let cf_rocksdb_metrics = BlockstoreRocksDbColumnFamilyMetrics {
            total_sst_files_size: self
                .get_int_property(RocksProperties::TOTAL_SST_FILES_SIZE)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            size_all_mem_tables: self
                .get_int_property(RocksProperties::SIZE_ALL_MEM_TABLES)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            num_snapshots: self
                .get_int_property(RocksProperties::NUM_SNAPSHOTS)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            oldest_snapshot_time: self
                .get_int_property(RocksProperties::OLDEST_SNAPSHOT_TIME)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            actual_delayed_write_rate: self
                .get_int_property(RocksProperties::ACTUAL_DELAYED_WRITE_RATE)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            is_write_stopped: self
                .get_int_property(RocksProperties::IS_WRITE_STOPPED)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            block_cache_capacity: self
                .get_int_property(RocksProperties::BLOCK_CACHE_CAPACITY)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            block_cache_usage: self
                .get_int_property(RocksProperties::BLOCK_CACHE_USAGE)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            block_cache_pinned_usage: self
                .get_int_property(RocksProperties::BLOCK_CACHE_PINNED_USAGE)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            estimate_table_readers_mem: self
                .get_int_property(RocksProperties::ESTIMATE_TABLE_READERS_MEM)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            mem_table_flush_pending: self
                .get_int_property(RocksProperties::MEM_TABLE_FLUSH_PENDING)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            compaction_pending: self
                .get_int_property(RocksProperties::COMPACTION_PENDING)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            num_running_compactions: self
                .get_int_property(RocksProperties::NUM_RUNNING_COMPACTIONS)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            num_running_flushes: self
                .get_int_property(RocksProperties::NUM_RUNNING_FLUSHES)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            estimate_oldest_key_time: self
                .get_int_property(RocksProperties::ESTIMATE_OLDEST_KEY_TIME)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
            background_errors: self
                .get_int_property(RocksProperties::BACKGROUND_ERRORS)
                .unwrap_or(BLOCKSTORE_METRICS_ERROR),
        };
        cf_rocksdb_metrics.report_metrics(C::NAME, &self.column_options);
    }
}

pub struct WriteBatch {
    write_batch: RWriteBatch,
}

pub(crate) struct BlockstoreByteReference<'a> {
    slice: DBPinnableSlice<'a>,
}

impl<'a> From<DBPinnableSlice<'a>> for BlockstoreByteReference<'a> {
    #[inline]
    fn from(slice: DBPinnableSlice<'a>) -> Self {
        Self { slice }
    }
}

impl std::ops::Deref for BlockstoreByteReference<'_> {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        &self.slice
    }
}

impl AsRef<[u8]> for BlockstoreByteReference<'_> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl WriteBatch {
    fn put_cf<K: AsRef<[u8]>>(&mut self, cf: &ColumnFamily, key: K, value: &[u8]) -> Result<()> {
        self.write_batch.put_cf(cf, key, value);
        Ok(())
    }

    fn delete_cf<K: AsRef<[u8]>>(&mut self, cf: &ColumnFamily, key: K) -> Result<()> {
        self.write_batch.delete_cf(cf, key);
        Ok(())
    }

    fn delete_range_cf<K: AsRef<[u8]>>(&mut self, cf: &ColumnFamily, from: K, to: K) -> Result<()> {
        self.write_batch.delete_range_cf(cf, from, to);
        Ok(())
    }
}

impl<C> LedgerColumn<C>
where
    C: Column + ColumnName,
{
    pub fn get_bytes(&self, index: C::Index) -> Result<Option<Vec<u8>>> {
        let is_perf_enabled = maybe_enable_rocksdb_perf(
            self.column_options.rocks_perf_sample_interval,
            &self.read_perf_status,
        );

        let key = <C as Column>::key(&index);
        let result = self.backend.get_cf(self.handle(), key);

        if let Some(op_start_instant) = is_perf_enabled {
            report_rocksdb_read_perf(
                C::NAME,
                PERF_METRIC_OP_NAME_GET,
                &op_start_instant.elapsed(),
                &self.column_options,
            );
        }
        result
    }

    /// Create a key type suitable for use with multi_get_bytes() and
    /// multi_get(). Those functions return iterators, so the keys must be
    /// created with a separate function in order to live long enough
    pub(crate) fn multi_get_keys<I>(&self, keys: I) -> Vec<<C as Column>::Key>
    where
        I: IntoIterator<Item = C::Index>,
    {
        keys.into_iter().map(|index| C::key(&index)).collect()
    }

    pub(crate) fn multi_get_bytes<'a, K>(
        &'a self,
        keys: impl IntoIterator<Item = &'a K> + 'a,
    ) -> impl Iterator<Item = Result<Option<BlockstoreByteReference<'a>>>> + 'a
    where
        K: AsRef<[u8]> + 'a + ?Sized,
    {
        let is_perf_enabled = maybe_enable_rocksdb_perf(
            self.column_options.rocks_perf_sample_interval,
            &self.read_perf_status,
        );

        let result = self
            .backend
            .multi_get_cf(self.handle(), keys)
            .map(|out| Ok(out?.map(BlockstoreByteReference::from)));

        if let Some(op_start_instant) = is_perf_enabled {
            report_rocksdb_read_perf(
                C::NAME,
                PERF_METRIC_OP_NAME_MULTI_GET,
                &op_start_instant.elapsed(),
                &self.column_options,
            );
        }

        result
    }

    pub fn iter(
        &self,
        iterator_mode: IteratorMode<C::Index>,
    ) -> Result<impl Iterator<Item = (C::Index, Box<[u8]>)> + '_> {
        let start_key: <C as Column>::Key;
        let iterator_mode = match iterator_mode {
            IteratorMode::Start => RocksIteratorMode::Start,
            IteratorMode::End => RocksIteratorMode::End,
            IteratorMode::From(start, direction) => {
                start_key = <C as Column>::key(&start);
                RocksIteratorMode::From(start_key.as_ref(), direction)
            }
        };

        let iter = self.backend.iterator_cf(self.handle(), iterator_mode);
        Ok(iter.map(|pair| {
            let (key, value) = pair.unwrap();
            (C::index(&key), value)
        }))
    }

    #[cfg(test)]
    // The validator performs compactions asynchronously, this method is
    // provided to force a synchronous compaction to test our compaction filter
    pub fn compact(&self) {
        // compact_range_cf() optionally takes a start and end key to limit
        // compaction. Providing values will result in a different method
        // getting called in the rocksdb code, even if the specified keys span
        // the entire key range of the column
        //
        // Internally, rocksdb will do some checks to figure out if it should
        // run a compaction. Empirically, it has been found that passing the
        // keys leads to more variability in whether rocksdb runs a compaction
        // or not. For the sake of our unit tests, we want the compaction to
        // run everytime. So, set the keys as None which will result in rocksdb
        // using the heavier method to determine if a compaction should run
        let (start, end) = (None::<&[u8]>, None::<&[u8]>);
        self.backend.db.compact_range_cf(self.handle(), start, end);
    }

    #[inline]
    pub fn handle(&self) -> &ColumnFamily {
        self.backend.cf_handle(C::NAME)
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> Result<bool> {
        let mut iter = self.backend.raw_iterator_cf(self.handle())?;
        iter.seek_to_first();
        Ok(!iter.valid())
    }

    pub fn put_bytes(&self, index: C::Index, value: &[u8]) -> Result<()> {
        let is_perf_enabled = maybe_enable_rocksdb_perf(
            self.column_options.rocks_perf_sample_interval,
            &self.write_perf_status,
        );

        let key = <C as Column>::key(&index);
        let result = self.backend.put_cf(self.handle(), key, value);

        if let Some(op_start_instant) = is_perf_enabled {
            report_rocksdb_write_perf(
                C::NAME,
                PERF_METRIC_OP_NAME_PUT,
                &op_start_instant.elapsed(),
                &self.column_options,
            );
        }
        result
    }

    pub fn put_bytes_in_batch(
        &self,
        batch: &mut WriteBatch,
        index: C::Index,
        value: &[u8],
    ) -> Result<()> {
        let key = <C as Column>::key(&index);
        batch.put_cf(self.handle(), key, value)
    }

    /// Retrieves the specified RocksDB integer property of the current
    /// column family.
    ///
    /// Full list of properties that return int values could be found
    /// [here](https://github.com/facebook/rocksdb/blob/08809f5e6cd9cc4bc3958dd4d59457ae78c76660/include/rocksdb/db.h#L654-L689).
    pub fn get_int_property(&self, name: &'static std::ffi::CStr) -> Result<i64> {
        self.backend.get_int_property_cf(self.handle(), name)
    }

    pub fn delete(&self, index: C::Index) -> Result<()> {
        let is_perf_enabled = maybe_enable_rocksdb_perf(
            self.column_options.rocks_perf_sample_interval,
            &self.write_perf_status,
        );

        let key = <C as Column>::key(&index);
        let result = self.backend.delete_cf(self.handle(), key);

        if let Some(op_start_instant) = is_perf_enabled {
            report_rocksdb_write_perf(
                C::NAME,
                "delete",
                &op_start_instant.elapsed(),
                &self.column_options,
            );
        }
        result
    }

    pub fn delete_in_batch(&self, batch: &mut WriteBatch, index: C::Index) -> Result<()> {
        let key = <C as Column>::key(&index);
        batch.delete_cf(self.handle(), key)
    }

    /// Adds a \[`from`, `to`\] range that deletes all entries between the `from` slot
    /// and `to` slot inclusively.  If `from` slot and `to` slot are the same, then all
    /// entries in that slot will be removed.
    pub fn delete_range_in_batch(&self, batch: &mut WriteBatch, from: Slot, to: Slot) -> Result<()>
    where
        C: Column + ColumnName,
    {
        // Note that the default behavior of rocksdb's delete_range_cf deletes
        // files within [from, to), while our purge logic applies to [from, to].
        //
        // For consistency, we make our delete_range_cf works for [from, to] by
        // adjusting the `to` slot range by 1.
        let from_key = <C as Column>::key(&C::as_index(from));
        let to_key = <C as Column>::key(&C::as_index(to.saturating_add(1)));
        batch.delete_range_cf(self.handle(), from_key, to_key)
    }

    /// Delete files whose slot range is within \[`from`, `to`\].
    pub fn delete_file_in_range(&self, from: Slot, to: Slot) -> Result<()>
    where
        C: Column + ColumnName,
    {
        let from_key = <C as Column>::key(&C::as_index(from));
        let to_key = <C as Column>::key(&C::as_index(to));
        self.backend
            .delete_file_in_range_cf(self.handle(), from_key, to_key)
    }
}

impl<C> LedgerColumn<C>
where
    C: TypedColumn + ColumnName,
{
    pub(crate) fn multi_get<'a, K>(
        &'a self,
        keys: impl IntoIterator<Item = &'a K> + 'a,
    ) -> impl Iterator<Item = Result<Option<C::Type>>> + 'a
    where
        K: AsRef<[u8]> + 'a + ?Sized,
    {
        let is_perf_enabled = maybe_enable_rocksdb_perf(
            self.column_options.rocks_perf_sample_interval,
            &self.read_perf_status,
        );

        let result = self
            .backend
            .multi_get_cf(self.handle(), keys)
            .map(|out| out?.as_deref().map(C::deserialize).transpose());

        if let Some(op_start_instant) = is_perf_enabled {
            // use multi-get instead
            report_rocksdb_read_perf(
                C::NAME,
                PERF_METRIC_OP_NAME_MULTI_GET,
                &op_start_instant.elapsed(),
                &self.column_options,
            );
        }

        result
    }

    pub fn get(&self, index: C::Index) -> Result<Option<C::Type>> {
        let key = <C as Column>::key(&index);
        self.get_raw(key)
    }

    pub fn get_raw<K: AsRef<[u8]>>(&self, key: K) -> Result<Option<C::Type>> {
        let mut result = Ok(None);
        let is_perf_enabled = maybe_enable_rocksdb_perf(
            self.column_options.rocks_perf_sample_interval,
            &self.read_perf_status,
        );
        if let Some(pinnable_slice) = self.backend.get_pinned_cf(self.handle(), key)? {
            let value = C::deserialize(pinnable_slice.as_ref())?;
            result = Ok(Some(value))
        }

        if let Some(op_start_instant) = is_perf_enabled {
            report_rocksdb_read_perf(
                C::NAME,
                PERF_METRIC_OP_NAME_GET,
                &op_start_instant.elapsed(),
                &self.column_options,
            );
        }
        result
    }

    pub fn put(&self, index: C::Index, value: &C::Type) -> Result<()> {
        let is_perf_enabled = maybe_enable_rocksdb_perf(
            self.column_options.rocks_perf_sample_interval,
            &self.write_perf_status,
        );
        let serialized_value = C::serialize(value)?;

        let key = <C as Column>::key(&index);
        let result = self.backend.put_cf(self.handle(), key, &serialized_value);

        if let Some(op_start_instant) = is_perf_enabled {
            report_rocksdb_write_perf(
                C::NAME,
                PERF_METRIC_OP_NAME_PUT,
                &op_start_instant.elapsed(),
                &self.column_options,
            );
        }
        result
    }

    pub fn put_in_batch(
        &self,
        batch: &mut WriteBatch,
        index: C::Index,
        value: &C::Type,
    ) -> Result<()> {
        let key = <C as Column>::key(&index);
        let serialized_value = C::serialize(value)?;
        batch.put_cf(self.handle(), key, &serialized_value)
    }
}

impl<C> LedgerColumn<C>
where
    C: ProtobufColumn + ColumnName,
{
    pub fn get_protobuf_or_bincode<T: DeserializeOwned + Into<C::Type>>(
        &self,
        index: C::Index,
    ) -> Result<Option<C::Type>> {
        let key = <C as Column>::key(&index);
        self.get_raw_protobuf_or_bincode::<T>(key)
    }

    pub(crate) fn get_raw_protobuf_or_bincode<T: DeserializeOwned + Into<C::Type>>(
        &self,
        key: impl AsRef<[u8]>,
    ) -> Result<Option<C::Type>> {
        let is_perf_enabled = maybe_enable_rocksdb_perf(
            self.column_options.rocks_perf_sample_interval,
            &self.read_perf_status,
        );
        let result = self.backend.get_pinned_cf(self.handle(), key);
        if let Some(op_start_instant) = is_perf_enabled {
            report_rocksdb_read_perf(
                C::NAME,
                PERF_METRIC_OP_NAME_GET,
                &op_start_instant.elapsed(),
                &self.column_options,
            );
        }

        if let Some(pinnable_slice) = result? {
            let value = match C::Type::decode(pinnable_slice.as_ref()) {
                Ok(value) => value,
                Err(_) => deserialize::<T>(pinnable_slice.as_ref())?.into(),
            };
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }

    pub fn get_protobuf(&self, index: C::Index) -> Result<Option<C::Type>> {
        let is_perf_enabled = maybe_enable_rocksdb_perf(
            self.column_options.rocks_perf_sample_interval,
            &self.read_perf_status,
        );

        let key = <C as Column>::key(&index);
        let result = self.backend.get_pinned_cf(self.handle(), key);

        if let Some(op_start_instant) = is_perf_enabled {
            report_rocksdb_read_perf(
                C::NAME,
                PERF_METRIC_OP_NAME_GET,
                &op_start_instant.elapsed(),
                &self.column_options,
            );
        }

        if let Some(pinnable_slice) = result? {
            Ok(Some(C::Type::decode(pinnable_slice.as_ref())?))
        } else {
            Ok(None)
        }
    }

    pub fn put_protobuf(&self, index: C::Index, value: &C::Type) -> Result<()> {
        let mut buf = Vec::with_capacity(value.encoded_len());
        value.encode(&mut buf)?;

        let is_perf_enabled = maybe_enable_rocksdb_perf(
            self.column_options.rocks_perf_sample_interval,
            &self.write_perf_status,
        );

        let key = <C as Column>::key(&index);
        let result = self.backend.put_cf(self.handle(), key, &buf);

        if let Some(op_start_instant) = is_perf_enabled {
            report_rocksdb_write_perf(
                C::NAME,
                PERF_METRIC_OP_NAME_PUT,
                &op_start_instant.elapsed(),
                &self.column_options,
            );
        }

        result
    }
}

impl<C> LedgerColumn<C>
where
    C: ColumnIndexDeprecation + ColumnName,
{
    pub(crate) fn iter_current_index_filtered(
        &self,
        iterator_mode: IteratorMode<C::Index>,
    ) -> Result<impl Iterator<Item = (C::Index, Box<[u8]>)> + '_> {
        let start_key: <C as Column>::Key;
        let iterator_mode = match iterator_mode {
            IteratorMode::Start => RocksIteratorMode::Start,
            IteratorMode::End => RocksIteratorMode::End,
            IteratorMode::From(start, direction) => {
                start_key = <C as Column>::key(&start);
                RocksIteratorMode::From(start_key.as_ref(), direction)
            }
        };

        let iter = self.backend.iterator_cf(self.handle(), iterator_mode);
        Ok(iter.filter_map(|pair| {
            let (key, value) = pair.unwrap();
            C::try_current_index(&key).ok().map(|index| (index, value))
        }))
    }

    pub(crate) fn iter_deprecated_index_filtered(
        &self,
        iterator_mode: IteratorMode<C::DeprecatedIndex>,
    ) -> Result<impl Iterator<Item = (C::DeprecatedIndex, Box<[u8]>)> + '_> {
        let start_key: <C as ColumnIndexDeprecation>::DeprecatedKey;
        let iterator_mode = match iterator_mode {
            IteratorMode::Start => RocksIteratorMode::Start,
            IteratorMode::End => RocksIteratorMode::End,
            IteratorMode::From(start_from, direction) => {
                start_key = C::deprecated_key(start_from);
                RocksIteratorMode::From(start_key.as_ref(), direction)
            }
        };

        let iterator = self.backend.iterator_cf(self.handle(), iterator_mode);
        Ok(iterator.filter_map(|pair| {
            let (key, value) = pair.unwrap();
            C::try_deprecated_index(&key)
                .ok()
                .map(|index| (index, value))
        }))
    }

    pub(crate) fn delete_deprecated_in_batch(
        &self,
        batch: &mut WriteBatch,
        index: C::DeprecatedIndex,
    ) -> Result<()> {
        let key = C::deprecated_key(index);
        batch.delete_cf(self.handle(), &key)
    }
}

/// A CompactionFilter implementation to remove keys older than a given slot.
struct PurgedSlotFilter<C: Column + ColumnName> {
    /// The oldest slot to keep; any slot < oldest_slot will be removed
    oldest_slot: Slot,
    /// Whether to preserve keys that return slot 0, even when oldest_slot > 0.
    // This is used to delete old column data that wasn't keyed with a Slot, and so always returns
    // `C::slot() == 0`
    clean_slot_0: bool,
    name: CString,
    _phantom: PhantomData<C>,
}

impl<C: Column + ColumnName> CompactionFilter for PurgedSlotFilter<C> {
    fn filter(&mut self, _level: u32, key: &[u8], _value: &[u8]) -> CompactionDecision {
        use rocksdb::CompactionDecision::*;

        let slot_in_key = C::slot(C::index(key));
        if slot_in_key >= self.oldest_slot || (slot_in_key == 0 && !self.clean_slot_0) {
            Keep
        } else {
            Remove
        }
    }

    fn name(&self) -> &CStr {
        &self.name
    }
}

struct PurgedSlotFilterFactory<C: Column + ColumnName> {
    oldest_slot: OldestSlot,
    name: CString,
    _phantom: PhantomData<C>,
}

impl<C: Column + ColumnName> CompactionFilterFactory for PurgedSlotFilterFactory<C> {
    type Filter = PurgedSlotFilter<C>;

    fn create(&mut self, _context: CompactionFilterContext) -> Self::Filter {
        let copied_oldest_slot = self.oldest_slot.get();
        let copied_clean_slot_0 = self.oldest_slot.get_clean_slot_0();
        PurgedSlotFilter::<C> {
            oldest_slot: copied_oldest_slot,
            clean_slot_0: copied_clean_slot_0,
            name: CString::new(format!(
                "purged_slot_filter({}, {:?})",
                C::NAME,
                copied_oldest_slot
            ))
            .unwrap(),
            _phantom: PhantomData,
        }
    }

    fn name(&self) -> &CStr {
        &self.name
    }
}

fn new_cf_descriptor<C: 'static + Column + ColumnName>(
    options: &BlockstoreOptions,
    oldest_slot: &OldestSlot,
) -> ColumnFamilyDescriptor {
    ColumnFamilyDescriptor::new(C::NAME, get_cf_options::<C>(options, oldest_slot))
}

fn get_cf_options<C: 'static + Column + ColumnName>(
    options: &BlockstoreOptions,
    oldest_slot: &OldestSlot,
) -> Options {
    let mut cf_options = Options::default();
    // 256 * 8 = 2GB. 6 of these columns should take at most 12GB of RAM
    cf_options.set_max_write_buffer_number(8);
    cf_options.set_write_buffer_size(MAX_WRITE_BUFFER_SIZE as usize);
    let file_num_compaction_trigger = 4;
    // Recommend that this be around the size of level 0. Level 0 estimated size in stable state is
    // write_buffer_size * min_write_buffer_number_to_merge * level0_file_num_compaction_trigger
    // Source: https://docs.rs/rocksdb/0.6.0/rocksdb/struct.Options.html#method.set_level_zero_file_num_compaction_trigger
    let total_size_base = MAX_WRITE_BUFFER_SIZE * file_num_compaction_trigger;
    let file_size_base = total_size_base / 10;
    cf_options.set_level_zero_file_num_compaction_trigger(file_num_compaction_trigger as i32);
    cf_options.set_max_bytes_for_level_base(total_size_base);
    cf_options.set_target_file_size_base(file_size_base);

    let disable_auto_compactions = should_disable_auto_compactions(&options.access_type);
    if disable_auto_compactions {
        cf_options.set_disable_auto_compactions(true);
    }

    if !disable_auto_compactions && should_enable_cf_compaction(C::NAME) {
        cf_options.set_compaction_filter_factory(PurgedSlotFilterFactory::<C> {
            oldest_slot: oldest_slot.clone(),
            name: CString::new(format!("purged_slot_filter_factory({})", C::NAME)).unwrap(),
            _phantom: PhantomData,
        });
    }

    process_cf_options_advanced::<C>(&mut cf_options, &options.column_options);

    cf_options
}

fn process_cf_options_advanced<C: 'static + Column + ColumnName>(
    cf_options: &mut Options,
    column_options: &LedgerColumnOptions,
) {
    // Explicitly disable compression on all columns by default
    // See https://docs.rs/rocksdb/0.21.0/rocksdb/struct.Options.html#method.set_compression_type
    cf_options.set_compression_type(DBCompressionType::None);

    if should_enable_compression::<C>() {
        cf_options.set_compression_type(
            column_options
                .compression_type
                .to_rocksdb_compression_type(),
        );
    }
}

fn get_db_options(blockstore_options: &BlockstoreOptions) -> Options {
    let mut options = Options::default();

    // Create missing items to support a clean start
    options.create_if_missing(true);
    options.create_missing_column_families(true);

    // rocksdb builds two threadpools: low and high priority. The low priority
    // pool is used for compactions whereas the high priority pool is used for
    // memtable flushes. Separate pools are created so that compactions are
    // unable to stall memtable flushes (which could stall memtable writes).
    //
    // For now, use the deprecated methods to configure the exact amount of
    // threads for each pool. The new method, set_max_background_jobs(N),
    // configures N/4 low priority threads and 3N/4 high priority threads.
    #[allow(deprecated)]
    {
        options.set_max_background_compactions(
            blockstore_options.num_rocksdb_compaction_threads.get() as i32,
        );
        options
            .set_max_background_flushes(blockstore_options.num_rocksdb_flush_threads.get() as i32);
    }
    // Set max total wal size to 4G.
    options.set_max_total_wal_size(4 * 1024 * 1024 * 1024);

    if should_disable_auto_compactions(&blockstore_options.access_type) {
        options.set_disable_auto_compactions(true);
    }

    // Limit to (10) 50 MB log files (500 MB total)
    // Logs grow at < 5 MB / hour, so this provides several days of logs
    options.set_max_log_file_size(50 * 1024 * 1024);
    options.set_keep_log_file_num(10);

    // Allow Rocks to open/keep open as many files as it needs for performance;
    // it is not required for read-only access, but should be fine with our high ulimit.
    options.set_max_open_files(-1);

    options
}

/// The default number of threads to use for rocksdb compaction in the rocksdb
/// low priority threadpool
pub fn default_num_compaction_threads() -> NonZeroUsize {
    NonZeroUsize::new(num_cpus::get()).expect("thread count is non-zero")
}

/// The default number of threads to use for rocksdb memtable flushes in the
/// rocksdb high priority threadpool
pub fn default_num_flush_threads() -> NonZeroUsize {
    NonZeroUsize::new((num_cpus::get() / 4).max(1)).expect("thread count is non-zero")
}

// Returns whether automatic compactions should be disabled for the entire
// database based upon the given access type.
fn should_disable_auto_compactions(access_type: &AccessType) -> bool {
    // Leave automatic compactions enabled (do not disable) in Primary mode;
    // disable in all other modes to prevent accidental cleaning
    !matches!(access_type, AccessType::Primary)
}

// Returns whether compactions should be enabled for the given column (name).
fn should_enable_cf_compaction(cf_name: &str) -> bool {
    // In order to keep the ledger storage footprint within a desired size,
    // LedgerCleanupService removes data in FIFO order by slot.
    //
    // Several columns do not contain slot in their key. These columns must
    // be manually managed to avoid unbounded storage growth.
    //
    // Columns where slot is the primary index can be efficiently cleaned via
    // Database::delete_range_cf() && Database::delete_file_in_range_cf().
    //
    // Columns where a slot is part of the key but not the primary index can
    // not be range deleted like above. Instead, the individual key/value pairs
    // must be iterated over and a decision to keep or discard that pair is
    // made. The comparison logic is implemented in PurgedSlotFilter which is
    // configured to run as part of rocksdb's automatic compactions. Storage
    // space is reclaimed on this class of columns once compaction has
    // completed on a given range or file.
    matches!(
        cf_name,
        columns::TransactionStatus::NAME
            | columns::TransactionMemos::NAME
            | columns::AddressSignatures::NAME
    )
}

// Returns true if the column family enables compression.
fn should_enable_compression<C: 'static + Column + ColumnName>() -> bool {
    C::NAME == columns::TransactionStatus::NAME
}

// If the access type is read-only, we don't need to open all of the columns
fn must_open_all_column_families(access_type: &AccessType) -> bool {
    !matches!(access_type, AccessType::ReadOnly)
}

#[cfg(test)]
pub mod tests {
    use {
        super::*, crate::blockstore_db::columns::ShredData, std::path::PathBuf, tempfile::tempdir,
    };

    #[test]
    fn test_compaction_filter() {
        // this doesn't implement Clone...
        let dummy_compaction_filter_context = || CompactionFilterContext {
            is_full_compaction: true,
            is_manual_compaction: true,
        };
        let oldest_slot = OldestSlot::default();
        oldest_slot.set_clean_slot_0(true);

        let mut factory = PurgedSlotFilterFactory::<ShredData> {
            oldest_slot: oldest_slot.clone(),
            name: CString::new("test compaction filter").unwrap(),
            _phantom: PhantomData,
        };
        let mut compaction_filter = factory.create(dummy_compaction_filter_context());

        let dummy_level = 0;
        let key = ShredData::key(&ShredData::as_index(0));
        let dummy_value = vec![];

        // we can't use assert_matches! because CompactionDecision doesn't implement Debug
        assert!(matches!(
            compaction_filter.filter(dummy_level, &key, &dummy_value),
            CompactionDecision::Keep
        ));

        // mutating oldest_slot doesn't affect existing compaction filters...
        oldest_slot.set(1);
        assert!(matches!(
            compaction_filter.filter(dummy_level, &key, &dummy_value),
            CompactionDecision::Keep
        ));

        // recreating compaction filter starts to expire the key
        let mut compaction_filter = factory.create(dummy_compaction_filter_context());
        assert!(matches!(
            compaction_filter.filter(dummy_level, &key, &dummy_value),
            CompactionDecision::Remove
        ));

        // newer key shouldn't be removed
        let key = ShredData::key(&ShredData::as_index(1));
        matches!(
            compaction_filter.filter(dummy_level, &key, &dummy_value),
            CompactionDecision::Keep
        );
    }

    #[test]
    fn test_cf_names_and_descriptors_equal_length() {
        let path = PathBuf::default();
        let options = BlockstoreOptions::default();
        let oldest_slot = OldestSlot::default();
        // The names and descriptors don't need to be in the same order for our use cases;
        // however, there should be the same number of each. For example, adding a new column
        // should update both lists.
        assert_eq!(
            Rocks::columns().len(),
            Rocks::cf_descriptors(&path, &options, &oldest_slot).len()
        );
    }

    #[test]
    fn test_should_disable_auto_compactions() {
        assert!(!should_disable_auto_compactions(&AccessType::Primary));
        assert!(should_disable_auto_compactions(
            &AccessType::PrimaryForMaintenance
        ));
        assert!(should_disable_auto_compactions(&AccessType::ReadOnly));
    }

    #[test]
    fn test_should_enable_cf_compaction() {
        let columns_to_compact = [
            columns::TransactionStatus::NAME,
            columns::AddressSignatures::NAME,
        ];
        columns_to_compact.iter().for_each(|cf_name| {
            assert!(should_enable_cf_compaction(cf_name));
        });
        assert!(!should_enable_cf_compaction("something else"));
    }

    #[test]
    fn test_open_unknown_columns() {
        agave_logger::setup();

        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path();

        // Open with Primary to create the new database
        {
            let options = BlockstoreOptions {
                access_type: AccessType::Primary,
                ..BlockstoreOptions::default()
            };
            let mut rocks = Rocks::open(db_path.to_path_buf(), options).unwrap();

            // Introduce a new column that will not be known
            rocks
                .db
                .create_cf("new_column", &Options::default())
                .unwrap();
        }

        // Opening with any access mode should succeed,
        // even though the Rocks code is unaware of "new_column"
        {
            let options = BlockstoreOptions {
                access_type: AccessType::ReadOnly,
                ..BlockstoreOptions::default()
            };
            let _ = Rocks::open(db_path.to_path_buf(), options).unwrap();
        }
        {
            let options = BlockstoreOptions {
                access_type: AccessType::Primary,
                ..BlockstoreOptions::default()
            };
            let _ = Rocks::open(db_path.to_path_buf(), options).unwrap();
        }
    }

    #[test]
    fn test_remove_deprecated_progam_costs_column_compat() {
        agave_logger::setup();

        fn is_program_costs_column_present(path: &Path) -> bool {
            DB::list_cf(&Options::default(), path)
                .unwrap()
                .iter()
                .any(|column_name| column_name == DEPRECATED_PROGRAM_COSTS_COLUMN_NAME)
        }

        let temp_dir = tempdir().unwrap();
        let db_path = temp_dir.path();

        let options = BlockstoreOptions {
            access_type: AccessType::Primary,
            ..BlockstoreOptions::default()
        };

        // Create a new database
        {
            let _rocks = Rocks::open(db_path.to_path_buf(), options.clone()).unwrap();
        }

        // The newly created database should not contain the deprecated column
        assert!(!is_program_costs_column_present(db_path));

        // Create a program_costs column to simulate an old database that had the column
        {
            let mut rocks = Rocks::open(db_path.to_path_buf(), options.clone()).unwrap();
            rocks
                .db
                .create_cf(DEPRECATED_PROGRAM_COSTS_COLUMN_NAME, &Options::default())
                .unwrap();
        }

        // Ensure the column we just created is detected
        assert!(is_program_costs_column_present(db_path));

        // Reopen the database which has logic to delete program_costs column
        {
            let _rocks = Rocks::open(db_path.to_path_buf(), options.clone()).unwrap();
        }

        // The deprecated column should have been dropped by Rocks::open()
        assert!(!is_program_costs_column_present(db_path));
    }

    impl<C> LedgerColumn<C>
    where
        C: ColumnIndexDeprecation + ProtobufColumn + ColumnName,
    {
        pub fn put_deprecated_protobuf(
            &self,
            index: C::DeprecatedIndex,
            value: &C::Type,
        ) -> Result<()> {
            let mut buf = Vec::with_capacity(value.encoded_len());
            value.encode(&mut buf)?;
            self.backend
                .put_cf(self.handle(), C::deprecated_key(index), &buf)
        }
    }

    impl<C> LedgerColumn<C>
    where
        C: ColumnIndexDeprecation + TypedColumn + ColumnName,
    {
        pub fn put_deprecated(&self, index: C::DeprecatedIndex, value: &C::Type) -> Result<()> {
            let serialized_value = C::serialize(value)?;
            self.backend
                .put_cf(self.handle(), C::deprecated_key(index), &serialized_value)
        }
    }
}
