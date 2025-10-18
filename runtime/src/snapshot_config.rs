use {
    agave_snapshots::{ArchiveFormat, SnapshotInterval, ZstdConfig},
    std::{
        fmt,
        num::{NonZeroU64, NonZeroUsize},
        path::PathBuf,
        str::FromStr,
    },
};

pub const DEFAULT_FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS: NonZeroU64 =
    NonZeroU64::new(100_000).unwrap();
pub const DEFAULT_INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS: NonZeroU64 =
    NonZeroU64::new(100).unwrap();
pub const DEFAULT_MAX_FULL_SNAPSHOT_ARCHIVES_TO_RETAIN: NonZeroUsize =
    NonZeroUsize::new(2).unwrap();
pub const DEFAULT_MAX_INCREMENTAL_SNAPSHOT_ARCHIVES_TO_RETAIN: NonZeroUsize =
    NonZeroUsize::new(4).unwrap();

const VERSION_STRING_V1_2_0: &str = "1.2.0";

/// Snapshot configuration and runtime information
#[derive(Clone, Debug)]
pub struct SnapshotConfig {
    /// Specifies the ways thats snapshots are allowed to be used
    pub usage: SnapshotUsage,

    /// Generate a new full snapshot archive every this many slots
    pub full_snapshot_archive_interval: SnapshotInterval,

    /// Generate a new incremental snapshot archive every this many slots
    pub incremental_snapshot_archive_interval: SnapshotInterval,

    /// Path to the directory where full snapshot archives are stored
    pub full_snapshot_archives_dir: PathBuf,

    /// Path to the directory where incremental snapshot archives are stored
    pub incremental_snapshot_archives_dir: PathBuf,

    /// Path to the directory where bank snapshots are stored
    pub bank_snapshots_dir: PathBuf,

    /// The archive format to use for snapshots
    pub archive_format: ArchiveFormat,

    /// Snapshot version to generate
    pub snapshot_version: SnapshotVersion,

    /// Maximum number of full snapshot archives to retain
    pub maximum_full_snapshot_archives_to_retain: NonZeroUsize,

    /// Maximum number of incremental snapshot archives to retain
    /// NOTE: Incremental snapshots will only be kept for the latest full snapshot
    pub maximum_incremental_snapshot_archives_to_retain: NonZeroUsize,

    // Thread niceness adjustment for snapshot packager service
    pub packager_thread_niceness_adj: i8,
}

impl Default for SnapshotConfig {
    fn default() -> Self {
        Self {
            usage: SnapshotUsage::LoadAndGenerate,
            full_snapshot_archive_interval: SnapshotInterval::Slots(
                DEFAULT_FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS,
            ),
            incremental_snapshot_archive_interval: SnapshotInterval::Slots(
                DEFAULT_INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS,
            ),
            full_snapshot_archives_dir: PathBuf::default(),
            incremental_snapshot_archives_dir: PathBuf::default(),
            bank_snapshots_dir: PathBuf::default(),
            archive_format: ArchiveFormat::TarZstd {
                config: ZstdConfig::default(),
            },
            snapshot_version: SnapshotVersion::default(),
            maximum_full_snapshot_archives_to_retain: DEFAULT_MAX_FULL_SNAPSHOT_ARCHIVES_TO_RETAIN,
            maximum_incremental_snapshot_archives_to_retain:
                DEFAULT_MAX_INCREMENTAL_SNAPSHOT_ARCHIVES_TO_RETAIN,
            packager_thread_niceness_adj: 0,
        }
    }
}

impl SnapshotConfig {
    /// A new snapshot config used for only loading at startup
    pub fn new_load_only() -> Self {
        Self {
            usage: SnapshotUsage::LoadOnly,
            full_snapshot_archive_interval: SnapshotInterval::Disabled,
            incremental_snapshot_archive_interval: SnapshotInterval::Disabled,
            ..Self::default()
        }
    }

    /// A new snapshot config used to disable snapshot generation and loading at
    /// startup
    pub fn new_disabled() -> Self {
        Self {
            usage: SnapshotUsage::Disabled,
            full_snapshot_archive_interval: SnapshotInterval::Disabled,
            incremental_snapshot_archive_interval: SnapshotInterval::Disabled,
            ..Self::default()
        }
    }

    /// Should snapshots be generated?
    pub fn should_generate_snapshots(&self) -> bool {
        self.usage == SnapshotUsage::LoadAndGenerate
    }

    /// Should snapshots be loaded?
    pub fn should_load_snapshots(&self) -> bool {
        self.usage == SnapshotUsage::LoadAndGenerate || self.usage == SnapshotUsage::LoadOnly
    }
}

#[derive(Copy, Clone, Default, Eq, PartialEq, Debug)]
pub enum SnapshotVersion {
    #[default]
    V1_2_0,
}

impl fmt::Display for SnapshotVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(From::from(*self))
    }
}

impl From<SnapshotVersion> for &'static str {
    fn from(snapshot_version: SnapshotVersion) -> &'static str {
        match snapshot_version {
            SnapshotVersion::V1_2_0 => VERSION_STRING_V1_2_0,
        }
    }
}

impl FromStr for SnapshotVersion {
    type Err = &'static str;

    fn from_str(version_string: &str) -> Result<Self, Self::Err> {
        // Remove leading 'v' or 'V' from slice
        let version_string = if version_string
            .get(..1)
            .is_some_and(|s| s.eq_ignore_ascii_case("v"))
        {
            &version_string[1..]
        } else {
            version_string
        };
        match version_string {
            VERSION_STRING_V1_2_0 => Ok(SnapshotVersion::V1_2_0),
            _ => Err("unsupported snapshot version"),
        }
    }
}

impl SnapshotVersion {
    pub fn as_str(self) -> &'static str {
        <&str as From<Self>>::from(self)
    }
}

/// Specify the ways that snapshots are allowed to be used
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SnapshotUsage {
    /// Snapshots are never generated or loaded at startup,
    /// instead start from genesis.
    Disabled,
    /// Snapshots are only used at startup, to load the accounts and bank
    LoadOnly,
    /// Snapshots are used everywhere; both at startup (i.e. load) and steady-state (i.e.
    /// generate).  This enables taking snapshots.
    LoadAndGenerate,
}
