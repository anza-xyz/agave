use {
    crate::blockstore_db::{default_num_compaction_threads, default_num_flush_threads},
    rocksdb::{DBCompressionType as RocksCompressionType, DBRecoveryMode},
    std::num::NonZeroUsize,
};

/// The subdirectory under ledger directory where the Blockstore lives
pub const BLOCKSTORE_DIRECTORY_ROCKS_LEVEL: &str = "rocksdb";

#[derive(Debug, Clone, PartialEq)]
pub struct BlockstoreOptions {
    // The access type of blockstore. Default: Primary
    pub access_type: AccessType,
    // Whether to open a blockstore under a recovery mode. Default: None.
    pub recovery_mode: Option<BlockstoreRecoveryMode>,
    pub column_options: LedgerColumnOptions,
    pub num_rocksdb_compaction_threads: NonZeroUsize,
    pub num_rocksdb_flush_threads: NonZeroUsize,
}

impl Default for BlockstoreOptions {
    /// The default options are the values used by [`Blockstore::open`].
    ///
    /// [`Blockstore::open`]: crate::blockstore::Blockstore::open
    fn default() -> Self {
        Self {
            access_type: AccessType::Primary,
            recovery_mode: None,
            column_options: LedgerColumnOptions::default(),
            num_rocksdb_compaction_threads: default_num_compaction_threads(),
            num_rocksdb_flush_threads: default_num_flush_threads(),
        }
    }
}

impl BlockstoreOptions {
    pub fn default_for_tests() -> Self {
        BlockstoreOptions::default()
    }
}

/// The mode to open a Blockstore with. For more details, see:
/// https://github.com/facebook/rocksdb/wiki/Read-only-and-Secondary-instances
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccessType {
    /// Primary (read/write) access; only one process can have Primary access.
    Primary,
    /// Primary (read/write) access with RocksDB automatic compaction disabled.
    PrimaryForMaintenance,
    /// Read only access; multiple processes can obtain ReadOnly access.
    /// ReadOnly instance gets a static view of the database at creation time.
    ReadOnly,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BlockstoreRecoveryMode {
    TolerateCorruptedTailRecords,
    AbsoluteConsistency,
    PointInTime,
    SkipAnyCorruptedRecord,
}

impl From<&str> for BlockstoreRecoveryMode {
    fn from(string: &str) -> Self {
        match string {
            "tolerate_corrupted_tail_records" => {
                BlockstoreRecoveryMode::TolerateCorruptedTailRecords
            }
            "absolute_consistency" => BlockstoreRecoveryMode::AbsoluteConsistency,
            "point_in_time" => BlockstoreRecoveryMode::PointInTime,
            "skip_any_corrupted_record" => BlockstoreRecoveryMode::SkipAnyCorruptedRecord,
            bad_mode => panic!("Invalid recovery mode: {bad_mode}"),
        }
    }
}

impl From<BlockstoreRecoveryMode> for DBRecoveryMode {
    fn from(brm: BlockstoreRecoveryMode) -> Self {
        match brm {
            BlockstoreRecoveryMode::TolerateCorruptedTailRecords => {
                DBRecoveryMode::TolerateCorruptedTailRecords
            }
            BlockstoreRecoveryMode::AbsoluteConsistency => DBRecoveryMode::AbsoluteConsistency,
            BlockstoreRecoveryMode::PointInTime => DBRecoveryMode::PointInTime,
            BlockstoreRecoveryMode::SkipAnyCorruptedRecord => {
                DBRecoveryMode::SkipAnyCorruptedRecord
            }
        }
    }
}

/// Options for LedgerColumn.
/// Each field might also be used as a tag that supports group-by operation when
/// reporting metrics.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct LedgerColumnOptions {
    // Determine the way to compress column families which are eligible for
    // compression.
    pub compression_type: BlockstoreCompressionType,

    // Control how often RocksDB read/write performance samples are collected.
    // If the value is greater than 0, then RocksDB read/write perf sample
    // will be collected once for every `rocks_perf_sample_interval` ops.
    pub rocks_perf_sample_interval: usize,
}

impl LedgerColumnOptions {
    pub fn get_compression_type_string(&self) -> &'static str {
        match self.compression_type {
            BlockstoreCompressionType::None => "None",
            BlockstoreCompressionType::Snappy => "Snappy",
            BlockstoreCompressionType::Lz4 => "Lz4",
            BlockstoreCompressionType::Zlib => "Zlib",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum BlockstoreCompressionType {
    #[default]
    None,
    Snappy,
    Lz4,
    Zlib,
}

impl BlockstoreCompressionType {
    pub(crate) fn to_rocksdb_compression_type(&self) -> RocksCompressionType {
        match self {
            Self::None => RocksCompressionType::None,
            Self::Snappy => RocksCompressionType::Snappy,
            Self::Lz4 => RocksCompressionType::Lz4,
            Self::Zlib => RocksCompressionType::Zlib,
        }
    }
}
