#[cfg(feature = "dev-context-only-utils")]
use solana_accounts_db::utils::create_accounts_run_and_snapshot_dirs;
use {
    crate::{
        bank::BankFieldsToDeserialize,
        serde_snapshot::{
            self, AccountsDbFields, ExtraFieldsToSerialize, SerdeObsoleteAccountsMap,
            SerializableAccountStorageEntry, SnapshotAccountsDbFields, SnapshotBankFields,
            SnapshotStreams,
        },
        snapshot_package::BankSnapshotPackage,
        snapshot_utils::snapshot_storage_rebuilder::{
            get_slot_and_append_vec_id, SnapshotStorageRebuilder,
        },
    },
    agave_fs::buffered_writer::large_file_buf_writer,
    agave_snapshots::{
        archive_snapshot,
        error::{
            AddBankSnapshotError, GetSnapshotAccountsHardLinkDirError,
            HardLinkStoragesToSnapshotError, SnapshotError, SnapshotFastbootError,
            SnapshotNewFromDirError,
        },
        paths::{self as snapshot_paths, get_incremental_snapshot_archives},
        snapshot_archive_info::{
            FullSnapshotArchiveInfo, IncrementalSnapshotArchiveInfo, SnapshotArchiveInfo,
            SnapshotArchiveInfoGetter,
        },
        snapshot_config::SnapshotConfig,
        snapshot_hash::SnapshotHash,
        streaming_unarchive_snapshot, ArchiveFormat, Result, SnapshotArchiveKind, SnapshotVersion,
    },
    crossbeam_channel::{Receiver, Sender},
    log::*,
    regex::Regex,
    semver::Version,
    solana_accounts_db::{
        account_storage::AccountStorageMap,
        accounts_db::{AccountStorageEntry, AccountsDbConfig, AtomicAccountsFileId},
        accounts_file::{AccountsFile, StorageAccess},
        utils::{move_and_async_delete_path, ACCOUNTS_RUN_DIR, ACCOUNTS_SNAPSHOT_DIR},
    },
    solana_clock::Slot,
    solana_measure::{measure::Measure, measure_time, measure_us},
    std::{
        cmp::Ordering,
        collections::{HashMap, HashSet},
        fs,
        io::{self, BufReader, Error as IoError, Read, Seek, Write},
        mem,
        num::NonZeroUsize,
        path::{Path, PathBuf},
        str::FromStr,
        sync::{Arc, LazyLock},
    },
    tempfile::TempDir,
};

pub mod snapshot_storage_rebuilder;

/// Limit the size of the obsolete accounts file
/// If it exceeds this limit, remove the file which will force restore from archives
/// Limit is set assuming 24 bytes per entry, 5% of 10 billion accounts
/// = 500 million entries * 24 bytes = 12 GB
pub const MAX_OBSOLETE_ACCOUNTS_FILE_SIZE: u64 = 1024 * 1024 * 1024 * 12; // 12 GB
pub const MAX_SNAPSHOT_DATA_FILE_SIZE: u64 = 32 * 1024 * 1024 * 1024; // 32 GiB
const MAX_SNAPSHOT_VERSION_FILE_SIZE: u64 = 8; // byte

// Snapshot Fastboot Version History
// Legacy - No fastboot version file, storages flushed file presence determines if snapshot is loadable
// 1.0.0 - Initial version file. Backwards and forwards compatible with Legacy.
// 2.0.0 - Obsolete Accounts File added, storages flushed file not written anymore
//         Snapshots created with version 2.0.0 will not fastboot to older versions
//         Snapshots created with versions <2.0.0 will fastboot to version 2.0.0
const SNAPSHOT_FASTBOOT_VERSION: Version = Version::new(2, 0, 0);

/// Information about a bank snapshot. Namely the slot of the bank, the path to the snapshot, and
/// the kind of the snapshot.
#[derive(PartialEq, Eq, Debug)]
pub struct BankSnapshotInfo {
    /// Slot of the bank
    pub slot: Slot,
    /// Path to the bank snapshot directory
    pub snapshot_dir: PathBuf,
    /// Snapshot version
    pub snapshot_version: SnapshotVersion,
    /// Fastboot version
    pub fastboot_version: Option<Version>,
}

impl PartialOrd for BankSnapshotInfo {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// Order BankSnapshotInfo by slot (ascending), which practically is sorting chronologically
impl Ord for BankSnapshotInfo {
    fn cmp(&self, other: &Self) -> Ordering {
        self.slot.cmp(&other.slot)
    }
}

impl BankSnapshotInfo {
    pub fn new_from_dir(
        bank_snapshots_dir: impl AsRef<Path>,
        slot: Slot,
    ) -> std::result::Result<BankSnapshotInfo, SnapshotNewFromDirError> {
        // check this directory to see if there is a BankSnapshotPre and/or
        // BankSnapshotPost file
        let bank_snapshot_dir = snapshot_paths::get_bank_snapshot_dir(&bank_snapshots_dir, slot);

        if !bank_snapshot_dir.is_dir() {
            return Err(SnapshotNewFromDirError::InvalidBankSnapshotDir(
                bank_snapshot_dir,
            ));
        }

        // Among the files checks, the completion flag file check should be done first to avoid the later
        // I/O errors.

        // There is a time window from the slot directory being created, and the content being completely
        // filled.  Check the version file as it is the last file written to avoid using a highest
        // found slot directory with missing content
        let version_path = bank_snapshot_dir.join(snapshot_paths::SNAPSHOT_VERSION_FILENAME);
        let version_str = snapshot_version_from_file(&version_path).map_err(|err| {
            SnapshotNewFromDirError::IncompleteDir(err, bank_snapshot_dir.clone())
        })?;

        let snapshot_version = SnapshotVersion::from_str(version_str.as_str())
            .or(Err(SnapshotNewFromDirError::InvalidVersion(version_str)))?;

        let status_cache_file =
            bank_snapshot_dir.join(snapshot_paths::SNAPSHOT_STATUS_CACHE_FILENAME);
        if !status_cache_file.is_file() {
            return Err(SnapshotNewFromDirError::MissingStatusCacheFile(
                status_cache_file,
            ));
        }

        let bank_snapshot_path =
            bank_snapshot_dir.join(snapshot_paths::get_snapshot_file_name(slot));
        if !bank_snapshot_path.is_file() {
            return Err(SnapshotNewFromDirError::MissingSnapshotFile(
                bank_snapshot_dir,
            ));
        };

        let snapshot_fastboot_version_path =
            bank_snapshot_dir.join(snapshot_paths::SNAPSHOT_FASTBOOT_VERSION_FILENAME);

        // If the version file is absent, fastboot_version will be None. This allows versions 3.1+
        // to load snapshots created by versions <3.1. In version 3.2, the version file will become
        // mandatory, and its absence can be treated as an error.
        let fastboot_version = fs::read_to_string(&snapshot_fastboot_version_path)
            .ok()
            .map(|version_string| {
                Version::from_str(version_string.trim())
                    .map_err(|_| SnapshotNewFromDirError::InvalidFastbootVersion(version_string))
            })
            .transpose()?;

        Ok(BankSnapshotInfo {
            slot,
            snapshot_dir: bank_snapshot_dir,
            snapshot_version,
            fastboot_version,
        })
    }

    pub fn snapshot_path(&self) -> PathBuf {
        self.snapshot_dir
            .join(snapshot_paths::get_snapshot_file_name(self.slot))
    }
}

/// When constructing a bank a snapshot, traditionally the snapshot was from a snapshot archive.  Now,
/// the snapshot can be from a snapshot directory, or from a snapshot archive.  This is the flag to
/// indicate which.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotFrom {
    /// Build from the snapshot archive
    Archive,
    /// Build directly from the bank snapshot directory
    Dir,
}

/// Helper type when rebuilding from snapshots.  Designed to handle when rebuilding from just a
/// full snapshot, or from both a full snapshot and an incremental snapshot.
#[derive(Debug)]
pub struct SnapshotRootPaths {
    pub full_snapshot_root_file_path: PathBuf,
    pub incremental_snapshot_root_file_path: Option<PathBuf>,
}

/// Helper type to bundle up the results from `unarchive_snapshot()`
#[derive(Debug)]
pub struct UnarchivedSnapshot {
    #[allow(dead_code)]
    unpack_dir: TempDir,
    pub storage: AccountStorageMap,
    pub bank_fields: BankFieldsToDeserialize,
    pub accounts_db_fields: AccountsDbFields<SerializableAccountStorageEntry>,
    pub unpacked_snapshots_dir_and_version: UnpackedSnapshotsDirAndVersion,
    pub measure_untar: Measure,
}

/// Helper type to bundle up the results from `verify_and_unarchive_snapshots()`.
#[derive(Debug)]
pub struct UnarchivedSnapshots {
    pub full_storage: AccountStorageMap,
    pub incremental_storage: Option<AccountStorageMap>,
    pub bank_fields: SnapshotBankFields,
    pub accounts_db_fields: SnapshotAccountsDbFields<SerializableAccountStorageEntry>,
    pub full_unpacked_snapshots_dir_and_version: UnpackedSnapshotsDirAndVersion,
    pub incremental_unpacked_snapshots_dir_and_version: Option<UnpackedSnapshotsDirAndVersion>,
    pub full_measure_untar: Measure,
    pub incremental_measure_untar: Option<Measure>,
    pub next_append_vec_id: AtomicAccountsFileId,
}

/// Guard type that keeps the unpack directories of snapshots alive.
/// Once dropped, the unpack directories are removed.
#[allow(dead_code)]
#[derive(Debug)]
pub struct UnarchivedSnapshotsGuard {
    full_unpack_dir: TempDir,
    incremental_unpack_dir: Option<TempDir>,
}
/// Helper type for passing around the unpacked snapshots dir and the snapshot version together
#[derive(Debug)]
pub struct UnpackedSnapshotsDirAndVersion {
    pub unpacked_snapshots_dir: PathBuf,
    pub snapshot_version: SnapshotVersion,
}

/// Helper type for passing around account storage map and next append vec id
/// for reconstructing accounts from a snapshot
pub(crate) struct StorageAndNextAccountsFileId {
    pub storage: AccountStorageMap,
    pub next_append_vec_id: AtomicAccountsFileId,
}

/// The account snapshot directories under <account_path>/snapshot/<slot> contain account files hardlinked
/// from <account_path>/run taken at snapshot <slot> time.  They are referenced by the symlinks from the
/// bank snapshot dir snapshot/<slot>/accounts_hardlinks/.  We observed that sometimes the bank snapshot dir
/// could be deleted but the account snapshot directories were left behind, possibly by some manual operations
/// or some legacy code not using the symlinks to clean up the account snapshot hardlink directories.
/// This function cleans up any account snapshot directories that are no longer referenced by the bank
/// snapshot dirs, to ensure proper snapshot operations.
pub fn clean_orphaned_account_snapshot_dirs(
    bank_snapshots_dir: impl AsRef<Path>,
    account_snapshot_paths: &[PathBuf],
) -> io::Result<()> {
    // Create the HashSet of the account snapshot hardlink directories referenced by the snapshot dirs.
    // This is used to clean up any hardlinks that are no longer referenced by the snapshot dirs.
    let mut account_snapshot_dirs_referenced = HashSet::new();
    let snapshots = get_bank_snapshots(bank_snapshots_dir);
    for snapshot in snapshots {
        let account_hardlinks_dir = snapshot
            .snapshot_dir
            .join(snapshot_paths::SNAPSHOT_ACCOUNTS_HARDLINKS);
        // loop through entries in the snapshot_hardlink_dir, read the symlinks, add the target to the HashSet
        let Ok(read_dir) = fs::read_dir(&account_hardlinks_dir) else {
            // The bank snapshot may not have a hard links dir with the storages.
            // This is fine, and happens for bank snapshots we do *not* fastboot from.
            // In this case, log it and go to the next bank snapshot.
            debug!(
                "failed to read account hardlinks dir '{}'",
                account_hardlinks_dir.display(),
            );
            continue;
        };
        for entry in read_dir {
            let path = entry?.path();
            let target = fs::read_link(&path).map_err(|err| {
                IoError::other(format!(
                    "failed to read symlink '{}': {err}",
                    path.display(),
                ))
            })?;
            account_snapshot_dirs_referenced.insert(target);
        }
    }

    // loop through the account snapshot hardlink directories, if the directory is not in the account_snapshot_dirs_referenced set, delete it
    for account_snapshot_path in account_snapshot_paths {
        let read_dir = fs::read_dir(account_snapshot_path).map_err(|err| {
            IoError::other(format!(
                "failed to read account snapshot dir '{}': {err}",
                account_snapshot_path.display(),
            ))
        })?;
        for entry in read_dir {
            let path = entry?.path();
            if !account_snapshot_dirs_referenced.contains(&path) {
                info!(
                    "Removing orphaned account snapshot hardlink directory '{}'...",
                    path.display()
                );
                move_and_async_delete_path(&path);
            }
        }
    }

    Ok(())
}

/// Purges incomplete bank snapshots
pub fn purge_incomplete_bank_snapshots(bank_snapshots_dir: impl AsRef<Path>) {
    let Ok(read_dir_iter) = std::fs::read_dir(&bank_snapshots_dir) else {
        // If we cannot read the bank snapshots dir, then there's nothing to do
        return;
    };

    let is_incomplete = |dir: &PathBuf| !is_bank_snapshot_complete(dir);

    let incomplete_dirs: Vec<_> = read_dir_iter
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(is_incomplete)
        .collect();

    // attempt to purge all the incomplete directories; do not exit early
    for incomplete_dir in incomplete_dirs {
        let result = purge_bank_snapshot(&incomplete_dir);
        match result {
            Ok(_) => info!(
                "Purged incomplete snapshot dir: {}",
                incomplete_dir.display()
            ),
            Err(err) => warn!("Failed to purge incomplete snapshot dir: {err}"),
        }
    }
}

/// Is the bank snapshot complete?
fn is_bank_snapshot_complete(bank_snapshot_dir: impl AsRef<Path>) -> bool {
    let version_path = bank_snapshot_dir
        .as_ref()
        .join(snapshot_paths::SNAPSHOT_VERSION_FILENAME);
    version_path.is_file()
}

/// Writes files that indicate the bank snapshot is loadable by fastboot
pub fn mark_bank_snapshot_as_loadable(bank_snapshot_dir: impl AsRef<Path>) -> io::Result<()> {
    let snapshot_fastboot_version_path = bank_snapshot_dir
        .as_ref()
        .join(snapshot_paths::SNAPSHOT_FASTBOOT_VERSION_FILENAME);
    fs::write(
        &snapshot_fastboot_version_path,
        SNAPSHOT_FASTBOOT_VERSION.to_string(),
    )
    .map_err(|err| {
        IoError::other(format!(
            "failed to write fastboot version file '{}': {err}",
            snapshot_fastboot_version_path.display(),
        ))
    })?;
    Ok(())
}

/// Is this bank snapshot loadable?
fn is_bank_snapshot_loadable(
    fastboot_version: Option<&Version>,
) -> std::result::Result<bool, SnapshotFastbootError> {
    if let Some(fastboot_version) = fastboot_version {
        is_snapshot_fastboot_compatible(fastboot_version)
    } else {
        // No fastboot version file, so this is not a fastbootable
        Ok(false)
    }
}

/// Is the fastboot snapshot version compatible?
fn is_snapshot_fastboot_compatible(
    version: &Version,
) -> std::result::Result<bool, SnapshotFastbootError> {
    if version.major <= SNAPSHOT_FASTBOOT_VERSION.major {
        Ok(true)
    } else {
        Err(SnapshotFastbootError::IncompatibleVersion(version.clone()))
    }
}

/// Gets the highest, loadable, bank snapshot
///
/// The highest bank snapshot is the one with the highest slot.
pub fn get_highest_loadable_bank_snapshot(
    snapshot_config: &SnapshotConfig,
) -> Option<BankSnapshotInfo> {
    let highest_bank_snapshot = get_highest_bank_snapshot(&snapshot_config.bank_snapshots_dir)?;

    let is_bank_snapshot_loadable =
        is_bank_snapshot_loadable(highest_bank_snapshot.fastboot_version.as_ref());

    match is_bank_snapshot_loadable {
        Ok(true) => Some(highest_bank_snapshot),
        Ok(false) => None,
        Err(err) => {
            warn!(
                "Bank snapshot is not loadable '{}': {err}",
                highest_bank_snapshot.snapshot_dir.display()
            );
            None
        }
    }
}

/// If the validator halts in the middle of `archive_snapshot_package()`, the temporary staging
/// directory won't be cleaned up.  Call this function to clean them up.
pub fn remove_tmp_snapshot_archives(snapshot_archives_dir: impl AsRef<Path>) {
    if let Ok(entries) = std::fs::read_dir(snapshot_archives_dir) {
        for entry in entries.flatten() {
            if entry
                .file_name()
                .to_str()
                .map(|file_name| file_name.starts_with(snapshot_paths::TMP_SNAPSHOT_ARCHIVE_PREFIX))
                .unwrap_or(false)
            {
                let path = entry.path();
                let result = if path.is_dir() {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                };
                if let Err(err) = result {
                    warn!(
                        "Failed to remove temporary snapshot archive '{}': {err}",
                        path.display(),
                    );
                }
            }
        }
    }
}

/// Creates an archive based on the bank snapshot and snapshot storages
pub fn archive_snapshot_package(
    snapshot_archive_kind: SnapshotArchiveKind,
    snapshot_slot: Slot,
    snapshot_hash: SnapshotHash,
    bank_snapshot_dir: impl AsRef<Path>,
    mut snapshot_storages: Vec<Arc<AccountStorageEntry>>,
    snapshot_config: &SnapshotConfig,
) -> Result<SnapshotArchiveInfo> {
    let snapshot_archive_path = match snapshot_archive_kind {
        SnapshotArchiveKind::Full => snapshot_paths::build_full_snapshot_archive_path(
            &snapshot_config.full_snapshot_archives_dir,
            snapshot_slot,
            &snapshot_hash,
            snapshot_config.archive_format,
        ),
        SnapshotArchiveKind::Incremental(incremental_snapshot_base_slot) => {
            // After the snapshot has been serialized, it is now safe (and required) to prune all
            // the storages that are *not* to be archived for this incremental snapshot.
            snapshot_storages.retain(|storage| storage.slot() > incremental_snapshot_base_slot);
            snapshot_paths::build_incremental_snapshot_archive_path(
                &snapshot_config.incremental_snapshot_archives_dir,
                incremental_snapshot_base_slot,
                snapshot_slot,
                &snapshot_hash,
                snapshot_config.archive_format,
            )
        }
    };

    let snapshot_archive_info = archive_snapshot(
        snapshot_archive_kind,
        snapshot_slot,
        snapshot_hash,
        snapshot_storages.as_slice(),
        &bank_snapshot_dir,
        snapshot_archive_path,
        snapshot_config.archive_format,
    )?;

    Ok(snapshot_archive_info)
}

/// Serializes a snapshot into `bank_snapshots_dir`
pub fn serialize_snapshot(
    bank_snapshots_dir: impl AsRef<Path>,
    snapshot_version: SnapshotVersion,
    bank_snapshot_package: BankSnapshotPackage,
    snapshot_storages: &[Arc<AccountStorageEntry>],
    should_flush_and_hard_link_storages: bool,
) -> Result<BankSnapshotInfo> {
    let BankSnapshotPackage {
        mut bank_fields,
        bank_hash_stats,
        status_cache_slot_deltas,
        write_version,
    } = bank_snapshot_package;
    let status_cache_slot_deltas = status_cache_slot_deltas.as_slice();
    let slot = bank_fields.slot;

    // this lambda function is to facilitate converting between
    // the AddBankSnapshotError and SnapshotError types
    let do_serialize_snapshot = || {
        let mut measure_everything = Measure::start("");
        let bank_snapshot_dir = snapshot_paths::get_bank_snapshot_dir(&bank_snapshots_dir, slot);
        if bank_snapshot_dir.exists() {
            return Err(AddBankSnapshotError::SnapshotDirAlreadyExists(
                bank_snapshot_dir,
            ));
        }
        fs::create_dir_all(&bank_snapshot_dir).map_err(|err| {
            AddBankSnapshotError::CreateSnapshotDir(err, bank_snapshot_dir.clone())
        })?;

        // the bank snapshot is stored as bank_snapshots_dir/slot/slot
        let bank_snapshot_path =
            bank_snapshot_dir.join(snapshot_paths::get_snapshot_file_name(slot));
        info!(
            "Creating bank snapshot for slot {slot} at '{}'",
            bank_snapshot_path.display(),
        );

        let bank_snapshot_serializer = move |stream: &mut dyn Write| -> Result<()> {
            let versioned_epoch_stakes = mem::take(&mut bank_fields.versioned_epoch_stakes);
            let extra_fields = ExtraFieldsToSerialize {
                lamports_per_signature: bank_fields.fee_rate_governor.lamports_per_signature,
                obsolete_incremental_snapshot_persistence: None,
                obsolete_epoch_accounts_hash: None,
                versioned_epoch_stakes,
                accounts_lt_hash: Some(bank_fields.accounts_lt_hash.clone().into()),
            };
            serde_snapshot::serialize_bank_snapshot_into(
                stream,
                bank_fields,
                bank_hash_stats,
                &get_storages_to_serialize(snapshot_storages),
                extra_fields,
                write_version,
            )?;
            Ok(())
        };
        let (bank_snapshot_consumed_size, bank_serialize) = measure_time!(
            serialize_snapshot_data_file(&bank_snapshot_path, bank_snapshot_serializer)
                .map_err(|err| AddBankSnapshotError::SerializeBank(Box::new(err)))?,
            "bank serialize"
        );

        let status_cache_path =
            bank_snapshot_dir.join(snapshot_paths::SNAPSHOT_STATUS_CACHE_FILENAME);
        let (status_cache_consumed_size, status_cache_serialize_us) = measure_us!(
            serde_snapshot::serialize_status_cache(status_cache_slot_deltas, &status_cache_path)
                .map_err(|err| AddBankSnapshotError::SerializeStatusCache(Box::new(err)))?
        );

        let version_path = bank_snapshot_dir.join(snapshot_paths::SNAPSHOT_VERSION_FILENAME);
        let (_, write_version_file_us) = measure_us!(fs::write(
            &version_path,
            snapshot_version.as_str().as_bytes(),
        )
        .map_err(|err| AddBankSnapshotError::WriteSnapshotVersionFile(err, version_path))?);

        let (flush_storages_us, hard_link_storages_us, serialize_obsolete_accounts_us) =
            if should_flush_and_hard_link_storages {
                let flush_measure = Measure::start("");
                for storage in snapshot_storages {
                    storage.flush().map_err(|err| {
                        AddBankSnapshotError::FlushStorage(err, storage.path().to_path_buf())
                    })?;
                }
                let flush_us = flush_measure.end_as_us();
                let (_, hard_link_us) = measure_us!(hard_link_storages_to_snapshot(
                    &bank_snapshot_dir,
                    slot,
                    snapshot_storages
                )
                .map_err(AddBankSnapshotError::HardLinkStorages)?);

                let (_, serialize_obsolete_accounts_us) = measure_us!({
                    write_obsolete_accounts_to_snapshot(&bank_snapshot_dir, snapshot_storages, slot)
                        .map_err(|err| {
                            AddBankSnapshotError::SerializeObsoleteAccounts(Box::new(err))
                        })?
                });

                mark_bank_snapshot_as_loadable(&bank_snapshot_dir)
                    .map_err(AddBankSnapshotError::MarkSnapshotLoadable)?;

                (
                    Some(flush_us),
                    Some(hard_link_us),
                    Some(serialize_obsolete_accounts_us),
                )
            } else {
                (None, None, None)
            };

        measure_everything.stop();

        // Monitor sizes because they're capped to MAX_SNAPSHOT_DATA_FILE_SIZE
        datapoint_info!(
            "snapshot_bank",
            ("slot", slot, i64),
            ("bank_size", bank_snapshot_consumed_size, i64),
            ("status_cache_size", status_cache_consumed_size, i64),
            ("flush_storages_us", flush_storages_us, Option<i64>),
            ("hard_link_storages_us", hard_link_storages_us, Option<i64>),
            ("serialize_obsolete_accounts_us", serialize_obsolete_accounts_us, Option<i64>),
            ("bank_serialize_us", bank_serialize.as_us(), i64),
            ("status_cache_serialize_us", status_cache_serialize_us, i64),
            ("write_version_file_us", write_version_file_us, i64),
            ("total_us", measure_everything.as_us(), i64),
        );

        info!(
            "{} for slot {} at {}",
            bank_serialize,
            slot,
            bank_snapshot_path.display(),
        );

        Ok(BankSnapshotInfo {
            slot,
            snapshot_dir: bank_snapshot_dir,
            snapshot_version,
            fastboot_version: None,
        })
    };

    do_serialize_snapshot().map_err(|err| SnapshotError::AddBankSnapshot(err, slot))
}

/// Get the bank snapshots in a directory
pub fn get_bank_snapshots(bank_snapshots_dir: impl AsRef<Path>) -> Vec<BankSnapshotInfo> {
    let mut bank_snapshots = Vec::default();
    match fs::read_dir(&bank_snapshots_dir) {
        Err(err) => {
            info!(
                "Unable to read bank snapshots directory '{}': {err}",
                bank_snapshots_dir.as_ref().display(),
            );
        }
        Ok(paths) => paths
            .filter_map(|entry| {
                // check if this entry is a directory and only a Slot
                // bank snapshots are bank_snapshots_dir/slot/slot
                entry
                    .ok()
                    .filter(|entry| entry.path().is_dir())
                    .and_then(|entry| {
                        entry
                            .path()
                            .file_name()
                            .and_then(|file_name| file_name.to_str())
                            .and_then(|file_name| file_name.parse::<Slot>().ok())
                    })
            })
            .for_each(
                |slot| match BankSnapshotInfo::new_from_dir(&bank_snapshots_dir, slot) {
                    Ok(snapshot_info) => bank_snapshots.push(snapshot_info),
                    // Other threads may be modifying bank snapshots in parallel; only return
                    // snapshots that are complete as deemed by BankSnapshotInfo::new_from_dir()
                    Err(err) => debug!("Unable to read bank snapshot for slot {slot}: {err}"),
                },
            ),
    }
    bank_snapshots
}

/// Get the bank snapshot with the highest slot in a directory
///
/// This function gets the highest bank snapshot of any kind
pub fn get_highest_bank_snapshot(bank_snapshots_dir: impl AsRef<Path>) -> Option<BankSnapshotInfo> {
    do_get_highest_bank_snapshot(get_bank_snapshots(&bank_snapshots_dir))
}

fn do_get_highest_bank_snapshot(
    mut bank_snapshots: Vec<BankSnapshotInfo>,
) -> Option<BankSnapshotInfo> {
    bank_snapshots.sort_unstable();
    bank_snapshots.into_iter().next_back()
}

pub fn write_obsolete_accounts_to_snapshot(
    bank_snapshot_dir: impl AsRef<Path>,
    snapshot_storages: &[Arc<AccountStorageEntry>],
    snapshot_slot: Slot,
) -> Result<u64> {
    let obsolete_accounts =
        SerdeObsoleteAccountsMap::new_from_storages(snapshot_storages, snapshot_slot);
    serialize_obsolete_accounts(
        bank_snapshot_dir,
        &obsolete_accounts,
        MAX_OBSOLETE_ACCOUNTS_FILE_SIZE,
    )
}

fn serialize_obsolete_accounts(
    bank_snapshot_dir: impl AsRef<Path>,
    obsolete_accounts_map: &SerdeObsoleteAccountsMap,
    maximum_obsolete_accounts_file_size: u64,
) -> Result<u64> {
    let obsolete_accounts_path = bank_snapshot_dir
        .as_ref()
        .join(snapshot_paths::SNAPSHOT_OBSOLETE_ACCOUNTS_FILENAME);
    let mut file_stream = large_file_buf_writer(&obsolete_accounts_path)?;

    serde_snapshot::serialize_into(&mut file_stream, obsolete_accounts_map)?;

    file_stream.flush()?;

    let consumed_size = file_stream.stream_position()?;
    if consumed_size > maximum_obsolete_accounts_file_size {
        let error_message = format!(
            "too large obsolete accounts file to serialize: '{}' has {consumed_size} bytes, max \
             size is {maximum_obsolete_accounts_file_size}",
            obsolete_accounts_path.display(),
        );
        return Err(IoError::other(error_message).into());
    }
    Ok(consumed_size)
}

fn deserialize_obsolete_accounts(
    bank_snapshot_dir: impl AsRef<Path>,
    maximum_obsolete_accounts_file_size: u64,
) -> Result<SerdeObsoleteAccountsMap> {
    let obsolete_accounts_path = bank_snapshot_dir
        .as_ref()
        .join(snapshot_paths::SNAPSHOT_OBSOLETE_ACCOUNTS_FILENAME);
    let obsolete_accounts_file = fs::File::open(&obsolete_accounts_path)?;
    // If the file is too large return error
    let obsolete_accounts_file_metadata = fs::metadata(&obsolete_accounts_path)?;
    if obsolete_accounts_file_metadata.len() > maximum_obsolete_accounts_file_size {
        let error_message = format!(
            "too large obsolete accounts file to deserialize: '{}' has {} bytes (max size is \
             {maximum_obsolete_accounts_file_size} bytes)",
            obsolete_accounts_path.display(),
            obsolete_accounts_file_metadata.len(),
        );
        return Err(IoError::other(error_message).into());
    }

    let mut data_file_stream = BufReader::new(obsolete_accounts_file);

    let obsolete_accounts = serde_snapshot::deserialize_from(&mut data_file_stream)?;

    Ok(obsolete_accounts)
}

pub fn serialize_snapshot_data_file<F>(data_file_path: &Path, serializer: F) -> Result<u64>
where
    F: FnOnce(&mut dyn Write) -> Result<()>,
{
    serialize_snapshot_data_file_capped::<F>(
        data_file_path,
        MAX_SNAPSHOT_DATA_FILE_SIZE,
        serializer,
    )
}

pub fn deserialize_snapshot_data_file<T: Sized>(
    data_file_path: &Path,
    deserializer: impl FnOnce(&mut BufReader<std::fs::File>) -> Result<T>,
) -> Result<T> {
    let wrapped_deserializer = move |streams: &mut SnapshotStreams<std::fs::File>| -> Result<T> {
        deserializer(streams.full_snapshot_stream)
    };

    let wrapped_data_file_path = SnapshotRootPaths {
        full_snapshot_root_file_path: data_file_path.to_path_buf(),
        incremental_snapshot_root_file_path: None,
    };

    deserialize_snapshot_data_files_capped(
        &wrapped_data_file_path,
        MAX_SNAPSHOT_DATA_FILE_SIZE,
        wrapped_deserializer,
    )
}

pub fn deserialize_snapshot_data_files<T: Sized>(
    snapshot_root_paths: &SnapshotRootPaths,
    deserializer: impl FnOnce(&mut SnapshotStreams<std::fs::File>) -> Result<T>,
) -> Result<T> {
    deserialize_snapshot_data_files_capped(
        snapshot_root_paths,
        MAX_SNAPSHOT_DATA_FILE_SIZE,
        deserializer,
    )
}

fn serialize_snapshot_data_file_capped<F>(
    data_file_path: &Path,
    maximum_file_size: u64,
    serializer: F,
) -> Result<u64>
where
    F: FnOnce(&mut dyn Write) -> Result<()>,
{
    let mut data_file_stream = large_file_buf_writer(data_file_path)?;
    serializer(&mut data_file_stream)?;
    data_file_stream.flush()?;

    let consumed_size = data_file_stream.stream_position()?;
    if consumed_size > maximum_file_size {
        let error_message = format!(
            "too large snapshot data file to serialize: '{}' has {consumed_size} bytes",
            data_file_path.display(),
        );
        return Err(IoError::other(error_message).into());
    }
    Ok(consumed_size)
}

fn deserialize_snapshot_data_files_capped<T: Sized>(
    snapshot_root_paths: &SnapshotRootPaths,
    maximum_file_size: u64,
    deserializer: impl FnOnce(&mut SnapshotStreams<std::fs::File>) -> Result<T>,
) -> Result<T> {
    let (full_snapshot_file_size, mut full_snapshot_data_file_stream) =
        create_snapshot_data_file_stream(
            &snapshot_root_paths.full_snapshot_root_file_path,
            maximum_file_size,
        )?;

    let (incremental_snapshot_file_size, mut incremental_snapshot_data_file_stream) =
        if let Some(ref incremental_snapshot_root_file_path) =
            snapshot_root_paths.incremental_snapshot_root_file_path
        {
            Some(create_snapshot_data_file_stream(
                incremental_snapshot_root_file_path,
                maximum_file_size,
            )?)
        } else {
            None
        }
        .unzip();

    let mut snapshot_streams = SnapshotStreams {
        full_snapshot_stream: &mut full_snapshot_data_file_stream,
        incremental_snapshot_stream: incremental_snapshot_data_file_stream.as_mut(),
    };
    let ret = deserializer(&mut snapshot_streams)?;

    check_deserialize_file_consumed(
        full_snapshot_file_size,
        &snapshot_root_paths.full_snapshot_root_file_path,
        &mut full_snapshot_data_file_stream,
    )?;

    if let Some(ref incremental_snapshot_root_file_path) =
        snapshot_root_paths.incremental_snapshot_root_file_path
    {
        check_deserialize_file_consumed(
            incremental_snapshot_file_size.unwrap(),
            incremental_snapshot_root_file_path,
            incremental_snapshot_data_file_stream.as_mut().unwrap(),
        )?;
    }

    Ok(ret)
}

/// Before running the deserializer function, perform common operations on the snapshot archive
/// files, such as checking the file size and opening the file into a stream.
fn create_snapshot_data_file_stream(
    snapshot_root_file_path: impl AsRef<Path>,
    maximum_file_size: u64,
) -> Result<(u64, BufReader<std::fs::File>)> {
    let snapshot_file_size = fs::metadata(&snapshot_root_file_path)?.len();

    if snapshot_file_size > maximum_file_size {
        let error_message = format!(
            "too large snapshot data file to deserialize: '{}' has {} bytes (max size is {} bytes)",
            snapshot_root_file_path.as_ref().display(),
            snapshot_file_size,
            maximum_file_size,
        );
        return Err(IoError::other(error_message).into());
    }

    let snapshot_data_file = fs::File::open(snapshot_root_file_path)?;
    let snapshot_data_file_stream = BufReader::new(snapshot_data_file);

    Ok((snapshot_file_size, snapshot_data_file_stream))
}

/// After running the deserializer function, perform common checks to ensure the snapshot archive
/// files were consumed correctly.
fn check_deserialize_file_consumed(
    file_size: u64,
    file_path: impl AsRef<Path>,
    file_stream: &mut BufReader<std::fs::File>,
) -> Result<()> {
    let consumed_size = file_stream.stream_position()?;

    if consumed_size != file_size {
        let error_message = format!(
            "invalid snapshot data file: '{}' has {} bytes, however consumed {} bytes to \
             deserialize",
            file_path.as_ref().display(),
            file_size,
            consumed_size,
        );
        return Err(IoError::other(error_message).into());
    }

    Ok(())
}

/// Return account path from the appendvec path after checking its format.
fn get_account_path_from_appendvec_path(appendvec_path: &Path) -> Option<PathBuf> {
    let run_path = appendvec_path.parent()?;
    let run_file_name = run_path.file_name()?;
    // All appendvec files should be under <account_path>/run/.
    // When generating the bank snapshot directory, they are hardlinked to <account_path>/snapshot/<slot>/
    if run_file_name != ACCOUNTS_RUN_DIR {
        error!(
            "The account path {} does not have run/ as its immediate parent directory.",
            run_path.display()
        );
        return None;
    }
    let account_path = run_path.parent()?;
    Some(account_path.to_path_buf())
}

/// From an appendvec path, derive the snapshot hardlink path.  If the corresponding snapshot hardlink
/// directory does not exist, create it.
fn get_snapshot_accounts_hardlink_dir(
    appendvec_path: &Path,
    bank_slot: Slot,
    account_paths: &mut HashSet<PathBuf>,
    hardlinks_dir: impl AsRef<Path>,
) -> std::result::Result<PathBuf, GetSnapshotAccountsHardLinkDirError> {
    let account_path = get_account_path_from_appendvec_path(appendvec_path).ok_or_else(|| {
        GetSnapshotAccountsHardLinkDirError::GetAccountPath(appendvec_path.to_path_buf())
    })?;

    let snapshot_hardlink_dir = account_path
        .join(ACCOUNTS_SNAPSHOT_DIR)
        .join(bank_slot.to_string());

    // Use the hashset to track, to avoid checking the file system.  Only set up the hardlink directory
    // and the symlink to it at the first time of seeing the account_path.
    if !account_paths.contains(&account_path) {
        let idx = account_paths.len();
        debug!(
            "for appendvec_path {}, create hard-link path {}",
            appendvec_path.display(),
            snapshot_hardlink_dir.display()
        );
        fs::create_dir_all(&snapshot_hardlink_dir).map_err(|err| {
            GetSnapshotAccountsHardLinkDirError::CreateSnapshotHardLinkDir(
                err,
                snapshot_hardlink_dir.clone(),
            )
        })?;
        let symlink_path = hardlinks_dir.as_ref().join(format!("account_path_{idx}"));
        symlink::symlink_dir(&snapshot_hardlink_dir, &symlink_path).map_err(|err| {
            GetSnapshotAccountsHardLinkDirError::SymlinkSnapshotHardLinkDir {
                source: err,
                original: snapshot_hardlink_dir.clone(),
                link: symlink_path,
            }
        })?;
        account_paths.insert(account_path);
    };

    Ok(snapshot_hardlink_dir)
}

/// Hard-link the files from accounts/ to snapshot/<bank_slot>/accounts/
/// This keeps the appendvec files alive and with the bank snapshot.  The slot and id
/// in the file names are also updated in case its file is a recycled one with inconsistent slot
/// and id.
pub fn hard_link_storages_to_snapshot(
    bank_snapshot_dir: impl AsRef<Path>,
    bank_slot: Slot,
    snapshot_storages: &[Arc<AccountStorageEntry>],
) -> std::result::Result<(), HardLinkStoragesToSnapshotError> {
    let accounts_hardlinks_dir = bank_snapshot_dir
        .as_ref()
        .join(snapshot_paths::SNAPSHOT_ACCOUNTS_HARDLINKS);
    fs::create_dir_all(&accounts_hardlinks_dir).map_err(|err| {
        HardLinkStoragesToSnapshotError::CreateAccountsHardLinksDir(
            err,
            accounts_hardlinks_dir.clone(),
        )
    })?;

    let mut account_paths: HashSet<PathBuf> = HashSet::new();
    for storage in snapshot_storages {
        let storage_path = storage.accounts.path();
        let snapshot_hardlink_dir = get_snapshot_accounts_hardlink_dir(
            storage_path,
            bank_slot,
            &mut account_paths,
            &accounts_hardlinks_dir,
        )?;
        // The appendvec could be recycled, so its filename may not be consistent to the slot and id.
        // Use the storage slot and id to compose a consistent file name for the hard-link file.
        let hardlink_filename = AccountsFile::file_name(storage.slot(), storage.id());
        let hard_link_path = snapshot_hardlink_dir.join(hardlink_filename);
        fs::hard_link(storage_path, &hard_link_path).map_err(|err| {
            HardLinkStoragesToSnapshotError::HardLinkStorage(
                err,
                storage_path.to_path_buf(),
                hard_link_path,
            )
        })?;
    }
    Ok(())
}

/// serializing needs Vec<Vec<Arc<AccountStorageEntry>>>, but data structure at runtime is Vec<Arc<AccountStorageEntry>>
/// translates to what we need
pub(crate) fn get_storages_to_serialize(
    snapshot_storages: &[Arc<AccountStorageEntry>],
) -> Vec<Vec<Arc<AccountStorageEntry>>> {
    snapshot_storages
        .iter()
        .map(|storage| vec![Arc::clone(storage)])
        .collect::<Vec<_>>()
}

/// Unarchives the given full and incremental snapshot archives, as long as they are compatible.
pub fn verify_and_unarchive_snapshots(
    bank_snapshots_dir: impl AsRef<Path>,
    full_snapshot_archive_info: &FullSnapshotArchiveInfo,
    incremental_snapshot_archive_info: Option<&IncrementalSnapshotArchiveInfo>,
    account_paths: &[PathBuf],
    accounts_db_config: &AccountsDbConfig,
) -> Result<(UnarchivedSnapshots, UnarchivedSnapshotsGuard)> {
    check_are_snapshots_compatible(
        full_snapshot_archive_info,
        incremental_snapshot_archive_info,
    )?;

    let next_append_vec_id = Arc::new(AtomicAccountsFileId::new(0));
    let UnarchivedSnapshot {
        unpack_dir: full_unpack_dir,
        storage: full_storage,
        bank_fields: full_bank_fields,
        accounts_db_fields: full_accounts_db_fields,
        unpacked_snapshots_dir_and_version: full_unpacked_snapshots_dir_and_version,
        measure_untar: full_measure_untar,
    } = unarchive_snapshot(
        &bank_snapshots_dir,
        snapshot_paths::TMP_SNAPSHOT_ARCHIVE_PREFIX,
        full_snapshot_archive_info.path(),
        "snapshot untar",
        account_paths,
        full_snapshot_archive_info.archive_format(),
        next_append_vec_id.clone(),
        accounts_db_config,
    )?;

    let (
        incremental_unpack_dir,
        incremental_storage,
        incremental_bank_fields,
        incremental_accounts_db_fields,
        incremental_unpacked_snapshots_dir_and_version,
        incremental_measure_untar,
    ) = if let Some(incremental_snapshot_archive_info) = incremental_snapshot_archive_info {
        let UnarchivedSnapshot {
            unpack_dir,
            storage,
            bank_fields,
            accounts_db_fields,
            unpacked_snapshots_dir_and_version,
            measure_untar,
        } = unarchive_snapshot(
            &bank_snapshots_dir,
            snapshot_paths::TMP_SNAPSHOT_ARCHIVE_PREFIX,
            incremental_snapshot_archive_info.path(),
            "incremental snapshot untar",
            account_paths,
            incremental_snapshot_archive_info.archive_format(),
            next_append_vec_id.clone(),
            accounts_db_config,
        )?;
        (
            Some(unpack_dir),
            Some(storage),
            Some(bank_fields),
            Some(accounts_db_fields),
            Some(unpacked_snapshots_dir_and_version),
            Some(measure_untar),
        )
    } else {
        (None, None, None, None, None, None)
    };

    let bank_fields = SnapshotBankFields::new(full_bank_fields, incremental_bank_fields);
    let accounts_db_fields =
        SnapshotAccountsDbFields::new(full_accounts_db_fields, incremental_accounts_db_fields);
    let next_append_vec_id = Arc::try_unwrap(next_append_vec_id).unwrap();

    Ok((
        UnarchivedSnapshots {
            full_storage,
            incremental_storage,
            bank_fields,
            accounts_db_fields,
            full_unpacked_snapshots_dir_and_version,
            incremental_unpacked_snapshots_dir_and_version,
            full_measure_untar,
            incremental_measure_untar,
            next_append_vec_id,
        },
        UnarchivedSnapshotsGuard {
            full_unpack_dir,
            incremental_unpack_dir,
        },
    ))
}

/// Used to determine if a filename is structured like a version file, bank file, or storage file
#[derive(PartialEq, Debug)]
enum SnapshotFileKind {
    Version,
    BankFields,
    Storage,
}

/// Determines `SnapshotFileKind` for `filename` if any
fn get_snapshot_file_kind(filename: &str) -> Option<SnapshotFileKind> {
    static VERSION_FILE_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^version$").unwrap());
    static BANK_FIELDS_FILE_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[0-9]+(\.pre)?$").unwrap());

    if VERSION_FILE_REGEX.is_match(filename) {
        Some(SnapshotFileKind::Version)
    } else if BANK_FIELDS_FILE_REGEX.is_match(filename) {
        Some(SnapshotFileKind::BankFields)
    } else if get_slot_and_append_vec_id(filename).is_ok() {
        Some(SnapshotFileKind::Storage)
    } else {
        None
    }
}

/// Waits for snapshot file
/// Due to parallel unpacking, we may receive some append_vec files before the snapshot file
/// This function will push append_vec files into a buffer until we receive the snapshot file
fn get_version_and_snapshot_files(
    file_receiver: &Receiver<PathBuf>,
) -> Result<(PathBuf, PathBuf, Vec<PathBuf>)> {
    let mut append_vec_files = Vec::with_capacity(1024);
    let mut snapshot_version_path = None;
    let mut snapshot_file_path = None;

    loop {
        if let Ok(path) = file_receiver.recv() {
            let filename = path.file_name().unwrap().to_str().unwrap();
            match get_snapshot_file_kind(filename) {
                Some(SnapshotFileKind::Version) => {
                    snapshot_version_path = Some(path);

                    // break if we have both the snapshot file and the version file
                    if snapshot_file_path.is_some() {
                        break;
                    }
                }
                Some(SnapshotFileKind::BankFields) => {
                    snapshot_file_path = Some(path);

                    // break if we have both the snapshot file and the version file
                    if snapshot_version_path.is_some() {
                        break;
                    }
                }
                Some(SnapshotFileKind::Storage) => {
                    append_vec_files.push(path);
                }
                None => {} // do nothing for other kinds of files
            }
        } else {
            return Err(SnapshotError::RebuildStorages(
                "did not receive snapshot file from unpacking threads".to_string(),
            ));
        }
    }
    let snapshot_version_path = snapshot_version_path.unwrap();
    let snapshot_file_path = snapshot_file_path.unwrap();

    Ok((snapshot_version_path, snapshot_file_path, append_vec_files))
}

/// Fields and information parsed from the snapshot.
struct SnapshotFieldsBundle {
    snapshot_version: SnapshotVersion,
    bank_fields: BankFieldsToDeserialize,
    accounts_db_fields: AccountsDbFields<SerializableAccountStorageEntry>,
    append_vec_files: Vec<PathBuf>,
}

/// Parses fields and information from the snapshot files provided by
/// `file_receiver`.
fn snapshot_fields_from_files(file_receiver: &Receiver<PathBuf>) -> Result<SnapshotFieldsBundle> {
    let (snapshot_version_path, snapshot_file_path, append_vec_files) =
        get_version_and_snapshot_files(file_receiver)?;
    let snapshot_version_str = snapshot_version_from_file(snapshot_version_path)?;
    let snapshot_version = snapshot_version_str.parse().map_err(|err| {
        IoError::other(format!(
            "unsupported snapshot version '{snapshot_version_str}': {err}",
        ))
    })?;

    let snapshot_file = fs::File::open(snapshot_file_path).unwrap();
    let mut snapshot_stream = BufReader::new(snapshot_file);
    let (bank_fields, accounts_db_fields) = match snapshot_version {
        SnapshotVersion::V1_2_0 => serde_snapshot::fields_from_stream(&mut snapshot_stream)?,
    };

    Ok(SnapshotFieldsBundle {
        snapshot_version,
        bank_fields,
        accounts_db_fields,
        append_vec_files,
    })
}

/// BankSnapshotInfo::new_from_dir() requires a few meta files to accept a snapshot dir
/// as a valid one.  A dir unpacked from an archive lacks these files.  Fill them here to
/// allow new_from_dir() checks to pass.  These checks are not needed for unpacked dirs,
/// but it is not clean to add another flag to new_from_dir() to skip them.
fn create_snapshot_meta_files_for_unarchived_snapshot(unpack_dir: impl AsRef<Path>) -> Result<()> {
    let snapshots_dir = unpack_dir.as_ref().join(snapshot_paths::BANK_SNAPSHOTS_DIR);
    if !snapshots_dir.is_dir() {
        return Err(SnapshotError::NoSnapshotSlotDir(snapshots_dir));
    }

    // The unpacked dir has a single slot dir, which is the snapshot slot dir.
    let slot_dir = std::fs::read_dir(&snapshots_dir)
        .map_err(|_| SnapshotError::NoSnapshotSlotDir(snapshots_dir.clone()))?
        .find(|entry| entry.as_ref().unwrap().path().is_dir())
        .ok_or_else(|| SnapshotError::NoSnapshotSlotDir(snapshots_dir.clone()))?
        .map_err(|_| SnapshotError::NoSnapshotSlotDir(snapshots_dir.clone()))?
        .path();

    let version_file = unpack_dir
        .as_ref()
        .join(snapshot_paths::SNAPSHOT_VERSION_FILENAME);
    fs::hard_link(
        version_file,
        slot_dir.join(snapshot_paths::SNAPSHOT_VERSION_FILENAME),
    )?;

    let status_cache_file = snapshots_dir.join(snapshot_paths::SNAPSHOT_STATUS_CACHE_FILENAME);
    fs::hard_link(
        status_cache_file,
        slot_dir.join(snapshot_paths::SNAPSHOT_STATUS_CACHE_FILENAME),
    )?;

    Ok(())
}

/// Perform the common tasks when unarchiving a snapshot.  Handles creating the temporary
/// directories, untaring, reading the version file, and then returning those fields plus the
/// rebuilt storage
fn unarchive_snapshot(
    bank_snapshots_dir: impl AsRef<Path>,
    unpacked_snapshots_dir_prefix: &'static str,
    snapshot_archive_path: impl AsRef<Path>,
    measure_name: &'static str,
    account_paths: &[PathBuf],
    archive_format: ArchiveFormat,
    next_append_vec_id: Arc<AtomicAccountsFileId>,
    accounts_db_config: &AccountsDbConfig,
) -> Result<UnarchivedSnapshot> {
    let unpack_dir = tempfile::Builder::new()
        .prefix(unpacked_snapshots_dir_prefix)
        .tempdir_in(bank_snapshots_dir)?;
    let unpacked_snapshots_dir = unpack_dir.path().join(snapshot_paths::BANK_SNAPSHOTS_DIR);

    let (file_sender, file_receiver) = crossbeam_channel::unbounded();
    let unarchive_handle = streaming_unarchive_snapshot(
        file_sender,
        account_paths.to_vec(),
        unpack_dir.path().to_path_buf(),
        snapshot_archive_path.as_ref().to_path_buf(),
        archive_format,
        accounts_db_config.memlock_budget_size,
    );

    let num_rebuilder_threads = num_cpus::get_physical().saturating_sub(1).max(1);
    let snapshot_result = snapshot_fields_from_files(&file_receiver).and_then(
        |SnapshotFieldsBundle {
             snapshot_version,
             bank_fields,
             accounts_db_fields,
             append_vec_files,
             ..
         }| {
            let (storage, measure_untar) = measure_time!(
                SnapshotStorageRebuilder::rebuild_storage(
                    &accounts_db_fields,
                    append_vec_files,
                    file_receiver,
                    num_rebuilder_threads,
                    next_append_vec_id,
                    SnapshotFrom::Archive,
                    accounts_db_config.storage_access,
                    None,
                )?,
                measure_name
            );
            info!("{measure_untar}");
            create_snapshot_meta_files_for_unarchived_snapshot(&unpack_dir)?;

            Ok(UnarchivedSnapshot {
                unpack_dir,
                storage,
                bank_fields,
                accounts_db_fields,
                unpacked_snapshots_dir_and_version: UnpackedSnapshotsDirAndVersion {
                    unpacked_snapshots_dir,
                    snapshot_version,
                },
                measure_untar,
            })
        },
    );
    unarchive_handle.join().unwrap()?;
    snapshot_result
}

/// Streams snapshot dir files across channel
/// Follow the flow of streaming_unarchive_snapshot(), but handle the from_dir case.
fn streaming_snapshot_dir_files(
    file_sender: Sender<PathBuf>,
    snapshot_file_path: impl Into<PathBuf>,
    snapshot_version_path: impl Into<PathBuf>,
    account_paths: &[PathBuf],
) -> Result<()> {
    file_sender.send(snapshot_file_path.into())?;
    file_sender.send(snapshot_version_path.into())?;

    for account_path in account_paths {
        for file in fs::read_dir(account_path)? {
            file_sender.send(file?.path())?;
        }
    }

    Ok(())
}

/// Performs the common tasks when deserializing a snapshot
///
/// Handles reading the snapshot file and version file,
/// then returning those fields plus the rebuilt storages.
pub fn rebuild_storages_from_snapshot_dir(
    snapshot_info: &BankSnapshotInfo,
    account_paths: &[PathBuf],
    next_append_vec_id: Arc<AtomicAccountsFileId>,
    storage_access: StorageAccess,
) -> Result<(
    AccountStorageMap,
    BankFieldsToDeserialize,
    AccountsDbFields<SerializableAccountStorageEntry>,
)> {
    let bank_snapshot_dir = &snapshot_info.snapshot_dir;
    let accounts_hardlinks = bank_snapshot_dir.join(snapshot_paths::SNAPSHOT_ACCOUNTS_HARDLINKS);
    let account_run_paths: HashSet<_> = HashSet::from_iter(account_paths);

    // With fastboot_version >= 2, obsolete accounts are tracked and stored in the snapshot
    // Even if obsolete accounts are not enabled, the snapshot may still contain obsolete accounts
    // as the feature may have been enabled in previous validator runs.
    let obsolete_accounts = snapshot_info
        .fastboot_version
        .as_ref()
        .is_some_and(|fastboot_version| fastboot_version.major >= 2)
        .then(|| deserialize_obsolete_accounts(bank_snapshot_dir, MAX_OBSOLETE_ACCOUNTS_FILE_SIZE))
        .transpose()
        .map_err(|err| {
            IoError::other(format!(
                "failed to read obsolete accounts file '{}': {err}",
                bank_snapshot_dir.display()
            ))
        })?;

    let read_dir = fs::read_dir(&accounts_hardlinks).map_err(|err| {
        IoError::other(format!(
            "failed to read accounts hardlinks dir '{}': {err}",
            accounts_hardlinks.display(),
        ))
    })?;
    for dir_entry in read_dir {
        let symlink_path = dir_entry?.path();
        // The symlink point to <account_path>/snapshot/<slot> which contain the account files hardlinks
        // The corresponding run path should be <account_path>/run/
        let account_snapshot_path = fs::read_link(&symlink_path).map_err(|err| {
            IoError::other(format!(
                "failed to read symlink '{}': {err}",
                symlink_path.display(),
            ))
        })?;
        let account_run_path = account_snapshot_path
            .parent()
            .ok_or_else(|| SnapshotError::InvalidAccountPath(account_snapshot_path.clone()))?
            .parent()
            .ok_or_else(|| SnapshotError::InvalidAccountPath(account_snapshot_path.clone()))?
            .join(ACCOUNTS_RUN_DIR);
        if !account_run_paths.contains(&account_run_path) {
            // The appendvec from the bank snapshot storage does not match any of the provided account_paths set.
            // The account paths have changed so the snapshot is no longer usable.
            return Err(SnapshotError::AccountPathsMismatch);
        }
        // Generate hard-links to make the account files available in the main accounts/, and let the new appendvec
        // paths be in accounts/
        let read_dir = fs::read_dir(&account_snapshot_path).map_err(|err| {
            IoError::other(format!(
                "failed to read account snapshot dir '{}': {err}",
                account_snapshot_path.display(),
            ))
        })?;
        for file in read_dir {
            let file_path = file?.path();
            let file_name = file_path
                .file_name()
                .ok_or_else(|| SnapshotError::InvalidAppendVecPath(file_path.to_path_buf()))?;
            let dest_path = account_run_path.join(file_name);
            fs::hard_link(&file_path, &dest_path).map_err(|err| {
                IoError::other(format!(
                    "failed to hard link from '{}' to '{}': {err}",
                    file_path.display(),
                    dest_path.display(),
                ))
            })?;
        }
    }

    let (file_sender, file_receiver) = crossbeam_channel::unbounded();
    let snapshot_file_path = &snapshot_info.snapshot_path();
    let snapshot_version_path = bank_snapshot_dir.join(snapshot_paths::SNAPSHOT_VERSION_FILENAME);
    streaming_snapshot_dir_files(
        file_sender,
        snapshot_file_path,
        snapshot_version_path,
        account_paths,
    )?;

    let SnapshotFieldsBundle {
        bank_fields,
        accounts_db_fields,
        append_vec_files,
        ..
    } = snapshot_fields_from_files(&file_receiver)?;

    let num_rebuilder_threads = num_cpus::get_physical().saturating_sub(1).max(1);
    let storage = SnapshotStorageRebuilder::rebuild_storage(
        &accounts_db_fields,
        append_vec_files,
        file_receiver,
        num_rebuilder_threads,
        next_append_vec_id,
        SnapshotFrom::Dir,
        storage_access,
        obsolete_accounts,
    )?;

    Ok((storage, bank_fields, accounts_db_fields))
}

/// Reads the `snapshot_version` from a file. Before opening the file, its size
/// is compared to `MAX_SNAPSHOT_VERSION_FILE_SIZE`. If the size exceeds this
/// threshold, it is not opened and an error is returned.
fn snapshot_version_from_file(path: impl AsRef<Path>) -> io::Result<String> {
    // Check file size.
    let file_metadata = fs::metadata(&path).map_err(|err| {
        IoError::other(format!(
            "failed to query snapshot version file metadata '{}': {err}",
            path.as_ref().display(),
        ))
    })?;
    let file_size = file_metadata.len();
    if file_size > MAX_SNAPSHOT_VERSION_FILE_SIZE {
        let error_message = format!(
            "snapshot version file too large: '{}' has {} bytes (max size is {} bytes)",
            path.as_ref().display(),
            file_size,
            MAX_SNAPSHOT_VERSION_FILE_SIZE,
        );
        return Err(IoError::other(error_message));
    }

    // Read snapshot_version from file.
    let mut snapshot_version = String::new();
    let mut file = fs::File::open(&path).map_err(|err| {
        IoError::other(format!(
            "failed to open snapshot version file '{}': {err}",
            path.as_ref().display()
        ))
    })?;
    file.read_to_string(&mut snapshot_version).map_err(|err| {
        IoError::other(format!(
            "failed to read snapshot version from file '{}': {err}",
            path.as_ref().display()
        ))
    })?;

    Ok(snapshot_version.trim().to_string())
}

/// Check if an incremental snapshot is compatible with a full snapshot.  This is done by checking
/// if the incremental snapshot's base slot is the same as the full snapshot's slot.
fn check_are_snapshots_compatible(
    full_snapshot_archive_info: &FullSnapshotArchiveInfo,
    incremental_snapshot_archive_info: Option<&IncrementalSnapshotArchiveInfo>,
) -> Result<()> {
    if incremental_snapshot_archive_info.is_none() {
        return Ok(());
    }

    let incremental_snapshot_archive_info = incremental_snapshot_archive_info.unwrap();

    (full_snapshot_archive_info.slot() == incremental_snapshot_archive_info.base_slot())
        .then_some(())
        .ok_or_else(|| {
            SnapshotError::MismatchedBaseSlot(
                full_snapshot_archive_info.slot(),
                incremental_snapshot_archive_info.base_slot(),
            )
        })
}

pub fn purge_old_snapshot_archives(
    full_snapshot_archives_dir: impl AsRef<Path>,
    incremental_snapshot_archives_dir: impl AsRef<Path>,
    maximum_full_snapshot_archives_to_retain: NonZeroUsize,
    maximum_incremental_snapshot_archives_to_retain: NonZeroUsize,
) {
    info!(
        "Purging old full snapshot archives in {}, retaining up to {} full snapshots",
        full_snapshot_archives_dir.as_ref().display(),
        maximum_full_snapshot_archives_to_retain
    );

    let mut full_snapshot_archives =
        snapshot_paths::get_full_snapshot_archives(&full_snapshot_archives_dir);
    full_snapshot_archives.sort_unstable();
    full_snapshot_archives.reverse();

    let num_to_retain = full_snapshot_archives
        .len()
        .min(maximum_full_snapshot_archives_to_retain.get());
    trace!(
        "There are {} full snapshot archives, retaining {}",
        full_snapshot_archives.len(),
        num_to_retain,
    );

    let (full_snapshot_archives_to_retain, full_snapshot_archives_to_remove) =
        if full_snapshot_archives.is_empty() {
            None
        } else {
            Some(full_snapshot_archives.split_at(num_to_retain))
        }
        .unwrap_or_default();

    let retained_full_snapshot_slots = full_snapshot_archives_to_retain
        .iter()
        .map(|ai| ai.slot())
        .collect::<HashSet<_>>();

    fn remove_archives<T: SnapshotArchiveInfoGetter>(archives: &[T]) {
        for path in archives.iter().map(|a| a.path()) {
            trace!("Removing snapshot archive: {}", path.display());
            let result = fs::remove_file(path);
            if let Err(err) = result {
                info!(
                    "Failed to remove snapshot archive '{}': {err}",
                    path.display()
                );
            }
        }
    }
    remove_archives(full_snapshot_archives_to_remove);

    info!(
        "Purging old incremental snapshot archives in {}, retaining up to {} incremental snapshots",
        incremental_snapshot_archives_dir.as_ref().display(),
        maximum_incremental_snapshot_archives_to_retain
    );
    let mut incremental_snapshot_archives_by_base_slot = HashMap::<Slot, Vec<_>>::new();
    for incremental_snapshot_archive in
        get_incremental_snapshot_archives(&incremental_snapshot_archives_dir)
    {
        incremental_snapshot_archives_by_base_slot
            .entry(incremental_snapshot_archive.base_slot())
            .or_default()
            .push(incremental_snapshot_archive)
    }

    let highest_full_snapshot_slot = retained_full_snapshot_slots.iter().max().copied();
    for (base_slot, mut incremental_snapshot_archives) in incremental_snapshot_archives_by_base_slot
    {
        incremental_snapshot_archives.sort_unstable();
        let num_to_retain = if Some(base_slot) == highest_full_snapshot_slot {
            maximum_incremental_snapshot_archives_to_retain.get()
        } else {
            usize::from(retained_full_snapshot_slots.contains(&base_slot))
        };
        trace!(
            "There are {} incremental snapshot archives for base slot {}, removing {} of them",
            incremental_snapshot_archives.len(),
            base_slot,
            incremental_snapshot_archives
                .len()
                .saturating_sub(num_to_retain),
        );

        incremental_snapshot_archives.truncate(
            incremental_snapshot_archives
                .len()
                .saturating_sub(num_to_retain),
        );
        remove_archives(&incremental_snapshot_archives);
    }
}

pub fn verify_unpacked_snapshots_dir_and_version(
    unpacked_snapshots_dir_and_version: &UnpackedSnapshotsDirAndVersion,
) -> Result<(SnapshotVersion, BankSnapshotInfo)> {
    info!(
        "snapshot version: {}",
        &unpacked_snapshots_dir_and_version.snapshot_version
    );

    let snapshot_version = unpacked_snapshots_dir_and_version.snapshot_version;
    let mut bank_snapshots =
        get_bank_snapshots(&unpacked_snapshots_dir_and_version.unpacked_snapshots_dir);
    if bank_snapshots.len() > 1 {
        return Err(IoError::other(format!(
            "invalid snapshot format: only one snapshot allowed, but found {}",
            bank_snapshots.len(),
        ))
        .into());
    }
    let root_paths = bank_snapshots.pop().ok_or_else(|| {
        IoError::other(format!(
            "no snapshots found in snapshots directory '{}'",
            unpacked_snapshots_dir_and_version
                .unpacked_snapshots_dir
                .display(),
        ))
    })?;
    Ok((snapshot_version, root_paths))
}

#[derive(Debug, Copy, Clone)]
/// allow tests to specify what happened to the serialized format
pub enum VerifyBank {
    /// the bank's serialized format is expected to be identical to what we are comparing against
    Deterministic,
    /// the serialized bank was 'reserialized' into a non-deterministic format
    /// so, deserialize both files and compare deserialized results
    NonDeterministic,
}

/// Purges all bank snapshots
pub fn purge_all_bank_snapshots(bank_snapshots_dir: impl AsRef<Path>) {
    let bank_snapshots = get_bank_snapshots(&bank_snapshots_dir);
    purge_bank_snapshots(&bank_snapshots);
}

/// Purges bank snapshots, retaining the newest `num_bank_snapshots_to_retain`
pub fn purge_old_bank_snapshots(
    bank_snapshots_dir: impl AsRef<Path>,
    num_bank_snapshots_to_retain: usize,
) {
    let mut bank_snapshots = get_bank_snapshots(&bank_snapshots_dir);

    bank_snapshots.sort_unstable();
    purge_bank_snapshots(
        bank_snapshots
            .iter()
            .rev()
            .skip(num_bank_snapshots_to_retain),
    );
}

/// At startup, purge old (i.e. unusable) bank snapshots
pub fn purge_old_bank_snapshots_at_startup(bank_snapshots_dir: impl AsRef<Path>) {
    purge_old_bank_snapshots(&bank_snapshots_dir, 1);

    let highest_bank_snapshot = get_highest_bank_snapshot(&bank_snapshots_dir);
    if let Some(highest_bank_snapshot) = highest_bank_snapshot {
        debug!(
            "Retained bank snapshot for slot {}, and purged the rest.",
            highest_bank_snapshot.slot
        );
    }
}

/// Purges bank snapshots that are older than `slot`
pub fn purge_bank_snapshots_older_than_slot(bank_snapshots_dir: impl AsRef<Path>, slot: Slot) {
    let mut bank_snapshots = get_bank_snapshots(&bank_snapshots_dir);
    bank_snapshots.retain(|bank_snapshot| bank_snapshot.slot < slot);
    purge_bank_snapshots(&bank_snapshots);
}

/// Purges all `bank_snapshots`
///
/// Does not exit early if there is an error while purging a bank snapshot.
fn purge_bank_snapshots<'a>(bank_snapshots: impl IntoIterator<Item = &'a BankSnapshotInfo>) {
    for snapshot_dir in bank_snapshots.into_iter().map(|s| &s.snapshot_dir) {
        if purge_bank_snapshot(snapshot_dir).is_err() {
            warn!("Failed to purge bank snapshot: {}", snapshot_dir.display());
        }
    }
}

/// Remove the bank snapshot at this path
pub fn purge_bank_snapshot(bank_snapshot_dir: impl AsRef<Path>) -> Result<()> {
    const FN_ERR: &str = "failed to purge bank snapshot";
    let accounts_hardlinks_dir = bank_snapshot_dir
        .as_ref()
        .join(snapshot_paths::SNAPSHOT_ACCOUNTS_HARDLINKS);
    if accounts_hardlinks_dir.is_dir() {
        // This directory contain symlinks to all accounts snapshot directories.
        // They should all be removed.
        let read_dir = fs::read_dir(&accounts_hardlinks_dir).map_err(|err| {
            IoError::other(format!(
                "{FN_ERR}: failed to read accounts hardlinks dir '{}': {err}",
                accounts_hardlinks_dir.display(),
            ))
        })?;
        for entry in read_dir {
            let accounts_hardlink_dir = entry?.path();
            let accounts_hardlink_dir = fs::read_link(&accounts_hardlink_dir).map_err(|err| {
                IoError::other(format!(
                    "{FN_ERR}: failed to read symlink '{}': {err}",
                    accounts_hardlink_dir.display(),
                ))
            })?;
            move_and_async_delete_path(&accounts_hardlink_dir);
        }
    }
    fs::remove_dir_all(&bank_snapshot_dir).map_err(|err| {
        IoError::other(format!(
            "{FN_ERR}: failed to remove dir '{}': {err}",
            bank_snapshot_dir.as_ref().display(),
        ))
    })?;
    Ok(())
}

pub fn should_take_full_snapshot(
    block_height: Slot,
    full_snapshot_archive_interval_slots: Slot,
) -> bool {
    block_height.is_multiple_of(full_snapshot_archive_interval_slots)
}

pub fn should_take_incremental_snapshot(
    block_height: Slot,
    incremental_snapshot_archive_interval_slots: Slot,
    latest_full_snapshot_slot: Option<Slot>,
) -> bool {
    block_height.is_multiple_of(incremental_snapshot_archive_interval_slots)
        && latest_full_snapshot_slot.is_some()
}

/// Creates an "accounts path" directory for tests
///
/// This temporary directory will contain the "run" and "snapshot"
/// sub-directories required by a validator.
#[cfg(feature = "dev-context-only-utils")]
pub fn create_tmp_accounts_dir_for_tests() -> (TempDir, PathBuf) {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let account_dir = create_accounts_run_and_snapshot_dirs(&tmp_dir).unwrap().0;
    (tmp_dir, account_dir)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_snapshots::{
            paths::{
                get_full_snapshot_archives, get_highest_full_snapshot_archive_slot,
                get_highest_incremental_snapshot_archive_slot,
            },
            snapshot_config::{
                DEFAULT_MAX_FULL_SNAPSHOT_ARCHIVES_TO_RETAIN,
                DEFAULT_MAX_INCREMENTAL_SNAPSHOT_ARCHIVES_TO_RETAIN,
            },
        },
        assert_matches::assert_matches,
        bincode::{deserialize_from, serialize_into},
        solana_accounts_db::accounts_file::AccountsFileProvider,
        solana_hash::Hash,
        std::{convert::TryFrom, mem::size_of},
        tempfile::NamedTempFile,
        test_case::test_case,
    };

    #[test]
    fn test_serialize_snapshot_data_file_under_limit() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let expected_consumed_size = size_of::<u32>() as u64;
        let consumed_size = serialize_snapshot_data_file_capped(
            &temp_dir.path().join("data-file"),
            expected_consumed_size,
            |stream| {
                serialize_into(stream, &2323_u32)?;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(consumed_size, expected_consumed_size);
    }

    #[test]
    fn test_serialize_snapshot_data_file_over_limit() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let expected_consumed_size = size_of::<u32>() as u64;
        let result = serialize_snapshot_data_file_capped(
            &temp_dir.path().join("data-file"),
            expected_consumed_size - 1,
            |stream| {
                serialize_into(stream, &2323_u32)?;
                Ok(())
            },
        );
        assert_matches!(result, Err(SnapshotError::Io(ref message)) if message.to_string().starts_with("too large snapshot data file to serialize"));
    }

    #[test]
    fn test_deserialize_snapshot_data_file_under_limit() {
        let expected_data = 2323_u32;
        let expected_consumed_size = size_of::<u32>() as u64;

        let temp_dir = tempfile::TempDir::new().unwrap();
        serialize_snapshot_data_file_capped(
            &temp_dir.path().join("data-file"),
            expected_consumed_size,
            |stream| {
                serialize_into(stream, &expected_data)?;
                Ok(())
            },
        )
        .unwrap();

        let snapshot_root_paths = SnapshotRootPaths {
            full_snapshot_root_file_path: temp_dir.path().join("data-file"),
            incremental_snapshot_root_file_path: None,
        };

        let actual_data = deserialize_snapshot_data_files_capped(
            &snapshot_root_paths,
            expected_consumed_size,
            |stream| {
                Ok(deserialize_from::<_, u32>(
                    &mut stream.full_snapshot_stream,
                )?)
            },
        )
        .unwrap();
        assert_eq!(actual_data, expected_data);
    }

    #[test]
    fn test_deserialize_snapshot_data_file_over_limit() {
        let expected_data = 2323_u32;
        let expected_consumed_size = size_of::<u32>() as u64;

        let temp_dir = tempfile::TempDir::new().unwrap();
        serialize_snapshot_data_file_capped(
            &temp_dir.path().join("data-file"),
            expected_consumed_size,
            |stream| {
                serialize_into(stream, &expected_data)?;
                Ok(())
            },
        )
        .unwrap();

        let snapshot_root_paths = SnapshotRootPaths {
            full_snapshot_root_file_path: temp_dir.path().join("data-file"),
            incremental_snapshot_root_file_path: None,
        };

        let result = deserialize_snapshot_data_files_capped(
            &snapshot_root_paths,
            expected_consumed_size - 1,
            |stream| {
                Ok(deserialize_from::<_, u32>(
                    &mut stream.full_snapshot_stream,
                )?)
            },
        );
        assert_matches!(result, Err(SnapshotError::Io(ref message)) if message.to_string().starts_with("too large snapshot data file to deserialize"));
    }

    #[test]
    fn test_deserialize_snapshot_data_file_extra_data() {
        let expected_data = 2323_u32;
        let expected_consumed_size = size_of::<u32>() as u64;

        let temp_dir = tempfile::TempDir::new().unwrap();
        serialize_snapshot_data_file_capped(
            &temp_dir.path().join("data-file"),
            expected_consumed_size * 2,
            |stream| {
                serialize_into(&mut *stream, &expected_data)?;
                serialize_into(&mut *stream, &expected_data)?;
                Ok(())
            },
        )
        .unwrap();

        let snapshot_root_paths = SnapshotRootPaths {
            full_snapshot_root_file_path: temp_dir.path().join("data-file"),
            incremental_snapshot_root_file_path: None,
        };

        let result = deserialize_snapshot_data_files_capped(
            &snapshot_root_paths,
            expected_consumed_size * 2,
            |stream| {
                Ok(deserialize_from::<_, u32>(
                    &mut stream.full_snapshot_stream,
                )?)
            },
        );
        assert_matches!(result, Err(SnapshotError::Io(ref message)) if message.to_string().starts_with("invalid snapshot data file"));
    }

    #[test]
    fn test_snapshot_version_from_file_under_limit() {
        let file_content = SnapshotVersion::default().as_str();
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(file_content.as_bytes()).unwrap();
        let version_from_file = snapshot_version_from_file(file.path()).unwrap();
        assert_eq!(version_from_file, file_content);
    }

    #[test]
    fn test_snapshot_version_from_file_over_limit() {
        let over_limit_size = usize::try_from(MAX_SNAPSHOT_VERSION_FILE_SIZE + 1).unwrap();
        let file_content = vec![7u8; over_limit_size];
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&file_content).unwrap();
        assert_matches!(
            snapshot_version_from_file(file.path()),
            Err(ref message) if message.to_string().starts_with("snapshot version file too large")
        );
    }

    #[test]
    fn test_check_are_snapshots_compatible() {
        let slot1: Slot = 1234;
        let slot2: Slot = 5678;
        let slot3: Slot = 999_999;

        let full_snapshot_archive_info = FullSnapshotArchiveInfo::new_from_path(PathBuf::from(
            format!("/dir/snapshot-{}-{}.tar.zst", slot1, Hash::new_unique()),
        ))
        .unwrap();

        assert!(check_are_snapshots_compatible(&full_snapshot_archive_info, None,).is_ok());

        let incremental_snapshot_archive_info =
            IncrementalSnapshotArchiveInfo::new_from_path(PathBuf::from(format!(
                "/dir/incremental-snapshot-{}-{}-{}.tar.zst",
                slot1,
                slot2,
                Hash::new_unique()
            )))
            .unwrap();

        assert!(check_are_snapshots_compatible(
            &full_snapshot_archive_info,
            Some(&incremental_snapshot_archive_info)
        )
        .is_ok());

        let incremental_snapshot_archive_info =
            IncrementalSnapshotArchiveInfo::new_from_path(PathBuf::from(format!(
                "/dir/incremental-snapshot-{}-{}-{}.tar.zst",
                slot2,
                slot3,
                Hash::new_unique()
            )))
            .unwrap();

        assert!(check_are_snapshots_compatible(
            &full_snapshot_archive_info,
            Some(&incremental_snapshot_archive_info)
        )
        .is_err());
    }

    /// A test heler function that creates bank snapshot files
    fn common_create_bank_snapshot_files(
        bank_snapshots_dir: &Path,
        min_slot: Slot,
        max_slot: Slot,
    ) {
        for slot in min_slot..max_slot {
            let snapshot_dir = snapshot_paths::get_bank_snapshot_dir(bank_snapshots_dir, slot);
            fs::create_dir_all(&snapshot_dir).unwrap();

            let snapshot_filename = snapshot_paths::get_snapshot_file_name(slot);
            let snapshot_path = snapshot_dir.join(snapshot_filename);
            fs::File::create(snapshot_path).unwrap();

            let status_cache_file =
                snapshot_dir.join(snapshot_paths::SNAPSHOT_STATUS_CACHE_FILENAME);
            fs::File::create(status_cache_file).unwrap();

            let version_path = snapshot_dir.join(snapshot_paths::SNAPSHOT_VERSION_FILENAME);
            fs::write(version_path, SnapshotVersion::default().as_str().as_bytes()).unwrap();
        }
    }

    #[test]
    fn test_get_bank_snapshots() {
        let temp_snapshots_dir = tempfile::TempDir::new().unwrap();
        let min_slot = 10;
        let max_slot = 20;
        common_create_bank_snapshot_files(temp_snapshots_dir.path(), min_slot, max_slot);

        let bank_snapshots = get_bank_snapshots(temp_snapshots_dir.path());
        assert_eq!(bank_snapshots.len() as Slot, max_slot - min_slot);
    }

    #[test]
    fn test_get_highest_bank_snapshot() {
        let temp_snapshots_dir = tempfile::TempDir::new().unwrap();
        let min_slot = 99;
        let max_slot = 123;
        common_create_bank_snapshot_files(temp_snapshots_dir.path(), min_slot, max_slot);

        let highest_bank_snapshot = get_highest_bank_snapshot(temp_snapshots_dir.path());
        assert!(highest_bank_snapshot.is_some());
        assert_eq!(highest_bank_snapshot.unwrap().slot, max_slot - 1);
    }

    /// A test helper function that creates full and incremental snapshot archive files.  Creates
    /// full snapshot files in the range (`min_full_snapshot_slot`, `max_full_snapshot_slot`], and
    /// incremental snapshot files in the range (`min_incremental_snapshot_slot`,
    /// `max_incremental_snapshot_slot`].  Additionally, "bad" files are created for both full and
    /// incremental snapshots to ensure the tests properly filter them out.
    fn common_create_snapshot_archive_files(
        full_snapshot_archives_dir: &Path,
        incremental_snapshot_archives_dir: &Path,
        min_full_snapshot_slot: Slot,
        max_full_snapshot_slot: Slot,
        min_incremental_snapshot_slot: Slot,
        max_incremental_snapshot_slot: Slot,
    ) {
        fs::create_dir_all(full_snapshot_archives_dir).unwrap();
        fs::create_dir_all(incremental_snapshot_archives_dir).unwrap();
        for full_snapshot_slot in min_full_snapshot_slot..max_full_snapshot_slot {
            for incremental_snapshot_slot in
                min_incremental_snapshot_slot..max_incremental_snapshot_slot
            {
                let snapshot_filename = format!(
                    "incremental-snapshot-{}-{}-{}.tar.zst",
                    full_snapshot_slot,
                    incremental_snapshot_slot,
                    Hash::default()
                );
                let snapshot_filepath = incremental_snapshot_archives_dir.join(snapshot_filename);
                fs::File::create(snapshot_filepath).unwrap();
            }

            let snapshot_filename = format!(
                "snapshot-{}-{}.tar.zst",
                full_snapshot_slot,
                Hash::default()
            );
            let snapshot_filepath = full_snapshot_archives_dir.join(snapshot_filename);
            fs::File::create(snapshot_filepath).unwrap();

            // Add in an incremental snapshot with a bad filename and high slot to ensure filename are filtered and sorted correctly
            let bad_filename = format!(
                "incremental-snapshot-{}-{}-bad!hash.tar.zst",
                full_snapshot_slot,
                max_incremental_snapshot_slot + 1,
            );
            let bad_filepath = incremental_snapshot_archives_dir.join(bad_filename);
            fs::File::create(bad_filepath).unwrap();
        }

        // Add in a snapshot with a bad filename and high slot to ensure filename are filtered and
        // sorted correctly
        let bad_filename = format!("snapshot-{}-bad!hash.tar.zst", max_full_snapshot_slot + 1);
        let bad_filepath = full_snapshot_archives_dir.join(bad_filename);
        fs::File::create(bad_filepath).unwrap();
    }

    #[test]
    fn test_get_full_snapshot_archives() {
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let min_slot = 123;
        let max_slot = 456;
        common_create_snapshot_archive_files(
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            min_slot,
            max_slot,
            0,
            0,
        );

        let snapshot_archives = get_full_snapshot_archives(full_snapshot_archives_dir);
        assert_eq!(snapshot_archives.len() as Slot, max_slot - min_slot);
    }

    #[test]
    fn test_get_full_snapshot_archives_remote() {
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let min_slot = 123;
        let max_slot = 456;
        common_create_snapshot_archive_files(
            &full_snapshot_archives_dir
                .path()
                .join(snapshot_paths::SNAPSHOT_ARCHIVE_DOWNLOAD_DIR),
            &incremental_snapshot_archives_dir
                .path()
                .join(snapshot_paths::SNAPSHOT_ARCHIVE_DOWNLOAD_DIR),
            min_slot,
            max_slot,
            0,
            0,
        );

        let snapshot_archives = get_full_snapshot_archives(full_snapshot_archives_dir);
        assert_eq!(snapshot_archives.len() as Slot, max_slot - min_slot);
        assert!(snapshot_archives.iter().all(|info| info.is_remote()));
    }

    #[test]
    fn test_get_incremental_snapshot_archives() {
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let min_full_snapshot_slot = 12;
        let max_full_snapshot_slot = 23;
        let min_incremental_snapshot_slot = 34;
        let max_incremental_snapshot_slot = 45;
        common_create_snapshot_archive_files(
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            min_full_snapshot_slot,
            max_full_snapshot_slot,
            min_incremental_snapshot_slot,
            max_incremental_snapshot_slot,
        );

        let incremental_snapshot_archives =
            get_incremental_snapshot_archives(incremental_snapshot_archives_dir);
        assert_eq!(
            incremental_snapshot_archives.len() as Slot,
            (max_full_snapshot_slot - min_full_snapshot_slot)
                * (max_incremental_snapshot_slot - min_incremental_snapshot_slot)
        );
    }

    #[test]
    fn test_get_incremental_snapshot_archives_remote() {
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let min_full_snapshot_slot = 12;
        let max_full_snapshot_slot = 23;
        let min_incremental_snapshot_slot = 34;
        let max_incremental_snapshot_slot = 45;
        common_create_snapshot_archive_files(
            &full_snapshot_archives_dir
                .path()
                .join(snapshot_paths::SNAPSHOT_ARCHIVE_DOWNLOAD_DIR),
            &incremental_snapshot_archives_dir
                .path()
                .join(snapshot_paths::SNAPSHOT_ARCHIVE_DOWNLOAD_DIR),
            min_full_snapshot_slot,
            max_full_snapshot_slot,
            min_incremental_snapshot_slot,
            max_incremental_snapshot_slot,
        );

        let incremental_snapshot_archives =
            get_incremental_snapshot_archives(incremental_snapshot_archives_dir);
        assert_eq!(
            incremental_snapshot_archives.len() as Slot,
            (max_full_snapshot_slot - min_full_snapshot_slot)
                * (max_incremental_snapshot_slot - min_incremental_snapshot_slot)
        );
        assert!(incremental_snapshot_archives
            .iter()
            .all(|info| info.is_remote()));
    }

    #[test]
    fn test_get_highest_full_snapshot_archive_slot() {
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let min_slot = 123;
        let max_slot = 456;
        common_create_snapshot_archive_files(
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            min_slot,
            max_slot,
            0,
            0,
        );

        assert_eq!(
            get_highest_full_snapshot_archive_slot(full_snapshot_archives_dir.path()),
            Some(max_slot - 1)
        );
    }

    #[test]
    fn test_get_highest_incremental_snapshot_slot() {
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let min_full_snapshot_slot = 12;
        let max_full_snapshot_slot = 23;
        let min_incremental_snapshot_slot = 34;
        let max_incremental_snapshot_slot = 45;
        common_create_snapshot_archive_files(
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            min_full_snapshot_slot,
            max_full_snapshot_slot,
            min_incremental_snapshot_slot,
            max_incremental_snapshot_slot,
        );

        for full_snapshot_slot in min_full_snapshot_slot..max_full_snapshot_slot {
            assert_eq!(
                get_highest_incremental_snapshot_archive_slot(
                    incremental_snapshot_archives_dir.path(),
                    full_snapshot_slot
                ),
                Some(max_incremental_snapshot_slot - 1)
            );
        }

        assert_eq!(
            get_highest_incremental_snapshot_archive_slot(
                incremental_snapshot_archives_dir.path(),
                max_full_snapshot_slot
            ),
            None
        );
    }

    fn common_test_purge_old_snapshot_archives(
        snapshot_names: &[&String],
        maximum_full_snapshot_archives_to_retain: NonZeroUsize,
        maximum_incremental_snapshot_archives_to_retain: NonZeroUsize,
        expected_snapshots: &[&String],
    ) {
        let temp_snap_dir = tempfile::TempDir::new().unwrap();

        for snap_name in snapshot_names {
            let snap_path = temp_snap_dir.path().join(snap_name);
            let mut _snap_file = fs::File::create(snap_path);
        }
        purge_old_snapshot_archives(
            temp_snap_dir.path(),
            temp_snap_dir.path(),
            maximum_full_snapshot_archives_to_retain,
            maximum_incremental_snapshot_archives_to_retain,
        );

        let mut retained_snaps = HashSet::new();
        for entry in fs::read_dir(temp_snap_dir.path()).unwrap() {
            let entry_path_buf = entry.unwrap().path();
            let entry_path = entry_path_buf.as_path();
            let snapshot_name = entry_path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_string();
            retained_snaps.insert(snapshot_name);
        }

        for snap_name in expected_snapshots {
            assert!(
                retained_snaps.contains(snap_name.as_str()),
                "{snap_name} not found"
            );
        }
        assert_eq!(retained_snaps.len(), expected_snapshots.len());
    }

    #[test]
    fn test_purge_old_full_snapshot_archives() {
        let snap1_name = format!("snapshot-1-{}.tar.zst", Hash::default());
        let snap2_name = format!("snapshot-3-{}.tar.zst", Hash::default());
        let snap3_name = format!("snapshot-50-{}.tar.zst", Hash::default());
        let snapshot_names = vec![&snap1_name, &snap2_name, &snap3_name];

        // expecting only the newest to be retained
        let expected_snapshots = vec![&snap3_name];
        common_test_purge_old_snapshot_archives(
            &snapshot_names,
            NonZeroUsize::new(1).unwrap(),
            DEFAULT_MAX_INCREMENTAL_SNAPSHOT_ARCHIVES_TO_RETAIN,
            &expected_snapshots,
        );

        // retaining 2, expecting the 2 newest to be retained
        let expected_snapshots = vec![&snap2_name, &snap3_name];
        common_test_purge_old_snapshot_archives(
            &snapshot_names,
            NonZeroUsize::new(2).unwrap(),
            DEFAULT_MAX_INCREMENTAL_SNAPSHOT_ARCHIVES_TO_RETAIN,
            &expected_snapshots,
        );

        // retaining 3, all three should be retained
        let expected_snapshots = vec![&snap1_name, &snap2_name, &snap3_name];
        common_test_purge_old_snapshot_archives(
            &snapshot_names,
            NonZeroUsize::new(3).unwrap(),
            DEFAULT_MAX_INCREMENTAL_SNAPSHOT_ARCHIVES_TO_RETAIN,
            &expected_snapshots,
        );
    }

    /// Mimic a running node's behavior w.r.t. purging old snapshot archives.  Take snapshots in a
    /// loop, and periodically purge old snapshot archives.  After purging, check to make sure the
    /// snapshot archives on disk are correct.
    #[test]
    fn test_purge_old_full_snapshot_archives_in_the_loop() {
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let maximum_snapshots_to_retain = NonZeroUsize::new(5).unwrap();
        let starting_slot: Slot = 42;

        for slot in (starting_slot..).take(100) {
            let full_snapshot_archive_file_name =
                format!("snapshot-{}-{}.tar.zst", slot, Hash::default());
            let full_snapshot_archive_path = full_snapshot_archives_dir
                .as_ref()
                .join(full_snapshot_archive_file_name);
            fs::File::create(full_snapshot_archive_path).unwrap();

            // don't purge-and-check until enough snapshot archives have been created
            if slot < starting_slot + maximum_snapshots_to_retain.get() as Slot {
                continue;
            }

            // purge infrequently, so there will always be snapshot archives to purge
            if slot % (maximum_snapshots_to_retain.get() as Slot * 2) != 0 {
                continue;
            }

            purge_old_snapshot_archives(
                &full_snapshot_archives_dir,
                &incremental_snapshot_archives_dir,
                maximum_snapshots_to_retain,
                NonZeroUsize::new(usize::MAX).unwrap(),
            );
            let mut full_snapshot_archives =
                get_full_snapshot_archives(&full_snapshot_archives_dir);
            full_snapshot_archives.sort_unstable();
            assert_eq!(
                full_snapshot_archives.len(),
                maximum_snapshots_to_retain.get()
            );
            assert_eq!(full_snapshot_archives.last().unwrap().slot(), slot);
            for (i, full_snapshot_archive) in full_snapshot_archives.iter().rev().enumerate() {
                assert_eq!(full_snapshot_archive.slot(), slot - i as Slot);
            }
        }
    }

    #[test]
    fn test_purge_old_incremental_snapshot_archives() {
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let starting_slot = 100_000;

        let maximum_incremental_snapshot_archives_to_retain =
            DEFAULT_MAX_INCREMENTAL_SNAPSHOT_ARCHIVES_TO_RETAIN;
        let maximum_full_snapshot_archives_to_retain = DEFAULT_MAX_FULL_SNAPSHOT_ARCHIVES_TO_RETAIN;

        let incremental_snapshot_interval = 100;
        let num_incremental_snapshots_per_full_snapshot =
            maximum_incremental_snapshot_archives_to_retain.get() * 2;
        let full_snapshot_interval =
            incremental_snapshot_interval * num_incremental_snapshots_per_full_snapshot;

        let mut snapshot_filenames = vec![];
        (starting_slot..)
            .step_by(full_snapshot_interval)
            .take(
                maximum_full_snapshot_archives_to_retain
                    .checked_mul(NonZeroUsize::new(2).unwrap())
                    .unwrap()
                    .get(),
            )
            .for_each(|full_snapshot_slot| {
                let snapshot_filename = format!(
                    "snapshot-{}-{}.tar.zst",
                    full_snapshot_slot,
                    Hash::default()
                );
                let snapshot_path = full_snapshot_archives_dir.path().join(&snapshot_filename);
                fs::File::create(snapshot_path).unwrap();
                snapshot_filenames.push(snapshot_filename);

                (full_snapshot_slot..)
                    .step_by(incremental_snapshot_interval)
                    .take(num_incremental_snapshots_per_full_snapshot)
                    .skip(1)
                    .for_each(|incremental_snapshot_slot| {
                        let snapshot_filename = format!(
                            "incremental-snapshot-{}-{}-{}.tar.zst",
                            full_snapshot_slot,
                            incremental_snapshot_slot,
                            Hash::default()
                        );
                        let snapshot_path = incremental_snapshot_archives_dir
                            .path()
                            .join(&snapshot_filename);
                        fs::File::create(snapshot_path).unwrap();
                        snapshot_filenames.push(snapshot_filename);
                    });
            });

        purge_old_snapshot_archives(
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            maximum_full_snapshot_archives_to_retain,
            maximum_incremental_snapshot_archives_to_retain,
        );

        // Ensure correct number of full snapshot archives are purged/retained
        let mut remaining_full_snapshot_archives =
            get_full_snapshot_archives(full_snapshot_archives_dir.path());
        assert_eq!(
            remaining_full_snapshot_archives.len(),
            maximum_full_snapshot_archives_to_retain.get(),
        );
        remaining_full_snapshot_archives.sort_unstable();
        let latest_full_snapshot_archive_slot =
            remaining_full_snapshot_archives.last().unwrap().slot();

        // Ensure correct number of incremental snapshot archives are purged/retained
        // For each additional full snapshot archive, one additional (the newest)
        // incremental snapshot archive is retained. This is accounted for by the
        // `+ maximum_full_snapshot_archives_to_retain.saturating_sub(1)`
        let mut remaining_incremental_snapshot_archives =
            get_incremental_snapshot_archives(incremental_snapshot_archives_dir.path());
        assert_eq!(
            remaining_incremental_snapshot_archives.len(),
            maximum_incremental_snapshot_archives_to_retain
                .get()
                .saturating_add(
                    maximum_full_snapshot_archives_to_retain
                        .get()
                        .saturating_sub(1)
                )
        );
        remaining_incremental_snapshot_archives.sort_unstable();
        remaining_incremental_snapshot_archives.reverse();

        // Ensure there exists one incremental snapshot all but the latest full snapshot
        for i in (1..maximum_full_snapshot_archives_to_retain.get()).rev() {
            let incremental_snapshot_archive =
                remaining_incremental_snapshot_archives.pop().unwrap();

            let expected_base_slot =
                latest_full_snapshot_archive_slot - (i * full_snapshot_interval) as u64;
            assert_eq!(incremental_snapshot_archive.base_slot(), expected_base_slot);
            let expected_slot = expected_base_slot
                + (full_snapshot_interval - incremental_snapshot_interval) as u64;
            assert_eq!(incremental_snapshot_archive.slot(), expected_slot);
        }

        // Ensure all remaining incremental snapshots are only for the latest full snapshot
        for incremental_snapshot_archive in &remaining_incremental_snapshot_archives {
            assert_eq!(
                incremental_snapshot_archive.base_slot(),
                latest_full_snapshot_archive_slot
            );
        }

        // Ensure the remaining incremental snapshots are at the right slot
        let expected_remaining_incremental_snapshot_archive_slots =
            (latest_full_snapshot_archive_slot..)
                .step_by(incremental_snapshot_interval)
                .take(num_incremental_snapshots_per_full_snapshot)
                .skip(
                    num_incremental_snapshots_per_full_snapshot
                        - maximum_incremental_snapshot_archives_to_retain.get(),
                )
                .collect::<HashSet<_>>();

        let actual_remaining_incremental_snapshot_archive_slots =
            remaining_incremental_snapshot_archives
                .iter()
                .map(|snapshot| snapshot.slot())
                .collect::<HashSet<_>>();
        assert_eq!(
            actual_remaining_incremental_snapshot_archive_slots,
            expected_remaining_incremental_snapshot_archive_slots
        );
    }

    #[test]
    fn test_purge_all_incremental_snapshot_archives_when_no_full_snapshot_archives() {
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();

        for snapshot_filenames in [
            format!("incremental-snapshot-100-120-{}.tar.zst", Hash::default()),
            format!("incremental-snapshot-100-140-{}.tar.zst", Hash::default()),
            format!("incremental-snapshot-100-160-{}.tar.zst", Hash::default()),
            format!("incremental-snapshot-100-180-{}.tar.zst", Hash::default()),
            format!("incremental-snapshot-200-220-{}.tar.zst", Hash::default()),
            format!("incremental-snapshot-200-240-{}.tar.zst", Hash::default()),
            format!("incremental-snapshot-200-260-{}.tar.zst", Hash::default()),
            format!("incremental-snapshot-200-280-{}.tar.zst", Hash::default()),
        ] {
            let snapshot_path = incremental_snapshot_archives_dir
                .path()
                .join(snapshot_filenames);
            fs::File::create(snapshot_path).unwrap();
        }

        purge_old_snapshot_archives(
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            NonZeroUsize::new(usize::MAX).unwrap(),
            NonZeroUsize::new(usize::MAX).unwrap(),
        );

        let remaining_incremental_snapshot_archives =
            get_incremental_snapshot_archives(incremental_snapshot_archives_dir.path());
        assert!(remaining_incremental_snapshot_archives.is_empty());
    }

    #[test]
    fn test_get_snapshot_accounts_hardlink_dir() {
        let slot: Slot = 1;

        let mut account_paths_set: HashSet<PathBuf> = HashSet::new();

        let bank_snapshots_dir_tmp = tempfile::TempDir::new().unwrap();
        let bank_snapshot_dir = bank_snapshots_dir_tmp.path().join(slot.to_string());
        let accounts_hardlinks_dir =
            bank_snapshot_dir.join(snapshot_paths::SNAPSHOT_ACCOUNTS_HARDLINKS);
        fs::create_dir_all(&accounts_hardlinks_dir).unwrap();

        let (_tmp_dir, accounts_dir) = create_tmp_accounts_dir_for_tests();
        let appendvec_filename = format!("{slot}.0");
        let appendvec_path = accounts_dir.join(appendvec_filename);

        let ret = get_snapshot_accounts_hardlink_dir(
            &appendvec_path,
            slot,
            &mut account_paths_set,
            &accounts_hardlinks_dir,
        );
        assert!(ret.is_ok());

        let wrong_appendvec_path = appendvec_path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(appendvec_path.file_name().unwrap());
        let ret = get_snapshot_accounts_hardlink_dir(
            &wrong_appendvec_path,
            slot,
            &mut account_paths_set,
            accounts_hardlinks_dir,
        );

        assert_matches!(
            ret,
            Err(GetSnapshotAccountsHardLinkDirError::GetAccountPath(_))
        );
    }

    #[test]
    fn test_get_snapshot_file_kind() {
        assert_eq!(None, get_snapshot_file_kind("file.txt"));
        assert_eq!(
            Some(SnapshotFileKind::Version),
            get_snapshot_file_kind(snapshot_paths::SNAPSHOT_VERSION_FILENAME)
        );
        assert_eq!(
            Some(SnapshotFileKind::BankFields),
            get_snapshot_file_kind("1234")
        );
        assert_eq!(
            Some(SnapshotFileKind::Storage),
            get_snapshot_file_kind("1000.999")
        );
    }

    #[test_case(0)]
    #[test_case(1)]
    #[test_case(10)]
    fn test_serialize_deserialize_account_storage_entries(num_storages: u64) {
        let temp_dir = tempfile::tempdir().unwrap();
        let bank_snapshot_dir = temp_dir.path();
        let snapshot_slot = num_storages + 1 as Slot;

        // Create AccountStorageEntries
        let mut snapshot_storages = Vec::new();
        for i in 0..num_storages {
            let storage = Arc::new(AccountStorageEntry::new(
                &PathBuf::new(),
                i,        // Incrementing slot
                i as u32, // Incrementing id
                1024,
                AccountsFileProvider::AppendVec,
                StorageAccess::File,
            ));
            snapshot_storages.push(storage);
        }

        // write obsolete accounts to snapshot
        write_obsolete_accounts_to_snapshot(bank_snapshot_dir, &snapshot_storages, snapshot_slot)
            .unwrap();

        // Deserialize
        let deserialized_accounts =
            deserialize_obsolete_accounts(bank_snapshot_dir, MAX_OBSOLETE_ACCOUNTS_FILE_SIZE)
                .unwrap();

        // Verify
        for storage in &snapshot_storages {
            assert!(deserialized_accounts.remove(&storage.slot()).unwrap().2 == 0);
        }
    }

    #[test]
    #[should_panic(expected = "too large obsolete accounts file to serialize")]
    fn test_serialize_obsolete_accounts_too_large_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let bank_snapshot_dir = temp_dir.path();
        let num_storages = 10;
        let snapshot_slot = num_storages + 1 as Slot;

        // Create AccountStorageEntries
        let mut snapshot_storages = Vec::new();
        for i in 0..num_storages {
            let storage = Arc::new(AccountStorageEntry::new(
                &PathBuf::new(),
                i,        // Incrementing slot
                i as u32, // Incrementing id
                1024,
                AccountsFileProvider::AppendVec,
                StorageAccess::File,
            ));
            snapshot_storages.push(storage);
        }

        // write obsolete accounts to snapshot
        let obsolete_accounts =
            SerdeObsoleteAccountsMap::new_from_storages(&snapshot_storages, snapshot_slot);

        // Limit the file size to something low for the test
        serialize_obsolete_accounts(bank_snapshot_dir, &obsolete_accounts, 100).unwrap();
    }

    #[test]
    #[should_panic(expected = "too large obsolete accounts file to deserialize")]
    fn test_deserialize_obsolete_accounts_too_large_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let bank_snapshot_dir = temp_dir.path();
        let num_storages = 10;
        let snapshot_slot = num_storages + 1 as Slot;

        // Create AccountStorageEntries
        let mut snapshot_storages = Vec::new();
        for i in 0..num_storages {
            let storage = Arc::new(AccountStorageEntry::new(
                &PathBuf::new(),
                i,        // Incrementing slot
                i as u32, // Incrementing id
                1024,
                AccountsFileProvider::AppendVec,
                StorageAccess::File,
            ));
            snapshot_storages.push(storage);
        }

        // Write obsolete accounts to snapshot
        write_obsolete_accounts_to_snapshot(bank_snapshot_dir, &snapshot_storages, snapshot_slot)
            .unwrap();

        // Set a very low maximum file size for deserialization
        // This should panic
        deserialize_obsolete_accounts(bank_snapshot_dir, 100).unwrap();
    }
}
