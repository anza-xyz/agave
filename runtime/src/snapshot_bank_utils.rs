#[cfg(feature = "dev-context-only-utils")]
use {
    crate::{
        bank::BankFieldsToDeserialize,
        serde_snapshot::fields_from_streams,
        snapshot_utils::{
            deserialize_snapshot_data_files, verify_unpacked_snapshots_dir_and_version,
            SnapshotRootPaths, UnpackedSnapshotsDirAndVersion,
        },
    },
    solana_accounts_db::accounts_file::StorageAccess,
    tempfile::TempDir,
};
use {
    crate::{
        bank::{Bank, BankSlotDelta},
        epoch_stakes::VersionedEpochStakes,
        runtime_config::RuntimeConfig,
        serde_snapshot::{
            reconstruct_bank_from_fields, BankIncrementalSnapshotPersistence,
            SnapshotAccountsDbFields, SnapshotBankFields,
        },
        snapshot_archive_info::{
            FullSnapshotArchiveInfo, IncrementalSnapshotArchiveInfo, SnapshotArchiveInfoGetter,
        },
        snapshot_config::SnapshotConfig,
        snapshot_hash::SnapshotHash,
        snapshot_package::{AccountsPackage, AccountsPackageKind, SnapshotKind, SnapshotPackage},
        snapshot_utils::{
            self, deserialize_snapshot_data_file, get_highest_bank_snapshot_post,
            get_highest_full_snapshot_archive_info, get_highest_incremental_snapshot_archive_info,
            rebuild_storages_from_snapshot_dir, serialize_snapshot_data_file,
            verify_and_unarchive_snapshots, ArchiveFormat, BankSnapshotInfo, SnapshotError,
            SnapshotVersion, StorageAndNextAccountsFileId, UnarchivedSnapshots,
            VerifyEpochStakesError, VerifySlotDeltasError,
        },
        status_cache,
    },
    agave_feature_set as feature_set,
    bincode::{config::Options, serialize_into},
    log::*,
    solana_accounts_db::{
        accounts_db::{
            AccountStorageEntry, AccountsDbConfig, AtomicAccountsFileId, CalcAccountsHashDataSource,
        },
        accounts_hash::MerkleOrLatticeAccountsHash,
        accounts_update_notifier_interface::AccountsUpdateNotifier,
        utils::remove_dir_contents,
    },
    solana_builtins::prototype::BuiltinPrototype,
    solana_clock::{Epoch, Slot},
    solana_genesis_config::GenesisConfig,
    solana_measure::{measure::Measure, measure_time},
    solana_pubkey::Pubkey,
    solana_slot_history::{Check, SlotHistory},
    std::{
        collections::{HashMap, HashSet},
        ops::RangeInclusive,
        path::{Path, PathBuf},
        sync::{atomic::AtomicBool, Arc},
    },
};

pub fn serialize_status_cache(
    slot_deltas: &[BankSlotDelta],
    status_cache_path: &Path,
) -> snapshot_utils::Result<u64> {
    serialize_snapshot_data_file(status_cache_path, |stream| {
        serialize_into(stream, slot_deltas)?;
        Ok(())
    })
}

#[derive(Debug)]
pub struct BankFromArchivesTimings {
    pub untar_full_snapshot_archive_us: u64,
    pub untar_incremental_snapshot_archive_us: u64,
    pub rebuild_bank_us: u64,
    pub verify_bank_us: u64,
}

#[derive(Debug)]
pub struct BankFromDirTimings {
    pub rebuild_storages_us: u64,
    pub rebuild_bank_us: u64,
}

/// Parses out bank specific information from a snapshot archive including the leader schedule.
/// epoch schedule, etc.
#[cfg(feature = "dev-context-only-utils")]
pub fn bank_fields_from_snapshot_archives(
    full_snapshot_archives_dir: impl AsRef<Path>,
    incremental_snapshot_archives_dir: impl AsRef<Path>,
    storage_access: StorageAccess,
) -> snapshot_utils::Result<BankFieldsToDeserialize> {
    let full_snapshot_archive_info =
        get_highest_full_snapshot_archive_info(&full_snapshot_archives_dir).ok_or_else(|| {
            SnapshotError::NoSnapshotArchives(full_snapshot_archives_dir.as_ref().to_path_buf())
        })?;

    let incremental_snapshot_archive_info = get_highest_incremental_snapshot_archive_info(
        &incremental_snapshot_archives_dir,
        full_snapshot_archive_info.slot(),
    );

    let temp_unpack_dir = TempDir::new()?;
    let temp_accounts_dir = TempDir::new()?;

    let account_paths = vec![temp_accounts_dir.path().to_path_buf()];

    let (
        UnarchivedSnapshots {
            full_unpacked_snapshots_dir_and_version,
            incremental_unpacked_snapshots_dir_and_version,
            ..
        },
        _guard,
    ) = verify_and_unarchive_snapshots(
        &temp_unpack_dir,
        &full_snapshot_archive_info,
        incremental_snapshot_archive_info.as_ref(),
        &account_paths,
        storage_access,
    )?;

    bank_fields_from_snapshots(
        &full_unpacked_snapshots_dir_and_version,
        incremental_unpacked_snapshots_dir_and_version.as_ref(),
    )
}

#[cfg(feature = "dev-context-only-utils")]
fn bank_fields_from_snapshots(
    full_snapshot_unpacked_snapshots_dir_and_version: &UnpackedSnapshotsDirAndVersion,
    incremental_snapshot_unpacked_snapshots_dir_and_version: Option<
        &UnpackedSnapshotsDirAndVersion,
    >,
) -> snapshot_utils::Result<BankFieldsToDeserialize> {
    let (snapshot_version, snapshot_root_paths) = snapshot_version_and_root_paths(
        full_snapshot_unpacked_snapshots_dir_and_version,
        incremental_snapshot_unpacked_snapshots_dir_and_version,
    )?;

    info!(
        "Loading bank from full snapshot {} and incremental snapshot {:?}",
        snapshot_root_paths.full_snapshot_root_file_path.display(),
        snapshot_root_paths.incremental_snapshot_root_file_path,
    );

    deserialize_snapshot_data_files(&snapshot_root_paths, |snapshot_streams| {
        Ok(match snapshot_version {
            SnapshotVersion::V1_2_0 => fields_from_streams(snapshot_streams)
                .map(|(bank_fields, _accountsdb_fields)| bank_fields.collapse_into()),
        }?)
    })
}

/// Rebuild bank from snapshot archives.  Handles either just a full snapshot, or both a full
/// snapshot and an incremental snapshot.
#[allow(clippy::too_many_arguments)]
pub fn bank_from_snapshot_archives(
    account_paths: &[PathBuf],
    bank_snapshots_dir: impl AsRef<Path>,
    full_snapshot_archive_info: &FullSnapshotArchiveInfo,
    incremental_snapshot_archive_info: Option<&IncrementalSnapshotArchiveInfo>,
    genesis_config: &GenesisConfig,
    runtime_config: &RuntimeConfig,
    debug_keys: Option<Arc<HashSet<Pubkey>>>,
    additional_builtins: Option<&[BuiltinPrototype]>,
    limit_load_slot_count_from_snapshot: Option<usize>,
    accounts_db_skip_shrink: bool,
    accounts_db_force_initial_clean: bool,
    verify_index: bool,
    accounts_db_config: Option<AccountsDbConfig>,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> snapshot_utils::Result<(Bank, BankFromArchivesTimings)> {
    info!(
        "Loading bank from full snapshot archive: {}, and incremental snapshot archive: {:?}",
        full_snapshot_archive_info.path().display(),
        incremental_snapshot_archive_info
            .as_ref()
            .map(
                |incremental_snapshot_archive_info| incremental_snapshot_archive_info
                    .path()
                    .display()
            )
    );

    let (
        UnarchivedSnapshots {
            full_storage: mut storage,
            incremental_storage,
            bank_fields,
            accounts_db_fields,
            full_unpacked_snapshots_dir_and_version,
            incremental_unpacked_snapshots_dir_and_version,
            full_measure_untar,
            incremental_measure_untar,
            next_append_vec_id,
            ..
        },
        _guard,
    ) = verify_and_unarchive_snapshots(
        bank_snapshots_dir,
        full_snapshot_archive_info,
        incremental_snapshot_archive_info,
        account_paths,
        accounts_db_config
            .as_ref()
            .map(|config| config.storage_access)
            .unwrap_or_default(),
    )?;

    if let Some(incremental_storage) = incremental_storage {
        storage.extend(incremental_storage);
    }

    let storage_and_next_append_vec_id = StorageAndNextAccountsFileId {
        storage,
        next_append_vec_id,
    };

    let mut measure_rebuild = Measure::start("rebuild bank from snapshots");
    let (bank, info) = reconstruct_bank_from_fields(
        bank_fields,
        accounts_db_fields,
        genesis_config,
        runtime_config,
        account_paths,
        storage_and_next_append_vec_id,
        debug_keys,
        additional_builtins,
        limit_load_slot_count_from_snapshot,
        verify_index,
        accounts_db_config,
        accounts_update_notifier,
        exit,
    )?;
    measure_rebuild.stop();
    info!("{}", measure_rebuild);

    verify_epoch_stakes(&bank)?;

    // The status cache is rebuilt from the latest snapshot.  So, if there's an incremental
    // snapshot, use that.  Otherwise use the full snapshot.
    let status_cache_path = incremental_unpacked_snapshots_dir_and_version
        .as_ref()
        .map_or_else(
            || {
                full_unpacked_snapshots_dir_and_version
                    .unpacked_snapshots_dir
                    .as_path()
            },
            |unarchived_incremental_snapshot| {
                unarchived_incremental_snapshot
                    .unpacked_snapshots_dir
                    .as_path()
            },
        )
        .join(snapshot_utils::SNAPSHOT_STATUS_CACHE_FILENAME);
    let slot_deltas = deserialize_status_cache(&status_cache_path)?;

    verify_slot_deltas(slot_deltas.as_slice(), &bank)?;

    bank.status_cache.write().unwrap().append(&slot_deltas);

    let snapshot_archive_info = incremental_snapshot_archive_info.map_or_else(
        || full_snapshot_archive_info.snapshot_archive_info(),
        |incremental_snapshot_archive_info| {
            incremental_snapshot_archive_info.snapshot_archive_info()
        },
    );
    verify_bank_against_expected_slot_hash(
        &bank,
        snapshot_archive_info.slot,
        snapshot_archive_info.hash,
    )?;

    let mut measure_verify = Measure::start("verify");
    if !bank.verify_snapshot_bank(
        accounts_db_skip_shrink || !full_snapshot_archive_info.is_remote(),
        accounts_db_force_initial_clean,
        full_snapshot_archive_info.slot(),
        info.duplicates_lt_hash,
    ) && limit_load_slot_count_from_snapshot.is_none()
    {
        panic!("Snapshot bank for slot {} failed to verify", bank.slot());
    }
    measure_verify.stop();

    let timings = BankFromArchivesTimings {
        untar_full_snapshot_archive_us: full_measure_untar.as_us(),
        untar_incremental_snapshot_archive_us: incremental_measure_untar
            .map_or(0, |incremental_measure_untar| {
                incremental_measure_untar.as_us()
            }),
        rebuild_bank_us: measure_rebuild.as_us(),
        verify_bank_us: measure_verify.as_us(),
    };
    datapoint_info!(
        "bank_from_snapshot_archives",
        (
            "untar_full_snapshot_archive_us",
            timings.untar_full_snapshot_archive_us,
            i64
        ),
        (
            "untar_incremental_snapshot_archive_us",
            timings.untar_incremental_snapshot_archive_us,
            i64
        ),
        ("rebuild_bank_us", timings.rebuild_bank_us, i64),
        ("verify_bank_us", timings.verify_bank_us, i64),
    );
    Ok((bank, timings))
}

/// Rebuild bank from snapshot archives
///
/// This function searches `full_snapshot_archives_dir` and `incremental_snapshot_archives_dir` for
/// the highest full snapshot and highest corresponding incremental snapshot, then rebuilds the bank.
#[allow(clippy::too_many_arguments)]
pub fn bank_from_latest_snapshot_archives(
    bank_snapshots_dir: impl AsRef<Path>,
    full_snapshot_archives_dir: impl AsRef<Path>,
    incremental_snapshot_archives_dir: impl AsRef<Path>,
    account_paths: &[PathBuf],
    genesis_config: &GenesisConfig,
    runtime_config: &RuntimeConfig,
    debug_keys: Option<Arc<HashSet<Pubkey>>>,
    additional_builtins: Option<&[BuiltinPrototype]>,
    limit_load_slot_count_from_snapshot: Option<usize>,
    accounts_db_skip_shrink: bool,
    accounts_db_force_initial_clean: bool,
    verify_index: bool,
    accounts_db_config: Option<AccountsDbConfig>,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> snapshot_utils::Result<(
    Bank,
    FullSnapshotArchiveInfo,
    Option<IncrementalSnapshotArchiveInfo>,
)> {
    let full_snapshot_archive_info =
        get_highest_full_snapshot_archive_info(&full_snapshot_archives_dir).ok_or_else(|| {
            SnapshotError::NoSnapshotArchives(full_snapshot_archives_dir.as_ref().to_path_buf())
        })?;

    let incremental_snapshot_archive_info = get_highest_incremental_snapshot_archive_info(
        &incremental_snapshot_archives_dir,
        full_snapshot_archive_info.slot(),
    );

    let (bank, _) = bank_from_snapshot_archives(
        account_paths,
        bank_snapshots_dir.as_ref(),
        &full_snapshot_archive_info,
        incremental_snapshot_archive_info.as_ref(),
        genesis_config,
        runtime_config,
        debug_keys,
        additional_builtins,
        limit_load_slot_count_from_snapshot,
        accounts_db_skip_shrink,
        accounts_db_force_initial_clean,
        verify_index,
        accounts_db_config,
        accounts_update_notifier,
        exit,
    )?;

    Ok((
        bank,
        full_snapshot_archive_info,
        incremental_snapshot_archive_info,
    ))
}

/// Build bank from a snapshot (a snapshot directory, not a snapshot archive)
#[allow(clippy::too_many_arguments)]
pub fn bank_from_snapshot_dir(
    account_paths: &[PathBuf],
    bank_snapshot: &BankSnapshotInfo,
    genesis_config: &GenesisConfig,
    runtime_config: &RuntimeConfig,
    debug_keys: Option<Arc<HashSet<Pubkey>>>,
    additional_builtins: Option<&[BuiltinPrototype]>,
    limit_load_slot_count_from_snapshot: Option<usize>,
    verify_index: bool,
    accounts_db_config: Option<AccountsDbConfig>,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> snapshot_utils::Result<(Bank, BankFromDirTimings)> {
    info!(
        "Loading bank from snapshot dir: {}",
        bank_snapshot.snapshot_dir.display()
    );

    // Clear the contents of the account paths run directories.  When constructing the bank, the appendvec
    // files will be extracted from the snapshot hardlink directories into these run/ directories.
    for path in account_paths {
        remove_dir_contents(path);
    }

    let next_append_vec_id = Arc::new(AtomicAccountsFileId::new(0));
    let storage_access = accounts_db_config
        .as_ref()
        .map(|config| config.storage_access)
        .unwrap_or_default();

    let ((storage, bank_fields, accounts_db_fields), measure_rebuild_storages) = measure_time!(
        rebuild_storages_from_snapshot_dir(
            bank_snapshot,
            account_paths,
            next_append_vec_id.clone(),
            storage_access,
        )?,
        "rebuild storages from snapshot dir"
    );
    info!("{}", measure_rebuild_storages);

    let next_append_vec_id =
        Arc::try_unwrap(next_append_vec_id).expect("this is the only strong reference");
    let storage_and_next_append_vec_id = StorageAndNextAccountsFileId {
        storage,
        next_append_vec_id,
    };
    let snapshot_bank_fields = SnapshotBankFields::new(bank_fields, None);
    let snapshot_accounts_db_fields = SnapshotAccountsDbFields::new(accounts_db_fields, None);
    let ((bank, _info), measure_rebuild_bank) = measure_time!(
        reconstruct_bank_from_fields(
            snapshot_bank_fields,
            snapshot_accounts_db_fields,
            genesis_config,
            runtime_config,
            account_paths,
            storage_and_next_append_vec_id,
            debug_keys,
            additional_builtins,
            limit_load_slot_count_from_snapshot,
            verify_index,
            accounts_db_config,
            accounts_update_notifier,
            exit,
        )?,
        "rebuild bank from snapshot"
    );
    info!("{}", measure_rebuild_bank);

    verify_epoch_stakes(&bank)?;

    let status_cache_path = bank_snapshot
        .snapshot_dir
        .join(snapshot_utils::SNAPSHOT_STATUS_CACHE_FILENAME);
    let slot_deltas = deserialize_status_cache(&status_cache_path)?;

    verify_slot_deltas(slot_deltas.as_slice(), &bank)?;

    bank.status_cache.write().unwrap().append(&slot_deltas);

    // We trust our local state, so skip the startup accounts verification.
    bank.set_initial_accounts_hash_verification_completed();

    let timings = BankFromDirTimings {
        rebuild_storages_us: measure_rebuild_storages.as_us(),
        rebuild_bank_us: measure_rebuild_bank.as_us(),
    };
    datapoint_info!(
        "bank_from_snapshot_dir",
        ("rebuild_storages_us", timings.rebuild_storages_us, i64),
        ("rebuild_bank_us", timings.rebuild_bank_us, i64),
    );
    Ok((bank, timings))
}

/// follow the prototype of fn bank_from_latest_snapshot_archives, implement the from_dir case
#[allow(clippy::too_many_arguments)]
pub fn bank_from_latest_snapshot_dir(
    bank_snapshots_dir: impl AsRef<Path>,
    genesis_config: &GenesisConfig,
    runtime_config: &RuntimeConfig,
    account_paths: &[PathBuf],
    debug_keys: Option<Arc<HashSet<Pubkey>>>,
    additional_builtins: Option<&[BuiltinPrototype]>,
    limit_load_slot_count_from_snapshot: Option<usize>,
    verify_index: bool,
    accounts_db_config: Option<AccountsDbConfig>,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> snapshot_utils::Result<Bank> {
    let bank_snapshot = get_highest_bank_snapshot_post(&bank_snapshots_dir).ok_or_else(|| {
        SnapshotError::NoSnapshotSlotDir(bank_snapshots_dir.as_ref().to_path_buf())
    })?;
    let (bank, _) = bank_from_snapshot_dir(
        account_paths,
        &bank_snapshot,
        genesis_config,
        runtime_config,
        debug_keys,
        additional_builtins,
        limit_load_slot_count_from_snapshot,
        verify_index,
        accounts_db_config,
        accounts_update_notifier,
        exit,
    )?;

    Ok(bank)
}

/// Verifies the snapshot's slot and hash matches the bank's
fn verify_bank_against_expected_slot_hash(
    bank: &Bank,
    snapshot_slot: Slot,
    snapshot_hash: SnapshotHash,
) -> snapshot_utils::Result<()> {
    let bank_slot = bank.slot();
    if bank_slot != snapshot_slot {
        return Err(SnapshotError::MismatchedSlot(bank_slot, snapshot_slot));
    }

    let bank_hash = bank.get_snapshot_hash();
    if bank_hash == snapshot_hash {
        return Ok(());
    }

    // If the slots match but the hashes don't, there may be a mismatch between snapshot
    // generation and snapshot load w.r.t. the snapshots_lt_hash cli args.

    if bank
        .feature_set
        .is_active(&feature_set::snapshots_lt_hash::id())
    {
        // ...but, if the snapshots_lt_hash *feature* is active, then mismatches are errors
        return Err(SnapshotError::MismatchedHash(bank_hash, snapshot_hash));
    }

    // once here, we know the snapshots_lt_hash *feature* is *disabled*, so check the other kind of
    // snapshot hash in case we match that one
    let other_bank_hash = if bank.is_snapshots_lt_hash_enabled() {
        // If our snapshots_lt_hash cli arg is ON, maybe the node that generated this snapshot had
        // the cli arg OFF?  Try getting the merkle-based snapshot hash to compare.
        bank.get_merkle_snapshot_hash()
    } else {
        // If our snapshots_lt_hash cli arg is OFF, maybe the node that generated this snapshot had
        // the cli arg ON?  Try getting the lattice-based snapshot hash to compare.
        bank.get_lattice_snapshot_hash()
    };

    if other_bank_hash == snapshot_hash {
        Ok(())
    } else {
        // yes, use the *original* bank_hash here, not other_bank_hash
        Err(SnapshotError::MismatchedHash(bank_hash, snapshot_hash))
    }
}

/// Returns the validated version and root paths for the given snapshots.
#[cfg(feature = "dev-context-only-utils")]
fn snapshot_version_and_root_paths(
    full_snapshot_unpacked_snapshots_dir_and_version: &UnpackedSnapshotsDirAndVersion,
    incremental_snapshot_unpacked_snapshots_dir_and_version: Option<
        &UnpackedSnapshotsDirAndVersion,
    >,
) -> snapshot_utils::Result<(SnapshotVersion, SnapshotRootPaths)> {
    let (full_snapshot_version, full_snapshot_root_paths) =
        verify_unpacked_snapshots_dir_and_version(
            full_snapshot_unpacked_snapshots_dir_and_version,
        )?;
    let (incremental_snapshot_version, incremental_snapshot_root_paths) =
        if let Some(snapshot_unpacked_snapshots_dir_and_version) =
            incremental_snapshot_unpacked_snapshots_dir_and_version
        {
            Some(verify_unpacked_snapshots_dir_and_version(
                snapshot_unpacked_snapshots_dir_and_version,
            )?)
        } else {
            None
        }
        .unzip();

    let snapshot_version = incremental_snapshot_version.unwrap_or(full_snapshot_version);
    let snapshot_root_paths = SnapshotRootPaths {
        full_snapshot_root_file_path: full_snapshot_root_paths.snapshot_path(),
        incremental_snapshot_root_file_path: incremental_snapshot_root_paths
            .map(|root_paths| root_paths.snapshot_path()),
    };

    Ok((snapshot_version, snapshot_root_paths))
}

fn deserialize_status_cache(
    status_cache_path: &Path,
) -> snapshot_utils::Result<Vec<BankSlotDelta>> {
    deserialize_snapshot_data_file(status_cache_path, |stream| {
        info!(
            "Rebuilding status cache from {}",
            status_cache_path.display()
        );
        let slot_delta: Vec<BankSlotDelta> = bincode::options()
            .with_limit(snapshot_utils::MAX_SNAPSHOT_DATA_FILE_SIZE)
            .with_fixint_encoding()
            .allow_trailing_bytes()
            .deserialize_from(stream)?;
        Ok(slot_delta)
    })
}

/// Verify that the snapshot's slot deltas are not corrupt/invalid
fn verify_slot_deltas(
    slot_deltas: &[BankSlotDelta],
    bank: &Bank,
) -> std::result::Result<(), VerifySlotDeltasError> {
    let info = verify_slot_deltas_structural(slot_deltas, bank.slot())?;
    verify_slot_deltas_with_history(&info.slots, &bank.get_slot_history(), bank.slot())
}

/// Verify that the snapshot's slot deltas are not corrupt/invalid
/// These checks are simple/structural
fn verify_slot_deltas_structural(
    slot_deltas: &[BankSlotDelta],
    bank_slot: Slot,
) -> std::result::Result<VerifySlotDeltasStructuralInfo, VerifySlotDeltasError> {
    // there should not be more entries than that status cache's max
    let num_entries = slot_deltas.len();
    if num_entries > status_cache::MAX_CACHE_ENTRIES {
        return Err(VerifySlotDeltasError::TooManyEntries(
            num_entries,
            status_cache::MAX_CACHE_ENTRIES,
        ));
    }

    let mut slots_seen_so_far = HashSet::new();
    for &(slot, is_root, ..) in slot_deltas {
        // all entries should be roots
        if !is_root {
            return Err(VerifySlotDeltasError::SlotIsNotRoot(slot));
        }

        // all entries should be for slots less than or equal to the bank's slot
        if slot > bank_slot {
            return Err(VerifySlotDeltasError::SlotGreaterThanMaxRoot(
                slot, bank_slot,
            ));
        }

        // there should only be one entry per slot
        let is_duplicate = !slots_seen_so_far.insert(slot);
        if is_duplicate {
            return Err(VerifySlotDeltasError::SlotHasMultipleEntries(slot));
        }
    }

    // detect serious logic error for future careless changes. :)
    assert_eq!(slots_seen_so_far.len(), slot_deltas.len());

    Ok(VerifySlotDeltasStructuralInfo {
        slots: slots_seen_so_far,
    })
}

/// Computed information from `verify_slot_deltas_structural()`, that may be reused/useful later.
#[derive(Debug, PartialEq, Eq)]
struct VerifySlotDeltasStructuralInfo {
    /// All the slots in the slot deltas
    slots: HashSet<Slot>,
}

/// Verify that the snapshot's slot deltas are not corrupt/invalid
/// These checks use the slot history for verification
fn verify_slot_deltas_with_history(
    slots_from_slot_deltas: &HashSet<Slot>,
    slot_history: &SlotHistory,
    bank_slot: Slot,
) -> std::result::Result<(), VerifySlotDeltasError> {
    // ensure the slot history is valid (as much as possible), since we're using it to verify the
    // slot deltas
    if slot_history.newest() != bank_slot {
        return Err(VerifySlotDeltasError::BadSlotHistory);
    }

    // all slots in the slot deltas should be in the bank's slot history
    let slot_missing_from_history = slots_from_slot_deltas
        .iter()
        .find(|slot| slot_history.check(**slot) != Check::Found);
    if let Some(slot) = slot_missing_from_history {
        return Err(VerifySlotDeltasError::SlotNotFoundInHistory(*slot));
    }

    // all slots in the history should be in the slot deltas (up to MAX_CACHE_ENTRIES)
    // this ensures nothing was removed from the status cache
    //
    // go through the slot history and make sure there's an entry for each slot
    // note: it's important to go highest-to-lowest since the status cache removes
    // older entries first
    // note: we already checked above that `bank_slot == slot_history.newest()`
    let slot_missing_from_deltas = (slot_history.oldest()..=slot_history.newest())
        .rev()
        .filter(|slot| slot_history.check(*slot) == Check::Found)
        .take(status_cache::MAX_CACHE_ENTRIES)
        .find(|slot| !slots_from_slot_deltas.contains(slot));
    if let Some(slot) = slot_missing_from_deltas {
        return Err(VerifySlotDeltasError::SlotNotFoundInDeltas(slot));
    }

    Ok(())
}

/// Verifies the bank's epoch stakes are valid after rebuilding from a snapshot
fn verify_epoch_stakes(bank: &Bank) -> std::result::Result<(), VerifyEpochStakesError> {
    // Stakes are required for epochs from the current epoch up-to-and-including the
    // leader schedule epoch.  In practice this will only be two epochs: the current and the next.
    // Using a range mirrors how Bank::new_with_paths() seeds the initial epoch stakes.
    let current_epoch = bank.epoch();
    let leader_schedule_epoch = bank.get_leader_schedule_epoch(bank.slot());
    let required_epochs = current_epoch..=leader_schedule_epoch;
    _verify_epoch_stakes(bank.epoch_stakes_map(), required_epochs)
}

/// Verifies the bank's epoch stakes are valid after rebuilding from a snapshot
///
/// This version of the function exists to facilitate testing.
/// Normal callers should use `verify_epoch_stakes()`.
fn _verify_epoch_stakes(
    epoch_stakes_map: &HashMap<Epoch, VersionedEpochStakes>,
    required_epochs: RangeInclusive<Epoch>,
) -> std::result::Result<(), VerifyEpochStakesError> {
    // Ensure epoch stakes from the snapshot does not contain entries for invalid epochs.
    // Since epoch stakes are computed for the leader schedule epoch (usually `epoch + 1`),
    // the snapshot's epoch stakes therefor can have entries for epochs at-or-below the
    // leader schedule epoch.
    let max_epoch = *required_epochs.end();
    if let Some(invalid_epoch) = epoch_stakes_map.keys().find(|epoch| **epoch > max_epoch) {
        return Err(VerifyEpochStakesError::EpochGreaterThanMax(
            *invalid_epoch,
            max_epoch,
        ));
    }

    // Ensure epoch stakes contains stakes for all the required epochs
    if let Some(missing_epoch) = required_epochs
        .clone()
        .find(|epoch| !epoch_stakes_map.contains_key(epoch))
    {
        return Err(VerifyEpochStakesError::StakesNotFound(
            missing_epoch,
            required_epochs,
        ));
    }

    Ok(())
}

/// Get the snapshot storages for this bank
pub fn get_snapshot_storages(bank: &Bank) -> Vec<Arc<AccountStorageEntry>> {
    let mut measure_snapshot_storages = Measure::start("snapshot-storages");
    let snapshot_storages = bank.get_snapshot_storages(None);
    measure_snapshot_storages.stop();
    datapoint_info!(
        "get_snapshot_storages",
        (
            "snapshot-storages-time-ms",
            measure_snapshot_storages.as_ms(),
            i64
        ),
    );

    snapshot_storages
}

/// Convenience function to create a full snapshot archive out of any Bank, regardless of state.
/// The Bank will be frozen during the process.
/// This is only called from ledger-tool or tests. Warping is a special case as well.
///
/// Requires:
///     - `bank` is complete
pub fn bank_to_full_snapshot_archive(
    bank_snapshots_dir: impl AsRef<Path>,
    bank: &Bank,
    snapshot_version: Option<SnapshotVersion>,
    full_snapshot_archives_dir: impl AsRef<Path>,
    incremental_snapshot_archives_dir: impl AsRef<Path>,
    archive_format: ArchiveFormat,
) -> snapshot_utils::Result<FullSnapshotArchiveInfo> {
    let snapshot_version = snapshot_version.unwrap_or_default();
    let temp_bank_snapshots_dir = tempfile::tempdir_in(bank_snapshots_dir)?;
    bank_to_full_snapshot_archive_with(
        &temp_bank_snapshots_dir,
        bank,
        snapshot_version,
        full_snapshot_archives_dir,
        incremental_snapshot_archives_dir,
        archive_format,
        false, // we do not intend to fastboot, so skip flushing and hard linking the storages
    )
}

/// See bank_to_full_snapshot_archive() for documentation
///
/// This fn does *not* create a tmpdir inside `bank_snapshots_dir`
/// (which is needed by a test)
fn bank_to_full_snapshot_archive_with(
    bank_snapshots_dir: impl AsRef<Path>,
    bank: &Bank,
    snapshot_version: SnapshotVersion,
    full_snapshot_archives_dir: impl AsRef<Path>,
    incremental_snapshot_archives_dir: impl AsRef<Path>,
    archive_format: ArchiveFormat,
    should_flush_and_hard_link_storages: bool,
) -> snapshot_utils::Result<FullSnapshotArchiveInfo> {
    assert!(bank.is_complete());
    // set accounts-db's latest full snapshot slot here to ensure zero lamport
    // accounts are handled properly.
    bank.rc
        .accounts
        .accounts_db
        .set_latest_full_snapshot_slot(bank.slot());
    bank.squash(); // Bank may not be a root
    bank.rehash(); // Bank may have been manually modified by the caller
    bank.force_flush_accounts_cache();
    bank.clean_accounts();

    let merkle_or_lattice_accounts_hash = if bank.is_snapshots_lt_hash_enabled() {
        MerkleOrLatticeAccountsHash::Lattice
    } else {
        let calculated_accounts_hash =
            bank.update_accounts_hash(CalcAccountsHashDataSource::Storages, false);
        let accounts_hash = bank
            .get_accounts_hash()
            .expect("accounts hash is required for snapshot");
        assert_eq!(accounts_hash, calculated_accounts_hash);
        MerkleOrLatticeAccountsHash::Merkle(accounts_hash.into())
    };

    let snapshot_storages = bank.get_snapshot_storages(None);
    let status_cache_slot_deltas = bank.status_cache.read().unwrap().root_slot_deltas();
    let accounts_package = AccountsPackage::new_for_snapshot(
        AccountsPackageKind::Snapshot(SnapshotKind::FullSnapshot),
        bank,
        snapshot_storages,
        status_cache_slot_deltas,
    );
    let snapshot_package =
        SnapshotPackage::new(accounts_package, merkle_or_lattice_accounts_hash, None);

    let snapshot_config = SnapshotConfig {
        full_snapshot_archives_dir: full_snapshot_archives_dir.as_ref().to_path_buf(),
        incremental_snapshot_archives_dir: incremental_snapshot_archives_dir.as_ref().to_path_buf(),
        bank_snapshots_dir: bank_snapshots_dir.as_ref().to_path_buf(),
        archive_format,
        snapshot_version,
        ..Default::default()
    };
    let snapshot_archive_info = snapshot_utils::serialize_and_archive_snapshot_package(
        snapshot_package,
        &snapshot_config,
        should_flush_and_hard_link_storages,
    )?;

    Ok(FullSnapshotArchiveInfo::new(snapshot_archive_info))
}

/// Convenience function to create an incremental snapshot archive out of any Bank, regardless of
/// state.  The Bank will be frozen during the process.
/// This is only called from ledger-tool or tests. Warping is a special case as well.
///
/// Requires:
///     - `bank` is complete
///     - `bank`'s slot is greater than `full_snapshot_slot`
pub fn bank_to_incremental_snapshot_archive(
    bank_snapshots_dir: impl AsRef<Path>,
    bank: &Bank,
    full_snapshot_slot: Slot,
    snapshot_version: Option<SnapshotVersion>,
    full_snapshot_archives_dir: impl AsRef<Path>,
    incremental_snapshot_archives_dir: impl AsRef<Path>,
    archive_format: ArchiveFormat,
) -> snapshot_utils::Result<IncrementalSnapshotArchiveInfo> {
    let snapshot_version = snapshot_version.unwrap_or_default();

    assert!(bank.is_complete());
    assert!(bank.slot() > full_snapshot_slot);
    // set accounts-db's latest full snapshot slot here to ensure zero lamport
    // accounts are handled properly.
    bank.rc
        .accounts
        .accounts_db
        .set_latest_full_snapshot_slot(full_snapshot_slot);
    bank.squash(); // Bank may not be a root
    bank.rehash(); // Bank may have been manually modified by the caller
    bank.force_flush_accounts_cache();
    bank.clean_accounts();

    let (merkle_or_lattice_accounts_hash, bank_incremental_snapshot_persistence) =
        if bank.is_snapshots_lt_hash_enabled() {
            (MerkleOrLatticeAccountsHash::Lattice, None)
        } else {
            let calculated_incremental_accounts_hash =
                bank.update_incremental_accounts_hash(full_snapshot_slot);
            let (full_accounts_hash, full_capitalization) = bank
                .rc
                .accounts
                .accounts_db
                .get_accounts_hash(full_snapshot_slot)
                .expect("base accounts hash is required for incremental snapshot");
            let (incremental_accounts_hash, incremental_capitalization) = bank
                .rc
                .accounts
                .accounts_db
                .get_incremental_accounts_hash(bank.slot())
                .expect("incremental accounts hash is required for incremental snapshot");
            assert_eq!(
                incremental_accounts_hash,
                calculated_incremental_accounts_hash,
            );
            let bank_incremental_snapshot_persistence = BankIncrementalSnapshotPersistence {
                full_slot: full_snapshot_slot,
                full_hash: full_accounts_hash.into(),
                full_capitalization,
                incremental_hash: incremental_accounts_hash.into(),
                incremental_capitalization,
            };
            (
                MerkleOrLatticeAccountsHash::Merkle(incremental_accounts_hash.into()),
                Some(bank_incremental_snapshot_persistence),
            )
        };

    let snapshot_storages = bank.get_snapshot_storages(Some(full_snapshot_slot));
    let status_cache_slot_deltas = bank.status_cache.read().unwrap().root_slot_deltas();
    let accounts_package = AccountsPackage::new_for_snapshot(
        AccountsPackageKind::Snapshot(SnapshotKind::IncrementalSnapshot(full_snapshot_slot)),
        bank,
        snapshot_storages,
        status_cache_slot_deltas,
    );
    let snapshot_package = SnapshotPackage::new(
        accounts_package,
        merkle_or_lattice_accounts_hash,
        bank_incremental_snapshot_persistence,
    );

    // Note: Since the snapshot_storages above are *only* the incremental storages,
    // this bank snapshot *cannot* be used by fastboot.
    // Putting the snapshot in a tempdir effectively enforces that.
    let temp_bank_snapshots_dir = tempfile::tempdir_in(bank_snapshots_dir)?;
    let snapshot_config = SnapshotConfig {
        full_snapshot_archives_dir: full_snapshot_archives_dir.as_ref().to_path_buf(),
        incremental_snapshot_archives_dir: incremental_snapshot_archives_dir.as_ref().to_path_buf(),
        bank_snapshots_dir: temp_bank_snapshots_dir.path().to_path_buf(),
        archive_format,
        snapshot_version,
        ..Default::default()
    };
    let snapshot_archive_info = snapshot_utils::serialize_and_archive_snapshot_package(
        snapshot_package,
        &snapshot_config,
        false, // we do not intend to fastboot, so skip flushing and hard linking the storages
    )?;

    Ok(IncrementalSnapshotArchiveInfo::new(
        full_snapshot_slot,
        snapshot_archive_info,
    ))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank::{tests::create_simple_test_bank, BankTestConfig},
            bank_forks::BankForks,
            genesis_utils,
            snapshot_config::SnapshotConfig,
            snapshot_utils::{
                clean_orphaned_account_snapshot_dirs, create_tmp_accounts_dir_for_tests,
                get_bank_snapshot_dir, get_bank_snapshots, get_bank_snapshots_post,
                get_bank_snapshots_pre, get_highest_bank_snapshot, get_highest_bank_snapshot_pre,
                get_highest_loadable_bank_snapshot, get_snapshot_file_name,
                purge_all_bank_snapshots, purge_bank_snapshot,
                purge_bank_snapshots_older_than_slot, purge_incomplete_bank_snapshots,
                purge_old_bank_snapshots, purge_old_bank_snapshots_at_startup,
                snapshot_storage_rebuilder::get_slot_and_append_vec_id, BankSnapshotKind,
                BANK_SNAPSHOT_PRE_FILENAME_EXTENSION, SNAPSHOT_FULL_SNAPSHOT_SLOT_FILENAME,
            },
            status_cache::Status,
        },
        agave_feature_set as feature_set,
        solana_accounts_db::{
            accounts_db::ACCOUNTS_DB_CONFIG_FOR_TESTING,
            accounts_hash::{CalcAccountsHashConfig, HashStats},
            sorted_storages::SortedStorages,
        },
        solana_genesis_config::create_genesis_config,
        solana_keypair::Keypair,
        solana_native_token::{sol_to_lamports, LAMPORTS_PER_SOL},
        solana_signer::Signer,
        solana_system_transaction as system_transaction,
        solana_transaction::sanitized::SanitizedTransaction,
        std::{
            fs,
            sync::{atomic::Ordering, Arc, RwLock},
        },
        test_case::test_case,
    };

    fn create_snapshot_dirs_for_tests(
        genesis_config: &GenesisConfig,
        bank_snapshots_dir: impl AsRef<Path>,
        num_total: usize,
        num_posts: usize,
        should_flush_and_hard_link_storages: bool,
    ) -> Bank {
        assert!(num_posts <= num_total);

        // We don't need the snapshot archives to live after this function returns,
        // so let TempDir::drop() handle cleanup.
        let snapshot_archives_dir = TempDir::new().unwrap();

        let mut bank = Arc::new(Bank::new_for_tests(genesis_config));
        for i in 0..num_total {
            let slot = bank.slot() + 1;
            bank = Arc::new(Bank::new_from_parent(bank, &Pubkey::new_unique(), slot));
            bank.fill_bank_with_ticks_for_tests();

            bank_to_full_snapshot_archive_with(
                &bank_snapshots_dir,
                &bank,
                SnapshotVersion::default(),
                &snapshot_archives_dir,
                &snapshot_archives_dir,
                SnapshotConfig::default().archive_format,
                should_flush_and_hard_link_storages,
            )
            .unwrap();

            // As a hack, to make a PRE bank snapshot, just rename the POST one.
            if i >= num_posts {
                let bank_snapshot_dir = get_bank_snapshot_dir(&bank_snapshots_dir, slot);
                let post = bank_snapshot_dir.join(get_snapshot_file_name(slot));
                let pre = post.with_extension(BANK_SNAPSHOT_PRE_FILENAME_EXTENSION);
                fs::rename(post, pre).unwrap();
            }
        }

        Arc::into_inner(bank).unwrap()
    }

    fn new_bank_from_parent_with_bank_forks(
        bank_forks: &RwLock<BankForks>,
        parent: Arc<Bank>,
        collector_id: &Pubkey,
        slot: Slot,
    ) -> Arc<Bank> {
        let bank = Bank::new_from_parent(parent, collector_id, slot);
        bank_forks
            .write()
            .unwrap()
            .insert(bank)
            .clone_without_scheduler()
    }

    /// Test roundtrip of bank to a full snapshot, then back again.  This test creates the simplest
    /// bank possible, so the contents of the snapshot archive will be quite minimal.
    #[test]
    fn test_roundtrip_bank_to_and_from_full_snapshot_simple() {
        let genesis_config = GenesisConfig::default();
        let original_bank = Bank::new_for_tests(&genesis_config);

        original_bank.fill_bank_with_ticks_for_tests();

        let (_tmp_dir, accounts_dir) = create_tmp_accounts_dir_for_tests();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let snapshot_archive_format = SnapshotConfig::default().archive_format;

        let snapshot_archive_info = bank_to_full_snapshot_archive(
            &bank_snapshots_dir,
            &original_bank,
            None,
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            snapshot_archive_format,
        )
        .unwrap();

        let (roundtrip_bank, _) = bank_from_snapshot_archives(
            &[accounts_dir],
            bank_snapshots_dir.path(),
            &snapshot_archive_info,
            None,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            None,
            false,
            false,
            false,
            Some(ACCOUNTS_DB_CONFIG_FOR_TESTING),
            None,
            Arc::default(),
        )
        .unwrap();
        roundtrip_bank.wait_for_initial_accounts_hash_verification_completed_for_tests();
        assert_eq!(original_bank, roundtrip_bank);
    }

    /// This tests handling of obsolete accounts during a full snapshot with obsolete accounts
    /// marked in the accounts database. This test injects them directly
    #[test]
    fn test_roundtrip_bank_to_and_from_full_snapshot_with_obsolete_account() {
        let collector = Pubkey::new_unique();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();

        // Create a few accounts
        let (genesis_config, mint_keypair) = create_genesis_config(sol_to_lamports(1_000_000.));
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        bank0
            .transfer(sol_to_lamports(1.), &mint_keypair, &key1.pubkey())
            .unwrap();
        bank0
            .transfer(sol_to_lamports(2.), &mint_keypair, &key2.pubkey())
            .unwrap();
        bank0
            .transfer(sol_to_lamports(3.), &mint_keypair, &key3.pubkey())
            .unwrap();
        bank0.fill_bank_with_ticks_for_tests();

        // Force flush the bank to create the account storage entry
        bank0.squash();
        bank0.force_flush_accounts_cache();

        // Find the account storage entry for slot 0
        let target_slot = 0;
        let account_storage_entry = bank0
            .accounts()
            .accounts_db
            .storage
            .get_slot_storage_entry(target_slot)
            .unwrap();

        // Find all the accounts in slot 0
        let accounts = bank0
            .accounts()
            .accounts_db
            .get_unique_accounts_from_storage(&account_storage_entry);

        // Find the offset of pubkey `key1` in the accounts db slot0 and save the offset.
        let offset = accounts
            .stored_accounts
            .iter()
            .find(|account| key1.pubkey() == *account.pubkey())
            .map(|account| account.index_info.offset())
            .expect("Pubkey1 is present in Slot0");

        // Create a new slot, and invalidate the account for key1 in slot0
        let slot = 1;
        let bank1 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
        bank1
            .transfer(sol_to_lamports(1.), &key3, &key1.pubkey())
            .unwrap();

        bank1.fill_bank_with_ticks_for_tests();

        // Mark the entry for pubkey1 as obsolete in slot0
        account_storage_entry.mark_accounts_obsolete(vec![(offset, 0)].into_iter(), slot);

        let (_tmp_dir, accounts_dir) = create_tmp_accounts_dir_for_tests();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let snapshot_archive_format = SnapshotConfig::default().archive_format;

        let full_snapshot_archive_info = bank_to_full_snapshot_archive(
            bank_snapshots_dir.path(),
            &bank1,
            None,
            snapshot_archives_dir.path(),
            snapshot_archives_dir.path(),
            snapshot_archive_format,
        )
        .unwrap();

        let (roundtrip_bank, _) = bank_from_snapshot_archives(
            &[accounts_dir],
            bank_snapshots_dir.path(),
            &full_snapshot_archive_info,
            None,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            None,
            false,
            false,
            false,
            Some(ACCOUNTS_DB_CONFIG_FOR_TESTING),
            None,
            Arc::default(),
        )
        .unwrap();
        roundtrip_bank.wait_for_initial_accounts_hash_verification_completed_for_tests();
        assert_eq!(*bank1, roundtrip_bank);
    }

    /// Test roundtrip of bank to a full snapshot, then back again.  This test is more involved
    /// than the simple version above; creating multiple banks over multiple slots and doing
    /// multiple transfers.  So this full snapshot should contain more data.
    #[test]
    fn test_roundtrip_bank_to_and_from_snapshot_complex() {
        let collector = Pubkey::new_unique();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();
        let key5 = Keypair::new();

        let (genesis_config, mint_keypair) = create_genesis_config(sol_to_lamports(1_000_000.));
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        bank0
            .transfer(sol_to_lamports(1.), &mint_keypair, &key1.pubkey())
            .unwrap();
        bank0
            .transfer(sol_to_lamports(2.), &mint_keypair, &key2.pubkey())
            .unwrap();
        bank0
            .transfer(sol_to_lamports(3.), &mint_keypair, &key3.pubkey())
            .unwrap();
        bank0.fill_bank_with_ticks_for_tests();

        let slot = 1;
        let bank1 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
        bank1
            .transfer(sol_to_lamports(3.), &mint_keypair, &key3.pubkey())
            .unwrap();
        bank1
            .transfer(sol_to_lamports(4.), &mint_keypair, &key4.pubkey())
            .unwrap();
        bank1
            .transfer(sol_to_lamports(5.), &mint_keypair, &key5.pubkey())
            .unwrap();
        bank1.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank2 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank1, &collector, slot);
        bank2
            .transfer(sol_to_lamports(1.), &mint_keypair, &key1.pubkey())
            .unwrap();
        bank2.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank3 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank2, &collector, slot);
        bank3
            .transfer(sol_to_lamports(1.), &mint_keypair, &key1.pubkey())
            .unwrap();
        bank3.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank4 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank3, &collector, slot);
        bank4
            .transfer(sol_to_lamports(1.), &mint_keypair, &key1.pubkey())
            .unwrap();
        bank4.fill_bank_with_ticks_for_tests();

        let (_tmp_dir, accounts_dir) = create_tmp_accounts_dir_for_tests();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let snapshot_archive_format = SnapshotConfig::default().archive_format;

        let full_snapshot_archive_info = bank_to_full_snapshot_archive(
            bank_snapshots_dir.path(),
            &bank4,
            None,
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            snapshot_archive_format,
        )
        .unwrap();

        let (roundtrip_bank, _) = bank_from_snapshot_archives(
            &[accounts_dir],
            bank_snapshots_dir.path(),
            &full_snapshot_archive_info,
            None,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            None,
            false,
            false,
            false,
            Some(ACCOUNTS_DB_CONFIG_FOR_TESTING),
            None,
            Arc::default(),
        )
        .unwrap();
        roundtrip_bank.wait_for_initial_accounts_hash_verification_completed_for_tests();
        assert_eq!(*bank4, roundtrip_bank);
    }

    /// Test roundtrip of bank to snapshots, then back again, with incremental snapshots.  In this
    /// version, build up a few slots and take a full snapshot.  Continue on a few more slots and
    /// take an incremental snapshot.  Rebuild the bank from both the incremental snapshot and full
    /// snapshot.
    ///
    /// For the full snapshot, touch all the accounts, but only one for the incremental snapshot.
    /// This is intended to mimic the real behavior of transactions, where only a small number of
    /// accounts are modified often, which are captured by the incremental snapshot.  The majority
    /// of the accounts are not modified often, and are captured by the full snapshot.
    #[test]
    fn test_roundtrip_bank_to_and_from_incremental_snapshot() {
        let collector = Pubkey::new_unique();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();
        let key4 = Keypair::new();
        let key5 = Keypair::new();

        let (genesis_config, mint_keypair) = create_genesis_config(sol_to_lamports(1_000_000.));
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        bank0
            .transfer(sol_to_lamports(1.), &mint_keypair, &key1.pubkey())
            .unwrap();
        bank0
            .transfer(sol_to_lamports(2.), &mint_keypair, &key2.pubkey())
            .unwrap();
        bank0
            .transfer(sol_to_lamports(3.), &mint_keypair, &key3.pubkey())
            .unwrap();
        bank0.fill_bank_with_ticks_for_tests();

        let slot = 1;
        let bank1 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
        bank1
            .transfer(sol_to_lamports(3.), &mint_keypair, &key3.pubkey())
            .unwrap();
        bank1
            .transfer(sol_to_lamports(4.), &mint_keypair, &key4.pubkey())
            .unwrap();
        bank1
            .transfer(sol_to_lamports(5.), &mint_keypair, &key5.pubkey())
            .unwrap();
        bank1.fill_bank_with_ticks_for_tests();

        let (_tmp_dir, accounts_dir) = create_tmp_accounts_dir_for_tests();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let snapshot_archive_format = SnapshotConfig::default().archive_format;

        let full_snapshot_slot = slot;
        let full_snapshot_archive_info = bank_to_full_snapshot_archive(
            bank_snapshots_dir.path(),
            &bank1,
            None,
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            snapshot_archive_format,
        )
        .unwrap();

        let slot = slot + 1;
        let bank2 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank1, &collector, slot);
        bank2
            .transfer(sol_to_lamports(1.), &mint_keypair, &key1.pubkey())
            .unwrap();
        bank2.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank3 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank2, &collector, slot);
        bank3
            .transfer(sol_to_lamports(1.), &mint_keypair, &key1.pubkey())
            .unwrap();
        bank3.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank4 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank3, &collector, slot);
        bank4
            .transfer(sol_to_lamports(1.), &mint_keypair, &key1.pubkey())
            .unwrap();
        bank4.fill_bank_with_ticks_for_tests();

        let incremental_snapshot_archive_info = bank_to_incremental_snapshot_archive(
            bank_snapshots_dir.path(),
            &bank4,
            full_snapshot_slot,
            None,
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            snapshot_archive_format,
        )
        .unwrap();

        let (roundtrip_bank, _) = bank_from_snapshot_archives(
            &[accounts_dir],
            bank_snapshots_dir.path(),
            &full_snapshot_archive_info,
            Some(&incremental_snapshot_archive_info),
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            None,
            false,
            false,
            false,
            Some(ACCOUNTS_DB_CONFIG_FOR_TESTING),
            None,
            Arc::default(),
        )
        .unwrap();
        roundtrip_bank.wait_for_initial_accounts_hash_verification_completed_for_tests();
        assert_eq!(*bank4, roundtrip_bank);
    }

    /// Test rebuilding bank from the latest snapshot archives
    #[test]
    fn test_bank_from_latest_snapshot_archives() {
        let collector = Pubkey::new_unique();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();

        let (genesis_config, mint_keypair) = create_genesis_config(sol_to_lamports(1_000_000.));
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        bank0
            .transfer(sol_to_lamports(1.), &mint_keypair, &key1.pubkey())
            .unwrap();
        bank0
            .transfer(sol_to_lamports(2.), &mint_keypair, &key2.pubkey())
            .unwrap();
        bank0
            .transfer(sol_to_lamports(3.), &mint_keypair, &key3.pubkey())
            .unwrap();
        bank0.fill_bank_with_ticks_for_tests();

        let slot = 1;
        let bank1 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
        bank1
            .transfer(sol_to_lamports(1.), &mint_keypair, &key1.pubkey())
            .unwrap();
        bank1
            .transfer(sol_to_lamports(2.), &mint_keypair, &key2.pubkey())
            .unwrap();
        bank1
            .transfer(sol_to_lamports(3.), &mint_keypair, &key3.pubkey())
            .unwrap();
        bank1.fill_bank_with_ticks_for_tests();

        let (_tmp_dir, accounts_dir) = create_tmp_accounts_dir_for_tests();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let snapshot_archive_format = SnapshotConfig::default().archive_format;

        let full_snapshot_slot = slot;
        bank_to_full_snapshot_archive(
            &bank_snapshots_dir,
            &bank1,
            None,
            &full_snapshot_archives_dir,
            &incremental_snapshot_archives_dir,
            snapshot_archive_format,
        )
        .unwrap();

        let slot = slot + 1;
        let bank2 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank1, &collector, slot);
        bank2
            .transfer(sol_to_lamports(1.), &mint_keypair, &key1.pubkey())
            .unwrap();
        bank2.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank3 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank2, &collector, slot);
        bank3
            .transfer(sol_to_lamports(2.), &mint_keypair, &key2.pubkey())
            .unwrap();
        bank3.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank4 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank3, &collector, slot);
        bank4
            .transfer(sol_to_lamports(3.), &mint_keypair, &key3.pubkey())
            .unwrap();
        bank4.fill_bank_with_ticks_for_tests();

        bank_to_incremental_snapshot_archive(
            &bank_snapshots_dir,
            &bank4,
            full_snapshot_slot,
            None,
            &full_snapshot_archives_dir,
            &incremental_snapshot_archives_dir,
            snapshot_archive_format,
        )
        .unwrap();

        let (deserialized_bank, ..) = bank_from_latest_snapshot_archives(
            &bank_snapshots_dir,
            &full_snapshot_archives_dir,
            &incremental_snapshot_archives_dir,
            &[accounts_dir],
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            None,
            false,
            false,
            false,
            Some(ACCOUNTS_DB_CONFIG_FOR_TESTING),
            None,
            Arc::default(),
        )
        .unwrap();
        deserialized_bank.wait_for_initial_accounts_hash_verification_completed_for_tests();
        assert_eq!(deserialized_bank, *bank4);
    }

    /// Test that cleaning works well in the edge cases of zero-lamport accounts and snapshots.
    /// Here's the scenario:
    ///
    /// slot 1:
    ///     - send some lamports to Account1 (from Account2) to bring it to life
    ///     - take a full snapshot
    /// slot 2:
    ///     - make Account1 have zero lamports (send back to Account2)
    ///     - take an incremental snapshot
    ///     - ensure deserializing from this snapshot is equal to this bank
    /// slot 3:
    ///     - remove Account2's reference back to slot 2 by transferring from the mint to Account2
    /// slot 4:
    ///     - ensure `clean_accounts()` has run and that Account1 is gone
    ///     - take another incremental snapshot
    ///     - ensure deserializing from this snapshots is equal to this bank
    ///     - ensure Account1 hasn't come back from the dead
    ///
    /// The check at slot 4 will fail with the pre-incremental-snapshot cleaning logic.  Because
    /// of the cleaning/purging at slot 4, the incremental snapshot at slot 4 will no longer have
    /// information about Account1, but the full snapshost _does_ have info for Account1, which is
    /// no longer correct!
    #[test]
    fn test_incremental_snapshots_handle_zero_lamport_accounts() {
        let collector = Pubkey::new_unique();
        let key1 = Keypair::new();
        let key2 = Keypair::new();

        let (_tmp_dir, accounts_dir) = create_tmp_accounts_dir_for_tests();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let snapshot_archive_format = SnapshotConfig::default().archive_format;

        let (mut genesis_config, mint_keypair) = create_genesis_config(sol_to_lamports(1_000_000.));
        // test expects 0 transaction fee
        genesis_config.fee_rate_governor = solana_fee_calculator::FeeRateGovernor::new(0, 0);

        let lamports_to_transfer = sol_to_lamports(123_456.);
        let (bank0, bank_forks) = Bank::new_with_paths_for_tests(
            &genesis_config,
            Arc::<RuntimeConfig>::default(),
            BankTestConfig::default(),
            vec![accounts_dir.clone()],
        )
        .wrap_with_bank_forks_for_tests();
        bank0
            .transfer(lamports_to_transfer, &mint_keypair, &key2.pubkey())
            .unwrap();
        bank0.fill_bank_with_ticks_for_tests();

        let slot = 1;
        let bank1 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
        bank1
            .transfer(lamports_to_transfer, &key2, &key1.pubkey())
            .unwrap();
        bank1.fill_bank_with_ticks_for_tests();

        let full_snapshot_slot = slot;
        let full_snapshot_archive_info = bank_to_full_snapshot_archive(
            bank_snapshots_dir.path(),
            &bank1,
            None,
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            snapshot_archive_format,
        )
        .unwrap();

        let slot = slot + 1;
        let bank2 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank1, &collector, slot);
        let blockhash = bank2.last_blockhash();
        let tx = SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
            &key1,
            &key2.pubkey(),
            lamports_to_transfer,
            blockhash,
        ));
        let fee = bank2.get_fee_for_message(tx.message()).unwrap();
        let tx = system_transaction::transfer(
            &key1,
            &key2.pubkey(),
            lamports_to_transfer - fee,
            blockhash,
        );
        bank2.process_transaction(&tx).unwrap();
        assert_eq!(
            bank2.get_balance(&key1.pubkey()),
            0,
            "Ensure Account1's balance is zero"
        );
        bank2.fill_bank_with_ticks_for_tests();

        // Take an incremental snapshot and then do a roundtrip on the bank and ensure it
        // deserializes correctly.
        let incremental_snapshot_archive_info = bank_to_incremental_snapshot_archive(
            bank_snapshots_dir.path(),
            &bank2,
            full_snapshot_slot,
            None,
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            snapshot_archive_format,
        )
        .unwrap();
        let (deserialized_bank, _) = bank_from_snapshot_archives(
            &[accounts_dir.clone()],
            bank_snapshots_dir.path(),
            &full_snapshot_archive_info,
            Some(&incremental_snapshot_archive_info),
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            None,
            false,
            false,
            false,
            Some(ACCOUNTS_DB_CONFIG_FOR_TESTING),
            None,
            Arc::default(),
        )
        .unwrap();
        deserialized_bank.wait_for_initial_accounts_hash_verification_completed_for_tests();
        assert_eq!(
            deserialized_bank, *bank2,
            "Ensure rebuilding from an incremental snapshot works"
        );

        let slot = slot + 1;
        let bank3 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank2, &collector, slot);
        // Update Account2 so that it no longer holds a reference to slot2
        bank3
            .transfer(lamports_to_transfer, &mint_keypair, &key2.pubkey())
            .unwrap();
        bank3.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank4 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank3, &collector, slot);
        bank4.fill_bank_with_ticks_for_tests();

        // Ensure account1 has been cleaned/purged from everywhere
        bank4.squash();
        bank4.clean_accounts();
        assert!(
            bank4.get_account_modified_slot(&key1.pubkey()).is_none(),
            "Ensure Account1 has been cleaned and purged from AccountsDb"
        );

        // Take an incremental snapshot and then do a roundtrip on the bank and ensure it
        // deserializes correctly
        let incremental_snapshot_archive_info = bank_to_incremental_snapshot_archive(
            bank_snapshots_dir.path(),
            &bank4,
            full_snapshot_slot,
            None,
            full_snapshot_archives_dir.path(),
            incremental_snapshot_archives_dir.path(),
            snapshot_archive_format,
        )
        .unwrap();

        let (deserialized_bank, _) = bank_from_snapshot_archives(
            &[accounts_dir],
            bank_snapshots_dir.path(),
            &full_snapshot_archive_info,
            Some(&incremental_snapshot_archive_info),
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            None,
            false,
            false,
            false,
            Some(ACCOUNTS_DB_CONFIG_FOR_TESTING),
            None,
            Arc::default(),
        )
        .unwrap();
        deserialized_bank.wait_for_initial_accounts_hash_verification_completed_for_tests();
        assert_eq!(
            deserialized_bank, *bank4,
            "Ensure rebuilding from an incremental snapshot works",
        );
        assert!(
            deserialized_bank
                .get_account_modified_slot(&key1.pubkey())
                .is_none(),
            "Ensure Account1 has not been brought back from the dead"
        );
    }

    #[test_case(StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_bank_fields_from_snapshot(storage_access: StorageAccess) {
        let collector = Pubkey::new_unique();
        let key1 = Keypair::new();

        let (genesis_config, mint_keypair) = create_genesis_config(sol_to_lamports(1_000_000.));
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        bank0.fill_bank_with_ticks_for_tests();

        let slot = 1;
        let bank1 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
        bank1.fill_bank_with_ticks_for_tests();

        let all_snapshots_dir = tempfile::TempDir::new().unwrap();
        let snapshot_archive_format = SnapshotConfig::default().archive_format;

        let full_snapshot_slot = slot;
        bank_to_full_snapshot_archive(
            &all_snapshots_dir,
            &bank1,
            None,
            &all_snapshots_dir,
            &all_snapshots_dir,
            snapshot_archive_format,
        )
        .unwrap();

        let slot = slot + 1;
        let bank2 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank1, &collector, slot);
        bank2
            .transfer(sol_to_lamports(1.), &mint_keypair, &key1.pubkey())
            .unwrap();
        bank2.fill_bank_with_ticks_for_tests();

        bank_to_incremental_snapshot_archive(
            &all_snapshots_dir,
            &bank2,
            full_snapshot_slot,
            None,
            &all_snapshots_dir,
            &all_snapshots_dir,
            snapshot_archive_format,
        )
        .unwrap();

        let bank_fields = bank_fields_from_snapshot_archives(
            &all_snapshots_dir,
            &all_snapshots_dir,
            storage_access,
        )
        .unwrap();
        assert_eq!(bank_fields.slot, bank2.slot());
        assert_eq!(bank_fields.parent_slot, bank2.parent_slot());
    }

    #[test]
    fn test_bank_snapshot_dir_accounts_hardlinks() {
        let bank = Bank::new_for_tests(&GenesisConfig::default());
        bank.fill_bank_with_ticks_for_tests();

        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        bank_to_full_snapshot_archive_with(
            &bank_snapshots_dir,
            &bank,
            SnapshotVersion::default(),
            &snapshot_archives_dir,
            &snapshot_archives_dir,
            SnapshotConfig::default().archive_format,
            true,
        )
        .unwrap();

        let accounts_hardlinks_dir = get_bank_snapshot_dir(&bank_snapshots_dir, bank.slot())
            .join(snapshot_utils::SNAPSHOT_ACCOUNTS_HARDLINKS);
        assert!(fs::metadata(&accounts_hardlinks_dir).is_ok());

        let mut hardlink_dirs = Vec::new();
        // This directory contain symlinks to all accounts snapshot directories.
        for entry in fs::read_dir(accounts_hardlinks_dir).unwrap() {
            let entry = entry.unwrap();
            let symlink = entry.path();
            let dst_path = fs::read_link(symlink).unwrap();
            assert!(fs::metadata(&dst_path).is_ok());
            hardlink_dirs.push(dst_path);
        }

        let bank_snapshot_dir = get_bank_snapshot_dir(&bank_snapshots_dir, bank.slot());
        assert!(purge_bank_snapshot(bank_snapshot_dir).is_ok());

        // When the bank snapshot is removed, all the snapshot hardlink directories should be removed.
        assert!(hardlink_dirs.iter().all(|dir| fs::metadata(dir).is_err()));
    }

    #[test_case(false)]
    #[test_case(true)]
    fn test_get_highest_bank_snapshot(should_flush_and_hard_link_storages: bool) {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let _bank = create_snapshot_dirs_for_tests(
            &genesis_config,
            &bank_snapshots_dir,
            4,
            0,
            should_flush_and_hard_link_storages,
        );

        let snapshot = get_highest_bank_snapshot(&bank_snapshots_dir).unwrap();
        assert_eq!(snapshot.slot, 4);

        let complete_flag_file = snapshot
            .snapshot_dir
            .join(snapshot_utils::SNAPSHOT_STATE_COMPLETE_FILENAME);
        fs::remove_file(complete_flag_file).unwrap();
        // The incomplete snapshot dir should still exist
        let snapshot_dir_4 = snapshot.snapshot_dir;
        assert!(snapshot_dir_4.exists());
        let snapshot = get_highest_bank_snapshot(&bank_snapshots_dir).unwrap();
        assert_eq!(snapshot.slot, 3);

        let snapshot_version_file = snapshot
            .snapshot_dir
            .join(snapshot_utils::SNAPSHOT_VERSION_FILENAME);
        fs::remove_file(snapshot_version_file).unwrap();
        let snapshot = get_highest_bank_snapshot(&bank_snapshots_dir).unwrap();
        assert_eq!(snapshot.slot, 2);

        let status_cache_file = snapshot
            .snapshot_dir
            .join(snapshot_utils::SNAPSHOT_STATUS_CACHE_FILENAME);
        fs::remove_file(status_cache_file).unwrap();
        let snapshot = get_highest_bank_snapshot(&bank_snapshots_dir).unwrap();
        assert_eq!(snapshot.slot, 1);
    }

    #[test]
    fn test_clean_orphaned_account_snapshot_dirs() {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let _bank =
            create_snapshot_dirs_for_tests(&genesis_config, &bank_snapshots_dir, 2, 0, true);

        let snapshot_dir_slot_2 = bank_snapshots_dir.path().join("2");
        let accounts_link_dir_slot_2 =
            snapshot_dir_slot_2.join(snapshot_utils::SNAPSHOT_ACCOUNTS_HARDLINKS);

        // the symlinks point to the account snapshot hardlink directories <account_path>/snapshot/<slot>/ for slot 2
        // get them via read_link
        let hardlink_dirs_slot_2: Vec<PathBuf> = fs::read_dir(accounts_link_dir_slot_2)
            .unwrap()
            .map(|entry| {
                let symlink = entry.unwrap().path();
                fs::read_link(symlink).unwrap()
            })
            .collect();

        // remove the bank snapshot directory for slot 2, so the account snapshot slot 2 directories become orphaned
        fs::remove_dir_all(snapshot_dir_slot_2).unwrap();

        // verify the orphaned account snapshot hardlink directories are still there
        assert!(hardlink_dirs_slot_2
            .iter()
            .all(|dir| fs::metadata(dir).is_ok()));

        let account_snapshot_paths: Vec<PathBuf> = hardlink_dirs_slot_2
            .iter()
            .map(|dir| dir.parent().unwrap().parent().unwrap().to_path_buf())
            .collect();
        // clean the orphaned hardlink directories
        clean_orphaned_account_snapshot_dirs(&bank_snapshots_dir, &account_snapshot_paths).unwrap();

        // verify the hardlink directories are gone
        assert!(hardlink_dirs_slot_2
            .iter()
            .all(|dir| fs::metadata(dir).is_err()));
    }

    // Ensure that `clean_orphaned_account_snapshot_dirs()` works correctly for bank snapshots
    // that *do not* hard link the storages into their staging dir.
    #[test]
    fn test_clean_orphaned_account_snapshot_dirs_no_hard_link() {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let _bank =
            create_snapshot_dirs_for_tests(&genesis_config, &bank_snapshots_dir, 2, 0, false);

        // Ensure the bank snapshot dir does exist.
        let bank_snapshot_dir = snapshot_utils::get_bank_snapshot_dir(&bank_snapshots_dir, 2);
        assert!(fs::exists(&bank_snapshot_dir).unwrap());

        // Ensure the accounts hard links dir does *not* exist for this bank snapshot
        // (since we asked create_snapshot_dirs_for_tests() to *not* hard link).
        let bank_snapshot_accounts_hard_link_dir =
            bank_snapshot_dir.join(snapshot_utils::SNAPSHOT_ACCOUNTS_HARDLINKS);
        assert!(!fs::exists(&bank_snapshot_accounts_hard_link_dir).unwrap());

        // Now make sure clean_orphaned_account_snapshot_dirs() doesn't error.
        clean_orphaned_account_snapshot_dirs(&bank_snapshots_dir, &[]).unwrap();
    }

    #[test_case(false)]
    #[test_case(true)]
    fn test_purge_incomplete_bank_snapshots(should_flush_and_hard_link_storages: bool) {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let _bank = create_snapshot_dirs_for_tests(
            &genesis_config,
            &bank_snapshots_dir,
            2,
            0,
            should_flush_and_hard_link_storages,
        );

        // remove the "state complete" files so the snapshots will be purged
        for slot in [1, 2] {
            let bank_snapshot_dir = get_bank_snapshot_dir(&bank_snapshots_dir, slot);
            let state_complete_file =
                bank_snapshot_dir.join(snapshot_utils::SNAPSHOT_STATE_COMPLETE_FILENAME);
            fs::remove_file(state_complete_file).unwrap();
        }

        purge_incomplete_bank_snapshots(&bank_snapshots_dir);

        // ensure the bank snapshots dirs are gone
        for slot in [1, 2] {
            let bank_snapshot_dir = get_bank_snapshot_dir(&bank_snapshots_dir, slot);
            assert!(!bank_snapshot_dir.exists());
        }
    }

    /// Test that snapshots with the Incremental Accounts Hash feature enabled can roundtrip.
    ///
    /// This test generates banks with zero and non-zero lamport accounts then takes full and
    /// incremental snapshots.  A bank is deserialized from the snapshots, its incremental
    /// accounts hash is recalculated, and then compared with the original.
    #[test]
    fn test_incremental_snapshot_with_incremental_accounts_hash() {
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = tempfile::TempDir::new().unwrap();

        let mut genesis_config_info = genesis_utils::create_genesis_config_with_leader(
            1_000_000 * LAMPORTS_PER_SOL,
            &Pubkey::new_unique(),
            100 * LAMPORTS_PER_SOL,
        );
        let mint = &genesis_config_info.mint_keypair;
        // When the snapshots lt hash feature is enabled, the IAH is effectively *disabled*,
        // which causes this test to fail.
        // Disable the snapshots lt hash feature by removing its account from genesis.
        genesis_config_info
            .genesis_config
            .accounts
            .remove(&feature_set::snapshots_lt_hash::id())
            .unwrap();

        let do_transfers = |bank: &Bank| {
            let key1 = Keypair::new(); // lamports from mint
            let key2 = Keypair::new(); // will end with ZERO lamports
            let key3 = Keypair::new(); // lamports from key2

            let amount = 123_456_789;
            let fee = {
                let blockhash = bank.last_blockhash();
                let transaction = SanitizedTransaction::from_transaction_for_tests(
                    system_transaction::transfer(&key2, &key3.pubkey(), amount, blockhash),
                );
                bank.get_fee_for_message(transaction.message()).unwrap()
            };
            bank.transfer(amount + fee, mint, &key1.pubkey()).unwrap();
            bank.transfer(amount + fee, mint, &key2.pubkey()).unwrap();
            bank.transfer(amount + fee, &key2, &key3.pubkey()).unwrap();
            assert_eq!(bank.get_balance(&key2.pubkey()), 0);

            bank.fill_bank_with_ticks_for_tests();
        };

        let (mut bank, bank_forks) =
            Bank::new_with_bank_forks_for_tests(&genesis_config_info.genesis_config);

        // make some banks, do some transactions, ensure there's some zero-lamport accounts
        for _ in 0..5 {
            let slot = bank.slot() + 1;
            bank = new_bank_from_parent_with_bank_forks(
                bank_forks.as_ref(),
                bank,
                &Pubkey::new_unique(),
                slot,
            );
            do_transfers(&bank);
        }

        // take full snapshot, save off the calculated accounts hash
        let full_snapshot_archive = bank_to_full_snapshot_archive(
            &bank_snapshots_dir,
            &bank,
            None,
            &full_snapshot_archives_dir,
            &incremental_snapshot_archives_dir,
            SnapshotConfig::default().archive_format,
        )
        .unwrap();
        let full_accounts_hash = bank
            .rc
            .accounts
            .accounts_db
            .get_accounts_hash(bank.slot())
            .unwrap();

        // make more banks, do more transactions, ensure there's more zero-lamport accounts
        for _ in 0..5 {
            let slot = bank.slot() + 1;
            bank = new_bank_from_parent_with_bank_forks(
                bank_forks.as_ref(),
                bank,
                &Pubkey::new_unique(),
                slot,
            );
            do_transfers(&bank);
        }

        // take incremental snapshot, save off the calculated incremental accounts hash
        let incremental_snapshot_archive = bank_to_incremental_snapshot_archive(
            &bank_snapshots_dir,
            &bank,
            full_snapshot_archive.slot(),
            None,
            &full_snapshot_archives_dir,
            &incremental_snapshot_archives_dir,
            SnapshotConfig::default().archive_format,
        )
        .unwrap();
        let incremental_accounts_hash = bank
            .rc
            .accounts
            .accounts_db
            .get_incremental_accounts_hash(bank.slot())
            .unwrap();

        // reconstruct a bank from the snapshots
        let other_accounts_dir = tempfile::TempDir::new().unwrap();
        let other_bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let (deserialized_bank, _) = bank_from_snapshot_archives(
            &[other_accounts_dir.path().to_path_buf()],
            &other_bank_snapshots_dir,
            &full_snapshot_archive,
            Some(&incremental_snapshot_archive),
            &genesis_config_info.genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            None,
            false,
            false,
            false,
            Some(ACCOUNTS_DB_CONFIG_FOR_TESTING),
            None,
            Arc::default(),
        )
        .unwrap();
        deserialized_bank.wait_for_initial_accounts_hash_verification_completed_for_tests();
        assert_eq!(&deserialized_bank, bank.as_ref());

        // ensure the accounts hash stored in the deserialized bank matches
        let deserialized_accounts_hash = deserialized_bank
            .rc
            .accounts
            .accounts_db
            .get_accounts_hash(full_snapshot_archive.slot())
            .unwrap();
        assert_eq!(deserialized_accounts_hash, full_accounts_hash);

        // ensure the incremental accounts hash stored in the deserialized bank matches
        let deserialized_incrmental_accounts_hash = deserialized_bank
            .rc
            .accounts
            .accounts_db
            .get_incremental_accounts_hash(incremental_snapshot_archive.slot())
            .unwrap();
        assert_eq!(
            deserialized_incrmental_accounts_hash,
            incremental_accounts_hash
        );

        // recalculate the incremental accounts hash on the desserialized bank and ensure it matches
        let other_incremental_snapshot_storages =
            deserialized_bank.get_snapshot_storages(Some(full_snapshot_archive.slot()));
        let other_incremental_accounts_hash = bank
            .rc
            .accounts
            .accounts_db
            .calculate_incremental_accounts_hash(
                &CalcAccountsHashConfig {
                    use_bg_thread_pool: false,
                    ancestors: None,
                    epoch_schedule: deserialized_bank.epoch_schedule(),
                    epoch: deserialized_bank.epoch(),
                    store_detailed_debug_info_on_failure: false,
                },
                &SortedStorages::new(&other_incremental_snapshot_storages),
                HashStats::default(),
            );
        assert_eq!(other_incremental_accounts_hash, incremental_accounts_hash);
    }

    /// Test that snapshots correctly handle zero lamport accounts
    ///
    /// slot 1:
    ///     - send some lamports to Account1 (from Account2) to bring it to life
    ///     - Send some lamports to Account3 (from mint) to preserve this slot
    ///     - Root slot 1 and flush write cache
    /// slot 2:
    ///     - make Account1 have zero lamports (send back to Account2)
    /// slot 3:
    ///     - remove Account2's reference back to slot 2 by transferring from the mint to Account2
    ///     - take a full snap shot
    ///     - verify that recovery from full snapshot does not bring account1 back to life
    #[test_case(StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_snapshots_handle_zero_lamport_accounts(storage_access: StorageAccess) {
        let collector = Pubkey::new_unique();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();

        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let snapshot_archive_format = SnapshotConfig::default().archive_format;

        let (genesis_config, mint_keypair) = create_genesis_config(sol_to_lamports(1_000_000.));

        let lamports_to_transfer = sol_to_lamports(123_456.);
        let bank_test_config = BankTestConfig {
            accounts_db_config: AccountsDbConfig {
                storage_access,
                ..AccountsDbConfig::default()
            },
        };

        let bank0 = Bank::new_with_config_for_tests(&genesis_config, bank_test_config);

        let (bank0, bank_forks) = Bank::wrap_with_bank_forks_for_tests(bank0);

        bank0
            .transfer(lamports_to_transfer, &mint_keypair, &key2.pubkey())
            .unwrap();

        bank0.fill_bank_with_ticks_for_tests();

        let slot = 1;
        let bank1 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
        bank1
            .transfer(lamports_to_transfer, &key2, &key1.pubkey())
            .unwrap();
        bank1
            .transfer(lamports_to_transfer, &mint_keypair, &key3.pubkey())
            .unwrap();

        bank1.fill_bank_with_ticks_for_tests();

        // Force rooting of slot1
        bank1.squash();
        bank1.force_flush_accounts_cache();

        let slot = slot + 1;
        let bank2 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank1, &collector, slot);
        let blockhash = bank2.last_blockhash();
        let tx = SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
            &key1,
            &key2.pubkey(),
            lamports_to_transfer,
            blockhash,
        ));

        let fee = bank2.get_fee_for_message(tx.message()).unwrap();
        bank2
            .transfer(lamports_to_transfer - fee, &key1, &key2.pubkey())
            .unwrap();

        assert_eq!(
            bank2.get_balance(&key1.pubkey()),
            0,
            "Ensure Account1's balance is zero"
        );
        bank2.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank3 =
            new_bank_from_parent_with_bank_forks(bank_forks.as_ref(), bank2, &collector, slot);
        // Update Account2 so that it no longer holds a reference to slot2
        bank3
            .transfer(lamports_to_transfer, &mint_keypair, &key2.pubkey())
            .unwrap();
        bank3.fill_bank_with_ticks_for_tests();

        assert!(
            bank3.get_account_modified_slot(&key1.pubkey()).is_none(),
            "Ensure Account1 has been cleaned and purged from AccountsDb"
        );

        // Take full snapshot and then do a roundtrip on the bank and ensure it
        // deserializes correctly
        let full_snapshot_archive_info = bank_to_full_snapshot_archive(
            bank_snapshots_dir.path(),
            &bank3,
            None,
            full_snapshot_archives_dir.path(),
            full_snapshot_archives_dir.path(),
            snapshot_archive_format,
        )
        .unwrap();

        let accounts_dir = tempfile::TempDir::new().unwrap();
        let other_bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let (deserialized_bank, _) = bank_from_snapshot_archives(
            &[accounts_dir.path().to_path_buf()],
            other_bank_snapshots_dir.path(),
            &full_snapshot_archive_info,
            None,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            None,
            false,
            false,
            false,
            Some(ACCOUNTS_DB_CONFIG_FOR_TESTING),
            None,
            Arc::default(),
        )
        .unwrap();

        deserialized_bank.wait_for_initial_accounts_hash_verification_completed_for_tests();

        assert!(
            deserialized_bank
                .get_account_modified_slot(&key1.pubkey())
                .is_none(),
            "Ensure Account1 has not been brought back from the dead"
        );

        assert_eq!(*bank3, deserialized_bank);
    }

    #[test_case(StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_bank_from_snapshot_dir(storage_access: StorageAccess) {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let bank = create_snapshot_dirs_for_tests(&genesis_config, &bank_snapshots_dir, 3, 0, true);

        let bank_snapshot = get_highest_bank_snapshot(&bank_snapshots_dir).unwrap();
        let account_paths = &bank.rc.accounts.accounts_db.paths;

        let (bank_constructed, ..) = bank_from_snapshot_dir(
            account_paths,
            &bank_snapshot,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            None,
            false,
            Some(AccountsDbConfig {
                storage_access,
                ..ACCOUNTS_DB_CONFIG_FOR_TESTING
            }),
            None,
            Arc::default(),
        )
        .unwrap();

        bank_constructed.wait_for_initial_accounts_hash_verification_completed_for_tests();
        assert_eq!(bank_constructed, bank);

        // Verify that the next_append_vec_id tracking is correct
        let mut max_id = 0;
        for path in account_paths {
            fs::read_dir(path).unwrap().for_each(|entry| {
                let path = entry.unwrap().path();
                let filename = path.file_name().unwrap();
                let (_slot, append_vec_id) =
                    get_slot_and_append_vec_id(filename.to_str().unwrap()).unwrap();
                max_id = std::cmp::max(max_id, append_vec_id);
            });
        }
        let next_id = bank.accounts().accounts_db.next_id.load(Ordering::Relaxed) as usize;
        assert_eq!(max_id, next_id - 1);
    }

    #[test]
    fn test_bank_from_latest_snapshot_dir() {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let bank = create_snapshot_dirs_for_tests(&genesis_config, &bank_snapshots_dir, 3, 3, true);

        let account_paths = &bank.rc.accounts.accounts_db.paths;

        let deserialized_bank = bank_from_latest_snapshot_dir(
            &bank_snapshots_dir,
            &genesis_config,
            &RuntimeConfig::default(),
            account_paths,
            None,
            None,
            None,
            false,
            Some(ACCOUNTS_DB_CONFIG_FOR_TESTING),
            None,
            Arc::default(),
        )
        .unwrap();

        assert_eq!(
            deserialized_bank, bank,
            "Ensure rebuilding bank from the highest snapshot dir results in the highest bank",
        );
    }

    #[test_case(false)]
    #[test_case(true)]
    fn test_purge_all_bank_snapshots(should_flush_and_hard_link_storages: bool) {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let _bank = create_snapshot_dirs_for_tests(
            &genesis_config,
            &bank_snapshots_dir,
            10,
            5,
            should_flush_and_hard_link_storages,
        );
        // Keep bank in this scope so that its account_paths tmp dirs are not released, and purge_all_bank_snapshots
        // can clear the account hardlinks correctly.

        assert_eq!(get_bank_snapshots(&bank_snapshots_dir).len(), 10);
        purge_all_bank_snapshots(&bank_snapshots_dir);
        assert_eq!(get_bank_snapshots(&bank_snapshots_dir).len(), 0);
    }

    #[test_case(false)]
    #[test_case(true)]
    fn test_purge_old_bank_snapshots(should_flush_and_hard_link_storages: bool) {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let _bank = create_snapshot_dirs_for_tests(
            &genesis_config,
            &bank_snapshots_dir,
            10,
            5,
            should_flush_and_hard_link_storages,
        );
        // Keep bank in this scope so that its account_paths tmp dirs are not released, and purge_old_bank_snapshots
        // can clear the account hardlinks correctly.

        assert_eq!(get_bank_snapshots(&bank_snapshots_dir).len(), 10);

        purge_old_bank_snapshots(&bank_snapshots_dir, 3, Some(BankSnapshotKind::Pre));
        assert_eq!(get_bank_snapshots_pre(&bank_snapshots_dir).len(), 3);

        purge_old_bank_snapshots(&bank_snapshots_dir, 2, Some(BankSnapshotKind::Post));
        assert_eq!(get_bank_snapshots_post(&bank_snapshots_dir).len(), 2);

        assert_eq!(get_bank_snapshots(&bank_snapshots_dir).len(), 5);

        purge_old_bank_snapshots(&bank_snapshots_dir, 2, None);
        assert_eq!(get_bank_snapshots(&bank_snapshots_dir).len(), 2);

        purge_old_bank_snapshots(&bank_snapshots_dir, 0, None);
        assert_eq!(get_bank_snapshots(&bank_snapshots_dir).len(), 0);
    }

    #[test_case(false)]
    #[test_case(true)]
    fn test_purge_bank_snapshots_older_than_slot(should_flush_and_hard_link_storages: bool) {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();

        // The bank must stay in scope to ensure the temp dirs that it holds are not dropped
        let _bank = create_snapshot_dirs_for_tests(
            &genesis_config,
            &bank_snapshots_dir,
            9,
            6,
            should_flush_and_hard_link_storages,
        );
        let bank_snapshots_before = get_bank_snapshots(&bank_snapshots_dir);

        purge_bank_snapshots_older_than_slot(&bank_snapshots_dir, 0);
        let bank_snapshots_after = get_bank_snapshots(&bank_snapshots_dir);
        assert_eq!(bank_snapshots_before.len(), bank_snapshots_after.len());

        purge_bank_snapshots_older_than_slot(&bank_snapshots_dir, 3);
        let bank_snapshots_after = get_bank_snapshots(&bank_snapshots_dir);
        assert_eq!(bank_snapshots_before.len(), bank_snapshots_after.len() + 2);

        purge_bank_snapshots_older_than_slot(&bank_snapshots_dir, 8);
        let bank_snapshots_after = get_bank_snapshots(&bank_snapshots_dir);
        assert_eq!(bank_snapshots_before.len(), bank_snapshots_after.len() + 7);

        purge_bank_snapshots_older_than_slot(&bank_snapshots_dir, Slot::MAX);
        let bank_snapshots_after = get_bank_snapshots(&bank_snapshots_dir);
        assert_eq!(bank_snapshots_before.len(), bank_snapshots_after.len() + 9);
        assert!(bank_snapshots_after.is_empty());
    }

    #[test_case(false)]
    #[test_case(true)]
    fn test_purge_old_bank_snapshots_at_startup(should_flush_and_hard_link_storages: bool) {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();

        // The bank must stay in scope to ensure the temp dirs that it holds are not dropped
        let _bank = create_snapshot_dirs_for_tests(
            &genesis_config,
            &bank_snapshots_dir,
            9,
            6,
            should_flush_and_hard_link_storages,
        );

        purge_old_bank_snapshots_at_startup(&bank_snapshots_dir);

        let bank_snapshots_pre = get_bank_snapshots_pre(&bank_snapshots_dir);
        assert!(bank_snapshots_pre.is_empty());

        let bank_snapshots_post = get_bank_snapshots_post(&bank_snapshots_dir);
        assert_eq!(bank_snapshots_post.len(), 1);
        assert_eq!(bank_snapshots_post.first().unwrap().slot, 6);
    }

    #[test]
    fn test_verify_slot_deltas_structural_bad_too_many_entries() {
        let bank_slot = status_cache::MAX_CACHE_ENTRIES as Slot + 1;
        let slot_deltas: Vec<_> = (0..bank_slot)
            .map(|slot| (slot, true, Status::default()))
            .collect();

        let result = verify_slot_deltas_structural(slot_deltas.as_slice(), bank_slot);
        assert_eq!(
            result,
            Err(VerifySlotDeltasError::TooManyEntries(
                status_cache::MAX_CACHE_ENTRIES + 1,
                status_cache::MAX_CACHE_ENTRIES
            )),
        );
    }

    #[test]
    fn test_verify_slot_deltas_structural_good() {
        // NOTE: slot deltas do not need to be sorted
        let slot_deltas = vec![
            (222, true, Status::default()),
            (333, true, Status::default()),
            (111, true, Status::default()),
        ];

        let bank_slot = 333;
        let result = verify_slot_deltas_structural(slot_deltas.as_slice(), bank_slot);
        assert_eq!(
            result,
            Ok(VerifySlotDeltasStructuralInfo {
                slots: HashSet::from([111, 222, 333])
            })
        );
    }

    #[test]
    fn test_verify_slot_deltas_structural_bad_slot_not_root() {
        let slot_deltas = vec![
            (111, true, Status::default()),
            (222, false, Status::default()), // <-- slot is not a root
            (333, true, Status::default()),
        ];

        let bank_slot = 333;
        let result = verify_slot_deltas_structural(slot_deltas.as_slice(), bank_slot);
        assert_eq!(result, Err(VerifySlotDeltasError::SlotIsNotRoot(222)));
    }

    #[test]
    fn test_verify_slot_deltas_structural_bad_slot_greater_than_bank() {
        let slot_deltas = vec![
            (222, true, Status::default()),
            (111, true, Status::default()),
            (555, true, Status::default()), // <-- slot is greater than the bank slot
        ];

        let bank_slot = 444;
        let result = verify_slot_deltas_structural(slot_deltas.as_slice(), bank_slot);
        assert_eq!(
            result,
            Err(VerifySlotDeltasError::SlotGreaterThanMaxRoot(
                555, bank_slot
            )),
        );
    }

    #[test]
    fn test_verify_slot_deltas_structural_bad_slot_has_multiple_entries() {
        let slot_deltas = vec![
            (111, true, Status::default()),
            (222, true, Status::default()),
            (111, true, Status::default()), // <-- slot is a duplicate
        ];

        let bank_slot = 222;
        let result = verify_slot_deltas_structural(slot_deltas.as_slice(), bank_slot);
        assert_eq!(
            result,
            Err(VerifySlotDeltasError::SlotHasMultipleEntries(111)),
        );
    }

    #[test]
    fn test_verify_slot_deltas_with_history_good() {
        let mut slots_from_slot_deltas = HashSet::default();
        let mut slot_history = SlotHistory::default();
        // note: slot history expects slots to be added in numeric order
        for slot in [0, 111, 222, 333, 444] {
            slots_from_slot_deltas.insert(slot);
            slot_history.add(slot);
        }

        let bank_slot = 444;
        let result =
            verify_slot_deltas_with_history(&slots_from_slot_deltas, &slot_history, bank_slot);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn test_verify_slot_deltas_with_history_bad_slot_history() {
        let bank_slot = 444;
        let result = verify_slot_deltas_with_history(
            &HashSet::default(),
            &SlotHistory::default(), // <-- will only have an entry for slot 0
            bank_slot,
        );
        assert_eq!(result, Err(VerifySlotDeltasError::BadSlotHistory));
    }

    #[test]
    fn test_verify_slot_deltas_with_history_bad_slot_not_in_history() {
        let slots_from_slot_deltas = HashSet::from([
            0, // slot history has slot 0 added by default
            444, 222,
        ]);
        let mut slot_history = SlotHistory::default();
        slot_history.add(444); // <-- slot history is missing slot 222

        let bank_slot = 444;
        let result =
            verify_slot_deltas_with_history(&slots_from_slot_deltas, &slot_history, bank_slot);

        assert_eq!(
            result,
            Err(VerifySlotDeltasError::SlotNotFoundInHistory(222)),
        );
    }

    #[test]
    fn test_verify_slot_deltas_with_history_bad_slot_not_in_deltas() {
        let slots_from_slot_deltas = HashSet::from([
            0, // slot history has slot 0 added by default
            444, 222,
            // <-- slot deltas is missing slot 333
        ]);
        let mut slot_history = SlotHistory::default();
        slot_history.add(222);
        slot_history.add(333);
        slot_history.add(444);

        let bank_slot = 444;
        let result =
            verify_slot_deltas_with_history(&slots_from_slot_deltas, &slot_history, bank_slot);

        assert_eq!(
            result,
            Err(VerifySlotDeltasError::SlotNotFoundInDeltas(333)),
        );
    }

    #[test]
    fn test_verify_epoch_stakes_good() {
        let bank = create_simple_test_bank(100 * LAMPORTS_PER_SOL);
        assert_eq!(verify_epoch_stakes(&bank), Ok(()));
    }

    #[test]
    fn test_verify_epoch_stakes_bad() {
        let bank = create_simple_test_bank(100 * LAMPORTS_PER_SOL);
        let current_epoch = bank.epoch();
        let leader_schedule_epoch = bank.get_leader_schedule_epoch(bank.slot());
        let required_epochs = current_epoch..=leader_schedule_epoch;

        // insert an invalid epoch into the epoch stakes
        {
            let mut epoch_stakes_map = bank.epoch_stakes_map().clone();
            let invalid_epoch = *required_epochs.end() + 1;
            epoch_stakes_map.insert(
                invalid_epoch,
                bank.epoch_stakes(bank.epoch()).cloned().unwrap(),
            );

            assert_eq!(
                _verify_epoch_stakes(&epoch_stakes_map, required_epochs.clone()),
                Err(VerifyEpochStakesError::EpochGreaterThanMax(
                    invalid_epoch,
                    *required_epochs.end(),
                )),
            );
        }

        // remove required stakes
        {
            for removed_epoch in required_epochs.clone() {
                let mut epoch_stakes_map = bank.epoch_stakes_map().clone();
                let removed_stakes = epoch_stakes_map.remove(&removed_epoch);
                assert!(removed_stakes.is_some());

                assert_eq!(
                    _verify_epoch_stakes(&epoch_stakes_map, required_epochs.clone()),
                    Err(VerifyEpochStakesError::StakesNotFound(
                        removed_epoch,
                        required_epochs.clone(),
                    )),
                );
            }
        }
    }

    #[test]
    fn test_get_highest_loadable_bank_snapshot() {
        let bank_snapshots_dir = TempDir::new().unwrap();
        let snapshot_archives_dir = TempDir::new().unwrap();

        let snapshot_config = SnapshotConfig {
            bank_snapshots_dir: bank_snapshots_dir.as_ref().to_path_buf(),
            full_snapshot_archives_dir: snapshot_archives_dir.as_ref().to_path_buf(),
            incremental_snapshot_archives_dir: snapshot_archives_dir.as_ref().to_path_buf(),
            ..Default::default()
        };
        let load_only_snapshot_config = SnapshotConfig {
            bank_snapshots_dir: snapshot_config.bank_snapshots_dir.clone(),
            full_snapshot_archives_dir: snapshot_config.full_snapshot_archives_dir.clone(),
            incremental_snapshot_archives_dir: snapshot_config
                .incremental_snapshot_archives_dir
                .clone(),
            ..SnapshotConfig::new_load_only()
        };

        let genesis_config = GenesisConfig::default();
        let mut bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let mut full_snapshot_archive_info = None;

        // take some snapshots, and archive them
        // note the `+1` at the end; we'll turn it into a PRE afterwards
        for _ in 0..snapshot_config
            .maximum_full_snapshot_archives_to_retain
            .get()
            + 1
        {
            let slot = bank.slot() + 1;
            bank = Arc::new(Bank::new_from_parent(bank, &Pubkey::default(), slot));
            bank.fill_bank_with_ticks_for_tests();
            full_snapshot_archive_info = Some(
                bank_to_full_snapshot_archive_with(
                    &snapshot_config.bank_snapshots_dir,
                    &bank,
                    snapshot_config.snapshot_version,
                    &snapshot_config.full_snapshot_archives_dir,
                    &snapshot_config.incremental_snapshot_archives_dir,
                    snapshot_config.archive_format,
                    false,
                )
                .unwrap(),
            );
        }

        // As a hack, to make a PRE bank snapshot, just rename the last POST one.
        let slot = bank.slot();
        let bank_snapshot_dir = get_bank_snapshot_dir(&bank_snapshots_dir, slot);
        let post = bank_snapshot_dir.join(get_snapshot_file_name(slot));
        let pre = post.with_extension(BANK_SNAPSHOT_PRE_FILENAME_EXTENSION);
        fs::rename(post, pre).unwrap();

        // ...and we also need to delete the last snapshot archive
        fs::remove_file(full_snapshot_archive_info.unwrap().path()).unwrap();

        let highest_full_snapshot_archive =
            get_highest_full_snapshot_archive_info(&snapshot_archives_dir).unwrap();
        let highest_bank_snapshot_post =
            get_highest_bank_snapshot_post(&bank_snapshots_dir).unwrap();
        let highest_bank_snapshot_pre = get_highest_bank_snapshot_pre(&bank_snapshots_dir).unwrap();

        // we want a bank snapshot PRE with the highest slot to ensure get_highest_loadable()
        // correctly skips bank snapshots PRE
        assert!(highest_bank_snapshot_pre.slot > highest_bank_snapshot_post.slot);

        // 1. call get_highest_loadable() but bad snapshot dir, so returns None
        assert!(get_highest_loadable_bank_snapshot(&SnapshotConfig::default()).is_none());

        // 2. the 'storages flushed' file hasn't been written yet, so get_highest_loadable() should return NONE
        assert!(get_highest_loadable_bank_snapshot(&snapshot_config).is_none());

        // 3. write 'storages flushed' file, get_highest_loadable(), should return highest_bank_snapshot_post_slot
        snapshot_utils::write_storages_flushed_file(&highest_bank_snapshot_post.snapshot_dir)
            .unwrap();
        let bank_snapshot = get_highest_loadable_bank_snapshot(&snapshot_config).unwrap();
        assert_eq!(bank_snapshot, highest_bank_snapshot_post);

        // 4. delete highest full snapshot archive, get_highest_loadable() should return NONE
        fs::remove_file(highest_full_snapshot_archive.path()).unwrap();
        assert!(get_highest_loadable_bank_snapshot(&snapshot_config).is_none());

        // 5. get_highest_loadable(), but with a load-only snapshot config, should return Some()
        let bank_snapshot = get_highest_loadable_bank_snapshot(&load_only_snapshot_config).unwrap();
        assert_eq!(bank_snapshot, highest_bank_snapshot_post);

        // 6. delete highest bank snapshot, get_highest_loadable() should return NONE
        fs::remove_dir_all(&highest_bank_snapshot_post.snapshot_dir).unwrap();
        assert!(get_highest_loadable_bank_snapshot(&snapshot_config).is_none());

        // 7. write 'storages flushed' file, get_highest_loadable() should return Some() again, with slot-1
        snapshot_utils::write_storages_flushed_file(get_bank_snapshot_dir(
            &snapshot_config.bank_snapshots_dir,
            highest_bank_snapshot_post.slot - 1,
        ))
        .unwrap();
        let bank_snapshot = get_highest_loadable_bank_snapshot(&snapshot_config).unwrap();
        assert_eq!(bank_snapshot.slot, highest_bank_snapshot_post.slot - 1);

        // 8. delete the full snapshot slot file, get_highest_loadable() should return NONE
        fs::remove_file(
            bank_snapshot
                .snapshot_dir
                .join(SNAPSHOT_FULL_SNAPSHOT_SLOT_FILENAME),
        )
        .unwrap();
        assert!(get_highest_loadable_bank_snapshot(&snapshot_config).is_none());

        // 9. however, a load-only snapshot config should return Some() again
        let bank_snapshot2 =
            get_highest_loadable_bank_snapshot(&load_only_snapshot_config).unwrap();
        assert_eq!(bank_snapshot2, bank_snapshot);
    }
}
