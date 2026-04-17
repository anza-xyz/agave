use {
    super::{ArchiveFormat, SnapshotInterval, SnapshotVersion, ZstdConfig},
    std::{
        num::{NonZeroU64, NonZeroUsize},
        path::{Path, PathBuf},
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

/// Snapshot configuration and runtime information
#[derive(Clone, Debug)]
pub struct SnapshotConfig {
    /// Specifies the ways that snapshots are allowed to be used
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

    /// A new snapshot config used for loading at startup and generating
    /// snapshots during runtime. Snapshot generation intervals are disabled
    /// to indicate that snapshots will be generated externally.
    pub fn new_generate_snapshots_externally() -> Self {
        Self {
            usage: SnapshotUsage::LoadAndGenerate,
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

    /// Constructs a `SnapshotConfig` from three directory paths, using defaults for all other
    /// fields. Intended for use in tests where only the directory layout matters.
    pub fn new_from_paths(
        bank_snapshots_dir: impl AsRef<Path>,
        full_snapshot_archives_dir: impl AsRef<Path>,
        incremental_snapshot_archives_dir: impl AsRef<Path>,
    ) -> Self {
        Self {
            bank_snapshots_dir: bank_snapshots_dir.as_ref().to_path_buf(),
            full_snapshot_archives_dir: full_snapshot_archives_dir.as_ref().to_path_buf(),
            incremental_snapshot_archives_dir: incremental_snapshot_archives_dir
                .as_ref()
                .to_path_buf(),
            ..Self::default()
        }
    }

    /// Returns this config with `bank_snapshots_dir` replaced.
    pub fn with_bank_snapshots_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.bank_snapshots_dir = dir.as_ref().to_path_buf();
        self
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_load_only() {
        let cfg = SnapshotConfig::new_load_only();

        assert!(!cfg.should_generate_snapshots());
        assert!(cfg.should_load_snapshots());

        assert_eq!(
            cfg.full_snapshot_archive_interval,
            SnapshotInterval::Disabled
        );
        assert_eq!(
            cfg.incremental_snapshot_archive_interval,
            SnapshotInterval::Disabled
        );
    }

    #[test]
    fn test_new_disabled() {
        let cfg = SnapshotConfig::new_disabled();

        assert!(!cfg.should_generate_snapshots());
        assert!(!cfg.should_load_snapshots());

        assert_eq!(
            cfg.full_snapshot_archive_interval,
            SnapshotInterval::Disabled
        );
        assert_eq!(
            cfg.incremental_snapshot_archive_interval,
            SnapshotInterval::Disabled
        );
    }

    #[test]
    fn test_new_generate_snapshots_externally() {
        let cfg = SnapshotConfig::new_generate_snapshots_externally();

        // Even though intervals are disabled (snapshots handled externally),
        // usage indicates load+generate semantics.
        assert_eq!(cfg.usage, SnapshotUsage::LoadAndGenerate);
        assert!(cfg.should_generate_snapshots());
        assert!(cfg.should_load_snapshots());

        assert_eq!(
            cfg.full_snapshot_archive_interval,
            SnapshotInterval::Disabled
        );
        assert_eq!(
            cfg.incremental_snapshot_archive_interval,
            SnapshotInterval::Disabled
        );
    }
}
