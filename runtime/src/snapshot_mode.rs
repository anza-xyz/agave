use {
    crate::{
        snapshot_bank_utils,
        snapshot_utils::{self, ArchiveFormat, SnapshotVersion, ZstdConfig},
    },
    std::{num::NonZeroUsize, path::PathBuf},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SnapshotStorageConfig {
    /// Path to the directory where snapshot archives are stored
    pub archives_dir: PathBuf,
    /// Maximum number of snapshot archives to retain
    pub archives_to_retain: NonZeroUsize,
}

impl SnapshotStorageConfig {
    /// Provides a default full snapshot configuration.
    /// Empty `archives_dir` paths are intended for testing purposes and are otherwise already populated
    /// with the exception of a few instances where an empty path placeholder is required.
    pub fn default_full_snapshot_config() -> Self {
        Self {
            archives_dir: PathBuf::default(),
            archives_to_retain: snapshot_utils::DEFAULT_MAX_FULL_SNAPSHOT_ARCHIVES_TO_RETAIN,
        }
    }

    pub fn default_incremental_snapshot_config() -> Self {
        Self {
            archives_dir: PathBuf::default(),
            archives_to_retain: snapshot_utils::DEFAULT_MAX_INCREMENTAL_SNAPSHOT_ARCHIVES_TO_RETAIN,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SnapshotLoadConfig {
    /// Full snapshot storage configuration
    pub full_snapshot_config: SnapshotStorageConfig,
    /// Incremental snapshot storage configuration
    pub incremental_snapshot_config: Option<SnapshotStorageConfig>,
    /// Path to the directory where bank snapshots are stored
    pub bank_snapshots_dir: PathBuf,
    /// The archive format to use for snapshots
    pub archive_format: ArchiveFormat,
    /// Snapshot version to generate
    pub snapshot_version: SnapshotVersion,
}

impl SnapshotLoadConfig {
    pub fn default_load_and_genarate() -> Self {
        Self {
            full_snapshot_config: SnapshotStorageConfig::default_full_snapshot_config(),
            incremental_snapshot_config: Some(
                SnapshotStorageConfig::default_incremental_snapshot_config(),
            ),
            bank_snapshots_dir: PathBuf::default(),
            archive_format: ArchiveFormat::TarZstd {
                config: ZstdConfig::default(),
            },
            snapshot_version: SnapshotVersion::default(),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SnapshotGenerateConfig {
    /// Generate a new full snapshot archive every this many slots
    pub full_snapshot_archive_interval_slots: NonZeroUsize,
    /// Generate a new incremental snapshot archive every this many slots
    pub incremental_snapshot_archive_interval_slots: Option<NonZeroUsize>,
    /// Thread niceness adjustment for snapshot packager service
    pub packager_thread_niceness_adj: i8,
}

impl SnapshotGenerateConfig {
    pub fn default_generate_config() -> Self {
        Self {
            full_snapshot_archive_interval_slots: NonZeroUsize::new(
                snapshot_bank_utils::DEFAULT_FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS as usize,
            )
            .unwrap(),
            incremental_snapshot_archive_interval_slots: Some(
                NonZeroUsize::new(
                    snapshot_bank_utils::DEFAULT_INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS
                        as usize,
                )
                .unwrap(),
            ),
            packager_thread_niceness_adj: 0,
        }
    }
}

/// Specify the ways that snapshots are allowed to be used
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SnapshotMode {
    Disabled,

    /// Snapshots are only used at startup, to load the accounts and bank
    LoadOnly {
        load: SnapshotLoadConfig,
    },
    /// Snapshots are used everywhere; both at startup (i.e. load) and steady-state (i.e.
    /// generate).  This enables taking snapshots.
    LoadAndGenerate {
        load: SnapshotLoadConfig,
        generate: SnapshotGenerateConfig,
    },
}

impl Default for SnapshotMode {
    fn default() -> Self {
        Self::LoadAndGenerate {
            load: SnapshotLoadConfig::default_load_and_genarate(),
            generate: SnapshotGenerateConfig::default_generate_config(),
        }
    }
}

impl SnapshotMode {
    /// A new snapshot mode used for only loading at startup
    #[must_use]
    pub fn new_load_only() -> Self {
        Self::default()
    }

    /// Should snapshots be generated?
    #[must_use]
    pub fn should_generate_snapshots(&self) -> bool {
        *self == Self::default()
    }
}
