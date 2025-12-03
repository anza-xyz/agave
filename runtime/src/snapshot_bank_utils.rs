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
    tempfile::TempDir,
};
use {
    crate::{
        bank::{Bank, BankSlotDelta},
        epoch_stakes::VersionedEpochStakes,
        runtime_config::RuntimeConfig,
        serde_snapshot::{
            self, reconstruct_bank_from_fields, SnapshotAccountsDbFields, SnapshotBankFields,
        },
        snapshot_package::SnapshotPackage,
        snapshot_utils::{
            self, rebuild_storages_from_snapshot_dir, verify_and_unarchive_snapshots,
            BankSnapshotInfo, StorageAndNextAccountsFileId, UnarchivedSnapshots,
        },
        status_cache,
    },
    agave_fs::dirs,
    agave_snapshots::{
        error::{
            SnapshotError, VerifyEpochStakesError, VerifySlotDeltasError, VerifySlotHistoryError,
        },
        paths::{
            self as snapshot_paths, get_highest_full_snapshot_archive_info,
            get_highest_incremental_snapshot_archive_info,
        },
        snapshot_archive_info::{
            FullSnapshotArchiveInfo, IncrementalSnapshotArchiveInfo, SnapshotArchiveInfoGetter,
        },
        snapshot_config::SnapshotConfig,
        snapshot_hash::SnapshotHash,
        ArchiveFormat, SnapshotArchiveKind, SnapshotKind, SnapshotVersion,
    },
    log::*,
    solana_accounts_db::{
        accounts_db::{AccountsDbConfig, AtomicAccountsFileId},
        accounts_update_notifier_interface::AccountsUpdateNotifier,
    },
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

/// Parses out bank specific information from a snapshot archive including the leader schedule.
/// epoch schedule, etc.
#[cfg(feature = "dev-context-only-utils")]
pub fn bank_fields_from_snapshot_archives(
    full_snapshot_archives_dir: impl AsRef<Path>,
    incremental_snapshot_archives_dir: impl AsRef<Path>,
    accounts_db_config: &AccountsDbConfig,
) -> agave_snapshots::Result<BankFieldsToDeserialize> {
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
        accounts_db_config,
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
) -> agave_snapshots::Result<BankFieldsToDeserialize> {
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
    limit_load_slot_count_from_snapshot: Option<usize>,
    accounts_db_skip_shrink: bool,
    accounts_db_force_initial_clean: bool,
    verify_index: bool,
    accounts_db_config: AccountsDbConfig,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> agave_snapshots::Result<Bank> {
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
        &accounts_db_config,
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
        limit_load_slot_count_from_snapshot,
        verify_index,
        accounts_db_config,
        accounts_update_notifier,
        exit,
    )?;
    measure_rebuild.stop();
    info!("{measure_rebuild}");

    verify_epoch_stakes(&bank)?;

    // The status cache is rebuilt from the latest snapshot.  So, if there's an incremental
    // snapshot, use that.  Otherwise use the full snapshot.
    let status_cache_path = incremental_unpacked_snapshots_dir_and_version
        .as_ref()
        .unwrap_or(&full_unpacked_snapshots_dir_and_version)
        .unpacked_snapshots_dir
        .join(snapshot_paths::SNAPSHOT_STATUS_CACHE_FILENAME);
    info!(
        "Rebuilding status cache from {}",
        status_cache_path.display()
    );
    let slot_deltas = serde_snapshot::deserialize_status_cache(&status_cache_path)?;

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
        Some(&info.calculated_accounts_lt_hash),
    ) && limit_load_slot_count_from_snapshot.is_none()
    {
        panic!("Snapshot bank for slot {} failed to verify", bank.slot());
    }
    measure_verify.stop();

    datapoint_info!(
        "bank_from_snapshot_archives",
        (
            "untar_full_snapshot_archive_us",
            full_measure_untar.as_us(),
            i64
        ),
        (
            "untar_incremental_snapshot_archive_us",
            incremental_measure_untar.as_ref().map(Measure::as_us),
            Option<i64>
        ),
        ("rebuild_bank_us", measure_rebuild.as_us(), i64),
        ("verify_bank_us", measure_verify.as_us(), i64),
    );
    Ok(bank)
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
    limit_load_slot_count_from_snapshot: Option<usize>,
    accounts_db_skip_shrink: bool,
    accounts_db_force_initial_clean: bool,
    verify_index: bool,
    accounts_db_config: AccountsDbConfig,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> agave_snapshots::Result<(
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

    let bank = bank_from_snapshot_archives(
        account_paths,
        bank_snapshots_dir.as_ref(),
        &full_snapshot_archive_info,
        incremental_snapshot_archive_info.as_ref(),
        genesis_config,
        runtime_config,
        debug_keys,
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
    limit_load_slot_count_from_snapshot: Option<usize>,
    verify_index: bool,
    accounts_db_config: AccountsDbConfig,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> agave_snapshots::Result<Bank> {
    info!(
        "Loading bank from snapshot dir: {}",
        bank_snapshot.snapshot_dir.display()
    );

    // Clear the contents of the account paths run directories.  When constructing the bank, the appendvec
    // files will be extracted from the snapshot hardlink directories into these run/ directories.
    for path in account_paths {
        dirs::remove_dir_contents(path);
    }

    let next_append_vec_id = Arc::new(AtomicAccountsFileId::new(0));

    let ((storage, bank_fields, accounts_db_fields), measure_rebuild_storages) = measure_time!(
        rebuild_storages_from_snapshot_dir(
            bank_snapshot,
            account_paths,
            next_append_vec_id.clone(),
            accounts_db_config.storage_access,
        )?,
        "rebuild storages from snapshot dir"
    );
    info!("{measure_rebuild_storages}");

    let next_append_vec_id =
        Arc::try_unwrap(next_append_vec_id).expect("this is the only strong reference");
    let storage_and_next_append_vec_id = StorageAndNextAccountsFileId {
        storage,
        next_append_vec_id,
    };
    let snapshot_bank_fields = SnapshotBankFields::new(bank_fields, None);
    let snapshot_accounts_db_fields = SnapshotAccountsDbFields::new(accounts_db_fields, None);
    let ((bank, info), measure_rebuild_bank) = measure_time!(
        reconstruct_bank_from_fields(
            snapshot_bank_fields,
            snapshot_accounts_db_fields,
            genesis_config,
            runtime_config,
            account_paths,
            storage_and_next_append_vec_id,
            debug_keys,
            limit_load_slot_count_from_snapshot,
            verify_index,
            accounts_db_config,
            accounts_update_notifier,
            exit,
        )?,
        "rebuild bank from snapshot"
    );
    info!("{measure_rebuild_bank}");

    verify_epoch_stakes(&bank)?;

    let status_cache_path = bank_snapshot
        .snapshot_dir
        .join(snapshot_paths::SNAPSHOT_STATUS_CACHE_FILENAME);
    info!(
        "Rebuilding status cache from {}",
        status_cache_path.display()
    );
    let slot_deltas = serde_snapshot::deserialize_status_cache(&status_cache_path)?;

    verify_slot_deltas(slot_deltas.as_slice(), &bank)?;

    bank.status_cache.write().unwrap().append(&slot_deltas);

    if !bank.verify_snapshot_bank(
        true,
        false,
        0, // since force_clean is false, this value is unused
        Some(&info.calculated_accounts_lt_hash),
    ) && limit_load_slot_count_from_snapshot.is_none()
    {
        panic!("Snapshot bank for slot {} failed to verify", bank.slot());
    }

    datapoint_info!(
        "bank_from_snapshot_dir",
        ("rebuild_storages_us", measure_rebuild_storages.as_us(), i64),
        ("rebuild_bank_us", measure_rebuild_bank.as_us(), i64),
    );
    Ok(bank)
}

/// Verifies the snapshot's slot and hash matches the bank's
fn verify_bank_against_expected_slot_hash(
    bank: &Bank,
    snapshot_slot: Slot,
    snapshot_hash: SnapshotHash,
) -> agave_snapshots::Result<()> {
    let bank_slot = bank.slot();
    if bank_slot != snapshot_slot {
        return Err(SnapshotError::MismatchedSlot(bank_slot, snapshot_slot));
    }

    let bank_hash = bank.get_snapshot_hash();
    if bank_hash == snapshot_hash {
        Ok(())
    } else {
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
) -> agave_snapshots::Result<(SnapshotVersion, SnapshotRootPaths)> {
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
    verify_slot_history(slot_history, bank_slot)?;

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

/// Verify that the snapshot's SlotHistory is not corrupt/invalid
fn verify_slot_history(
    slot_history: &SlotHistory,
    bank_slot: Slot,
) -> Result<(), VerifySlotHistoryError> {
    if slot_history.newest() != bank_slot {
        return Err(VerifySlotHistoryError::InvalidNewestSlot);
    }

    if slot_history.bits.len() != solana_slot_history::MAX_ENTRIES {
        return Err(VerifySlotHistoryError::InvalidNumEntries);
    }

    Ok(())
}

/// Verifies the bank's epoch stakes are valid after rebuilding from a snapshot
fn verify_epoch_stakes(bank: &Bank) -> std::result::Result<(), VerifyEpochStakesError> {
    // Stakes are required for epochs from the current epoch up-to-and-including the
    // leader schedule epoch.  In practice this will only be two epochs: the current and the next.
    // Using a range mirrors how Bank::new_from_genesis() seeds the initial epoch stakes.
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
    // the snapshot's epoch stakes therefore can have entries for epochs at-or-below the
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
) -> agave_snapshots::Result<FullSnapshotArchiveInfo> {
    let snapshot_version = snapshot_version.unwrap_or_default();
    let bank_snapshots_dir = tempfile::tempdir_in(&bank_snapshots_dir)?;

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

    let snapshot_archive_kind = SnapshotArchiveKind::Full;

    let snapshot_package = SnapshotPackage::new(
        SnapshotKind::Archive(snapshot_archive_kind),
        bank,
        bank.get_snapshot_storages(None),
        bank.status_cache.read().unwrap().root_slot_deltas(),
    );

    let snapshot_config = SnapshotConfig {
        full_snapshot_archives_dir: full_snapshot_archives_dir.as_ref().to_path_buf(),
        incremental_snapshot_archives_dir: incremental_snapshot_archives_dir.as_ref().to_path_buf(),
        bank_snapshots_dir: bank_snapshots_dir.as_ref().to_path_buf(),
        archive_format,
        snapshot_version,
        ..Default::default()
    };

    let snapshot_storages = snapshot_package.snapshot_storages;

    let bank_snapshot_info = snapshot_utils::serialize_snapshot(
        &snapshot_config.bank_snapshots_dir,
        snapshot_config.snapshot_version,
        snapshot_package.bank_snapshot_package,
        snapshot_storages.as_slice(),
        false, // we do not intend to fastboot, so skip flushing and hard linking the storages
    )?;

    let snapshot_archive_info = snapshot_utils::archive_snapshot_package(
        snapshot_archive_kind,
        snapshot_package.slot,
        snapshot_package.hash,
        bank_snapshot_info.snapshot_dir,
        snapshot_storages,
        &snapshot_config,
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
) -> agave_snapshots::Result<IncrementalSnapshotArchiveInfo> {
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

    let snapshot_archive_kind = SnapshotArchiveKind::Incremental(full_snapshot_slot);

    let snapshot_package = SnapshotPackage::new(
        SnapshotKind::Archive(snapshot_archive_kind),
        bank,
        bank.get_snapshot_storages(Some(full_snapshot_slot)),
        bank.status_cache.read().unwrap().root_slot_deltas(),
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

    let snapshot_storages = snapshot_package.snapshot_storages;

    let bank_snapshot_info = snapshot_utils::serialize_snapshot(
        &snapshot_config.bank_snapshots_dir,
        snapshot_config.snapshot_version,
        snapshot_package.bank_snapshot_package,
        snapshot_storages.as_slice(),
        false, // we do not intend to fastboot, so skip flushing and hard linking the storages
    )?;

    let snapshot_archive_info = snapshot_utils::archive_snapshot_package(
        snapshot_archive_kind,
        snapshot_package.slot,
        snapshot_package.hash,
        bank_snapshot_info.snapshot_dir,
        snapshot_storages,
        &snapshot_config,
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
            snapshot_package::BankSnapshotPackage,
            snapshot_utils::{
                clean_orphaned_account_snapshot_dirs, create_tmp_accounts_dir_for_tests,
                get_bank_snapshots, get_highest_bank_snapshot, get_highest_loadable_bank_snapshot,
                purge_all_bank_snapshots, purge_bank_snapshot,
                purge_bank_snapshots_older_than_slot, purge_incomplete_bank_snapshots,
                purge_old_bank_snapshots, purge_old_bank_snapshots_at_startup,
                snapshot_storage_rebuilder::get_slot_and_append_vec_id,
            },
            status_cache::Status,
        },
        agave_snapshots::{error::VerifySlotDeltasError, paths::get_bank_snapshot_dir},
        semver::Version,
        solana_accounts_db::{
            accounts_db::{MarkObsoleteAccounts, ACCOUNTS_DB_CONFIG_FOR_TESTING},
            accounts_file::StorageAccess,
        },
        solana_genesis_config::create_genesis_config,
        solana_keypair::Keypair,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_signer::Signer,
        solana_system_transaction as system_transaction,
        solana_transaction::sanitized::SanitizedTransaction,
        std::{
            fs, slice,
            sync::{atomic::Ordering, Arc},
        },
        test_case::test_case,
    };

    fn create_snapshot_dirs_for_tests(
        genesis_config: &GenesisConfig,
        bank_snapshots_dir: impl AsRef<Path>,
        num_total: usize,
        should_flush_and_hard_link_storages: bool,
    ) -> Bank {
        let mut bank = Arc::new(Bank::new_for_tests(genesis_config));
        for _i in 0..num_total {
            let slot = bank.slot() + 1;
            bank = Arc::new(Bank::new_from_parent(bank, &Pubkey::new_unique(), slot));
            bank.fill_bank_with_ticks_for_tests();

            create_bank_snapshot_from_bank(
                &bank_snapshots_dir,
                &bank,
                SnapshotVersion::default(),
                should_flush_and_hard_link_storages,
            )
            .unwrap();
        }

        Arc::into_inner(bank).unwrap()
    }

    /// Creates a bank snapshot from a bank, regardless of state. The Bank will be frozen during
    /// the process. Passing in should_flush_and_hard_link_storages as true will create a
    /// a fastbootable bank snapshot
    ///
    /// Requires:
    ///     - `bank` is complete
    fn create_bank_snapshot_from_bank(
        bank_snapshots_dir: impl AsRef<Path>,
        bank: &Bank,
        snapshot_version: SnapshotVersion,
        should_flush_and_hard_link_storages: bool,
    ) -> agave_snapshots::Result<()> {
        assert!(bank.is_complete());

        bank.squash(); // Bank may not be a root
        bank.rehash(); // Bank may have been manually modified by the caller
        bank.force_flush_accounts_cache();
        bank.clean_accounts();

        let bank_snapshot_package = BankSnapshotPackage {
            bank_fields: bank.get_fields_to_serialize(),
            bank_hash_stats: bank.get_bank_hash_stats(),
            status_cache_slot_deltas: bank.status_cache.read().unwrap().root_slot_deltas(),
            write_version: bank
                .rc
                .accounts
                .accounts_db
                .write_version
                .load(Ordering::Acquire),
        };

        let snapshot_storages = bank.get_snapshot_storages(None);

        snapshot_utils::serialize_snapshot(
            bank_snapshots_dir.as_ref(),
            snapshot_version,
            bank_snapshot_package,
            snapshot_storages.as_slice(),
            should_flush_and_hard_link_storages,
        )?;

        Ok(())
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

        let roundtrip_bank = bank_from_snapshot_archives(
            &[accounts_dir],
            bank_snapshots_dir.path(),
            &snapshot_archive_info,
            None,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            false,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();
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
        let (genesis_config, mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);

        let bank_test_config = BankTestConfig {
            accounts_db_config: AccountsDbConfig {
                mark_obsolete_accounts: MarkObsoleteAccounts::Enabled,
                ..ACCOUNTS_DB_CONFIG_FOR_TESTING
            },
        };

        let bank = Bank::new_with_config_for_tests(&genesis_config, bank_test_config);

        let (bank0, bank_forks) = Bank::wrap_with_bank_forks_for_tests(bank);
        bank0
            .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key1.pubkey())
            .unwrap();
        bank0
            .transfer(2 * LAMPORTS_PER_SOL, &mint_keypair, &key2.pubkey())
            .unwrap();
        bank0
            .transfer(3 * LAMPORTS_PER_SOL, &mint_keypair, &key3.pubkey())
            .unwrap();
        bank0.fill_bank_with_ticks_for_tests();

        // Force flush the bank to create the account storage entry
        bank0.squash();
        bank0.force_flush_accounts_cache();

        // Create a new slot, and invalidate the account for key1 in slot0
        let slot = 1;
        let bank1 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
        bank1
            .transfer(LAMPORTS_PER_SOL, &key3, &key1.pubkey())
            .unwrap();

        bank1.fill_bank_with_ticks_for_tests();

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

        let roundtrip_bank = bank_from_snapshot_archives(
            &[accounts_dir],
            bank_snapshots_dir.path(),
            &full_snapshot_archive_info,
            None,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            false,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();
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

        let (genesis_config, mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        bank0
            .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key1.pubkey())
            .unwrap();
        bank0
            .transfer(2 * LAMPORTS_PER_SOL, &mint_keypair, &key2.pubkey())
            .unwrap();
        bank0
            .transfer(3 * LAMPORTS_PER_SOL, &mint_keypair, &key3.pubkey())
            .unwrap();
        bank0.fill_bank_with_ticks_for_tests();

        let slot = 1;
        let bank1 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
        bank1
            .transfer(3 * LAMPORTS_PER_SOL, &mint_keypair, &key3.pubkey())
            .unwrap();
        bank1
            .transfer(4 * LAMPORTS_PER_SOL, &mint_keypair, &key4.pubkey())
            .unwrap();
        bank1
            .transfer(5 * LAMPORTS_PER_SOL, &mint_keypair, &key5.pubkey())
            .unwrap();
        bank1.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank2 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank1, &collector, slot);
        bank2
            .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key1.pubkey())
            .unwrap();
        bank2.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank3 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank2, &collector, slot);
        bank3
            .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key1.pubkey())
            .unwrap();
        bank3.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank4 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank3, &collector, slot);
        bank4
            .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key1.pubkey())
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

        let roundtrip_bank = bank_from_snapshot_archives(
            &[accounts_dir],
            bank_snapshots_dir.path(),
            &full_snapshot_archive_info,
            None,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            false,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();
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

        let (genesis_config, mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        bank0
            .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key1.pubkey())
            .unwrap();
        bank0
            .transfer(2 * LAMPORTS_PER_SOL, &mint_keypair, &key2.pubkey())
            .unwrap();
        bank0
            .transfer(3 * LAMPORTS_PER_SOL, &mint_keypair, &key3.pubkey())
            .unwrap();
        bank0.fill_bank_with_ticks_for_tests();

        let slot = 1;
        let bank1 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
        bank1
            .transfer(3 * LAMPORTS_PER_SOL, &mint_keypair, &key3.pubkey())
            .unwrap();
        bank1
            .transfer(4 * LAMPORTS_PER_SOL, &mint_keypair, &key4.pubkey())
            .unwrap();
        bank1
            .transfer(5 * LAMPORTS_PER_SOL, &mint_keypair, &key5.pubkey())
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
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank1, &collector, slot);
        bank2
            .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key1.pubkey())
            .unwrap();
        bank2.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank3 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank2, &collector, slot);
        bank3
            .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key1.pubkey())
            .unwrap();
        bank3.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank4 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank3, &collector, slot);
        bank4
            .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key1.pubkey())
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

        let roundtrip_bank = bank_from_snapshot_archives(
            &[accounts_dir],
            bank_snapshots_dir.path(),
            &full_snapshot_archive_info,
            Some(&incremental_snapshot_archive_info),
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            false,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();
        assert_eq!(*bank4, roundtrip_bank);
    }

    /// Test rebuilding bank from the latest snapshot archives
    #[test]
    fn test_bank_from_latest_snapshot_archives() {
        let collector = Pubkey::new_unique();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();

        let (genesis_config, mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        bank0
            .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key1.pubkey())
            .unwrap();
        bank0
            .transfer(2 * LAMPORTS_PER_SOL, &mint_keypair, &key2.pubkey())
            .unwrap();
        bank0
            .transfer(3 * LAMPORTS_PER_SOL, &mint_keypair, &key3.pubkey())
            .unwrap();
        bank0.fill_bank_with_ticks_for_tests();

        let slot = 1;
        let bank1 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
        bank1
            .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key1.pubkey())
            .unwrap();
        bank1
            .transfer(2 * LAMPORTS_PER_SOL, &mint_keypair, &key2.pubkey())
            .unwrap();
        bank1
            .transfer(3 * LAMPORTS_PER_SOL, &mint_keypair, &key3.pubkey())
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
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank1, &collector, slot);
        bank2
            .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key1.pubkey())
            .unwrap();
        bank2.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank3 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank2, &collector, slot);
        bank3
            .transfer(2 * LAMPORTS_PER_SOL, &mint_keypair, &key2.pubkey())
            .unwrap();
        bank3.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank4 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank3, &collector, slot);
        bank4
            .transfer(3 * LAMPORTS_PER_SOL, &mint_keypair, &key3.pubkey())
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
            false,
            false,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();
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

        let (mut genesis_config, mint_keypair) =
            create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        // test expects 0 transaction fee
        genesis_config.fee_rate_governor = solana_fee_calculator::FeeRateGovernor::new(0, 0);

        let lamports_to_transfer = 123_456 * LAMPORTS_PER_SOL;
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
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
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
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank1, &collector, slot);
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
        let deserialized_bank = bank_from_snapshot_archives(
            slice::from_ref(&accounts_dir),
            bank_snapshots_dir.path(),
            &full_snapshot_archive_info,
            Some(&incremental_snapshot_archive_info),
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            false,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();
        assert_eq!(
            deserialized_bank, *bank2,
            "Ensure rebuilding from an incremental snapshot works"
        );

        let slot = slot + 1;
        let bank3 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank2, &collector, slot);
        // Update Account2 so that it no longer holds a reference to slot2
        bank3
            .transfer(lamports_to_transfer, &mint_keypair, &key2.pubkey())
            .unwrap();
        bank3.fill_bank_with_ticks_for_tests();

        let slot = slot + 1;
        let bank4 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank3, &collector, slot);
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

        let deserialized_bank = bank_from_snapshot_archives(
            &[accounts_dir],
            bank_snapshots_dir.path(),
            &full_snapshot_archive_info,
            Some(&incremental_snapshot_archive_info),
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            false,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();
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

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_bank_fields_from_snapshot(storage_access: StorageAccess) {
        let collector = Pubkey::new_unique();
        let key1 = Keypair::new();

        let (genesis_config, mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        bank0.fill_bank_with_ticks_for_tests();

        let slot = 1;
        let bank1 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
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
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank1, &collector, slot);
        bank2
            .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key1.pubkey())
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
            &AccountsDbConfig {
                storage_access,
                ..ACCOUNTS_DB_CONFIG_FOR_TESTING
            },
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
        create_bank_snapshot_from_bank(
            &bank_snapshots_dir,
            &bank,
            SnapshotVersion::default(),
            true,
        )
        .unwrap();

        let accounts_hardlinks_dir = get_bank_snapshot_dir(&bank_snapshots_dir, bank.slot())
            .join(snapshot_paths::SNAPSHOT_ACCOUNTS_HARDLINKS);
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

    /// Test versioning when fastbooting
    /// If the storages flushed file is present, fastboot should always pass
    /// If only the fastboot version file is present, the version should be checked for compatibility
    #[test]
    fn test_fastboot_versioning() {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let _bank = create_snapshot_dirs_for_tests(&genesis_config, &bank_snapshots_dir, 3, true);

        let snapshot_config = SnapshotConfig {
            bank_snapshots_dir: bank_snapshots_dir.as_ref().to_path_buf(),
            full_snapshot_archives_dir: bank_snapshots_dir.as_ref().to_path_buf(),
            incremental_snapshot_archives_dir: bank_snapshots_dir.as_ref().to_path_buf(),
            ..Default::default()
        };

        // Verify the snapshot is found with all files present
        let snapshot = get_highest_loadable_bank_snapshot(&snapshot_config).unwrap();
        assert_eq!(snapshot.slot, 3);

        // Test 1: Modify the version in the fastboot version file to something newer
        // than current
        let complete_flag_file = snapshot
            .snapshot_dir
            .join(snapshot_paths::SNAPSHOT_FASTBOOT_VERSION_FILENAME);
        let version = fs::read_to_string(&complete_flag_file).unwrap();
        let version = Version::parse(&version).unwrap();
        let new_version = Version::new(version.major + 1, version.minor, version.patch);

        fs::write(&complete_flag_file, new_version.to_string()).unwrap();

        // With an invalid version, the snapshot will be considered invalid
        let new_snapshot = get_highest_loadable_bank_snapshot(&snapshot_config);
        assert!(new_snapshot.is_none());

        // Test 2: Remove the bank snapshot version file
        let complete_flag_file = snapshot
            .snapshot_dir
            .join(snapshot_paths::SNAPSHOT_VERSION_FILENAME);
        fs::remove_file(complete_flag_file).unwrap();

        // This will now find the previous entry in the directory, which is slot 2
        let snapshot = get_highest_loadable_bank_snapshot(&snapshot_config).unwrap();
        assert_eq!(snapshot.slot, 2);

        // Test 3: Remove the fastboot version file
        let fastboot_version_file = snapshot
            .snapshot_dir
            .join(snapshot_paths::SNAPSHOT_FASTBOOT_VERSION_FILENAME);
        fs::remove_file(fastboot_version_file).unwrap();

        // The fastboot file is not found, no loadable snapshot is found
        let snapshot = get_highest_loadable_bank_snapshot(&snapshot_config);
        assert!(snapshot.is_none());
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
            should_flush_and_hard_link_storages,
        );

        let snapshot = get_highest_bank_snapshot(&bank_snapshots_dir).unwrap();
        assert_eq!(snapshot.slot, 4);

        let version_file = snapshot
            .snapshot_dir
            .join(snapshot_paths::SNAPSHOT_VERSION_FILENAME);
        fs::remove_file(version_file).unwrap();
        // The incomplete snapshot dir should still exist
        let snapshot_dir_4 = snapshot.snapshot_dir;
        assert!(snapshot_dir_4.exists());
        let snapshot = get_highest_bank_snapshot(&bank_snapshots_dir).unwrap();
        assert_eq!(snapshot.slot, 3);

        let snapshot_version_file = snapshot
            .snapshot_dir
            .join(snapshot_paths::SNAPSHOT_VERSION_FILENAME);
        fs::remove_file(snapshot_version_file).unwrap();
        let snapshot = get_highest_bank_snapshot(&bank_snapshots_dir).unwrap();
        assert_eq!(snapshot.slot, 2);

        let status_cache_file = snapshot
            .snapshot_dir
            .join(snapshot_paths::SNAPSHOT_STATUS_CACHE_FILENAME);
        fs::remove_file(status_cache_file).unwrap();
        let snapshot = get_highest_bank_snapshot(&bank_snapshots_dir).unwrap();
        assert_eq!(snapshot.slot, 1);
    }

    #[test]
    fn test_clean_orphaned_account_snapshot_dirs() {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let _bank = create_snapshot_dirs_for_tests(&genesis_config, &bank_snapshots_dir, 2, true);

        let snapshot_dir_slot_2 = bank_snapshots_dir.path().join("2");
        let accounts_link_dir_slot_2 =
            snapshot_dir_slot_2.join(snapshot_paths::SNAPSHOT_ACCOUNTS_HARDLINKS);

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
        let _bank = create_snapshot_dirs_for_tests(&genesis_config, &bank_snapshots_dir, 2, false);

        // Ensure the bank snapshot dir does exist.
        let bank_snapshot_dir = get_bank_snapshot_dir(&bank_snapshots_dir, 2);
        assert!(fs::exists(&bank_snapshot_dir).unwrap());

        // Ensure the accounts hard links dir does *not* exist for this bank snapshot
        // (since we asked create_snapshot_dirs_for_tests() to *not* hard link).
        let bank_snapshot_accounts_hard_link_dir =
            bank_snapshot_dir.join(snapshot_paths::SNAPSHOT_ACCOUNTS_HARDLINKS);
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
            should_flush_and_hard_link_storages,
        );

        // remove the "version" files so the snapshots will be purged
        for slot in [1, 2] {
            let bank_snapshot_dir = get_bank_snapshot_dir(&bank_snapshots_dir, slot);
            let version_file = bank_snapshot_dir.join(snapshot_paths::SNAPSHOT_VERSION_FILENAME);
            fs::remove_file(version_file).unwrap();
        }

        purge_incomplete_bank_snapshots(&bank_snapshots_dir);

        // ensure the bank snapshots dirs are gone
        for slot in [1, 2] {
            let bank_snapshot_dir = get_bank_snapshot_dir(&bank_snapshots_dir, slot);
            assert!(!bank_snapshot_dir.exists());
        }
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
    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_snapshots_handle_zero_lamport_accounts(storage_access: StorageAccess) {
        let collector = Pubkey::new_unique();
        let key1 = Keypair::new();
        let key2 = Keypair::new();
        let key3 = Keypair::new();

        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let full_snapshot_archives_dir = tempfile::TempDir::new().unwrap();
        let snapshot_archive_format = SnapshotConfig::default().archive_format;

        let (genesis_config, mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);

        let lamports_to_transfer = 123_456 * LAMPORTS_PER_SOL;
        let bank_test_config = BankTestConfig {
            accounts_db_config: AccountsDbConfig {
                storage_access,
                ..ACCOUNTS_DB_CONFIG_FOR_TESTING
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
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
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
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank1, &collector, slot);
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
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank2, &collector, slot);
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
        let deserialized_bank = bank_from_snapshot_archives(
            &[accounts_dir.path().to_path_buf()],
            other_bank_snapshots_dir.path(),
            &full_snapshot_archive_info,
            None,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            false,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();

        assert!(
            deserialized_bank
                .get_account_modified_slot(&key1.pubkey())
                .is_none(),
            "Ensure Account1 has not been brought back from the dead"
        );

        assert_eq!(*bank3, deserialized_bank);
    }

    /// Test that fastboot correctly handles zero lamport accounts
    ///
    /// slot 0:
    ///     - send some lamports to Account2 (from mint) to bring it to life
    ///     - send some lamports to Account1 (from mint) to bring it to life
    ///
    /// slot 1:
    ///     - send all lamports to account2 from account1 to make account1 zero lamport
    ///
    /// slot 2:
    ///     - send all lamports from account2 to the mint to make account2 zero lamport
    ///
    /// If zero lamport accounts are not handled correctly, Account1 or Account2 will come back
    /// failing the test
    #[test_case(MarkObsoleteAccounts::Disabled)]
    #[test_case(MarkObsoleteAccounts::Enabled)]
    fn test_fastboot_handle_zero_lamport_accounts(mark_obsolete_accounts: MarkObsoleteAccounts) {
        let collector = Pubkey::new_unique();
        let key1 = Keypair::new();
        let key2 = Keypair::new();

        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let (mut genesis_config, mint) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);

        // Disable fees so fees don't need to be calculated
        genesis_config.fee_rate_governor = solana_fee_calculator::FeeRateGovernor::new(0, 0);

        let lamports = 123_456 * LAMPORTS_PER_SOL;
        let bank_test_config = BankTestConfig {
            accounts_db_config: AccountsDbConfig {
                mark_obsolete_accounts,
                ..ACCOUNTS_DB_CONFIG_FOR_TESTING
            },
        };

        let bank0 = Bank::new_with_config_for_tests(&genesis_config, bank_test_config);
        let (bank0, bank_forks) = Bank::wrap_with_bank_forks_for_tests(bank0);
        bank0.transfer(lamports, &mint, &key2.pubkey()).unwrap();
        bank0.transfer(lamports, &mint, &key1.pubkey()).unwrap();
        bank0.fill_bank_with_ticks_for_tests();

        // Squash and flush bank0 to ensure slot0 data is not cleaned before being written to storage
        bank0.squash();
        bank0.force_flush_accounts_cache();

        // In slot 1 transfer from key1 to key2, such that key1 becomes zero lamport
        let slot = 1;
        let bank1 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
        bank1.transfer(lamports, &key1, &key2.pubkey()).unwrap();
        assert_eq!(bank1.get_balance(&key1.pubkey()), 0,);
        bank1.fill_bank_with_ticks_for_tests();

        // In slot 2 transfer into key2 to mint such that key2 becomes zero lamport
        let slot = slot + 1;
        let bank2 =
            Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank1, &collector, slot);
        bank2.transfer(lamports * 2, &key2, &mint.pubkey()).unwrap();
        bank2.fill_bank_with_ticks_for_tests();
        assert_eq!(bank2.get_balance(&key2.pubkey()), 0);

        // Take a bank snapshot, passing `true` for `should_flush_and_hard_link_storages`.
        // This ensures that `serialize_snapshot` performs all necessary steps to create
        // a snapshot that supports fastbooting.
        create_bank_snapshot_from_bank(
            &bank_snapshots_dir,
            &bank2,
            SnapshotVersion::default(),
            true,
        )
        .unwrap();

        let account_paths = &bank2.rc.accounts.accounts_db.paths;
        let bank_snapshot = get_highest_bank_snapshot(&bank_snapshots_dir).unwrap();

        let deserialized_bank = bank_from_snapshot_dir(
            account_paths,
            &bank_snapshot,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();

        // Ensure both accounts are still zero lamport
        assert_eq!(deserialized_bank.get_balance(&key1.pubkey()), 0);
        assert_eq!(deserialized_bank.get_balance(&key2.pubkey()), 0);

        // Ensure the deserialized bank matches the original bank
        assert_eq!(*bank2, deserialized_bank);
    }

    /// Test that removing the obsolete accounts file causes fastboot to fail.
    /// Fastboot requires obsolete accounts files as of Version 2.0.0.
    #[test]
    #[should_panic(expected = "failed to read obsolete accounts file")]
    fn test_fastboot_missing_obsolete_accounts() {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let bank = create_snapshot_dirs_for_tests(&genesis_config, &bank_snapshots_dir, 3, true);

        let account_paths = &bank.rc.accounts.accounts_db.paths;
        let bank_snapshot = get_highest_bank_snapshot(&bank_snapshots_dir).unwrap();

        // Remove the obsolete account file
        let obsolete_accounts_file = bank_snapshot
            .snapshot_dir
            .join(snapshot_paths::SNAPSHOT_OBSOLETE_ACCOUNTS_FILENAME);
        fs::remove_file(obsolete_accounts_file).unwrap();

        bank_from_snapshot_dir(
            account_paths,
            &bank_snapshot,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            None,
            Arc::default(),
        )
        .unwrap();
    }

    #[test_case(#[allow(deprecated)] StorageAccess::Mmap)]
    #[test_case(StorageAccess::File)]
    fn test_bank_from_snapshot_dir(storage_access: StorageAccess) {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let bank = Bank::new_for_tests(&genesis_config);
        bank.fill_bank_with_ticks_for_tests();

        // Take a bank snapshot, passing `true` for `should_flush_and_hard_link_storages`.
        // This ensures that `serialize_snapshot` performs all necessary steps to create
        // a snapshot that supports fastbooting.
        create_bank_snapshot_from_bank(
            &bank_snapshots_dir,
            &bank,
            SnapshotVersion::default(),
            true,
        )
        .unwrap();

        let bank_snapshot = get_highest_bank_snapshot(&bank_snapshots_dir).unwrap();
        let account_paths = &bank.rc.accounts.accounts_db.paths;

        let bank_constructed = bank_from_snapshot_dir(
            account_paths,
            &bank_snapshot,
            &genesis_config,
            &RuntimeConfig::default(),
            None,
            None,
            false,
            AccountsDbConfig {
                storage_access,
                ..ACCOUNTS_DB_CONFIG_FOR_TESTING
            },
            None,
            Arc::default(),
        )
        .unwrap();
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

    #[test_case(false)]
    #[test_case(true)]
    fn test_purge_all_bank_snapshots(should_flush_and_hard_link_storages: bool) {
        let genesis_config = GenesisConfig::default();
        let bank_snapshots_dir = tempfile::TempDir::new().unwrap();
        let _bank = create_snapshot_dirs_for_tests(
            &genesis_config,
            &bank_snapshots_dir,
            10,
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
            should_flush_and_hard_link_storages,
        );
        // Keep bank in this scope so that its account_paths tmp dirs are not released, and purge_old_bank_snapshots
        // can clear the account hardlinks correctly.

        assert_eq!(get_bank_snapshots(&bank_snapshots_dir).len(), 10);

        purge_old_bank_snapshots(&bank_snapshots_dir, 2);

        assert_eq!(get_bank_snapshots(&bank_snapshots_dir).len(), 2);

        purge_old_bank_snapshots(&bank_snapshots_dir, 2);
        assert_eq!(get_bank_snapshots(&bank_snapshots_dir).len(), 2);

        purge_old_bank_snapshots(&bank_snapshots_dir, 0);
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
            should_flush_and_hard_link_storages,
        );

        purge_old_bank_snapshots_at_startup(&bank_snapshots_dir);

        let bank_snapshots = get_bank_snapshots(&bank_snapshots_dir);
        assert_eq!(bank_snapshots.len(), 1);
        assert_eq!(bank_snapshots.first().unwrap().slot, 9);
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
    fn test_verify_slot_history_good() {
        let mut slot_history = SlotHistory::default();
        // note: slot history expects slots to be added in numeric order
        for slot in [0, 111, 222, 333, 444] {
            slot_history.add(slot);
        }

        let bank_slot = 444;
        let result = verify_slot_history(&slot_history, bank_slot);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn test_verify_slot_history_bad_invalid_newest_slot() {
        let slot_history = SlotHistory::default();
        let bank_slot = 444;
        let result = verify_slot_history(&slot_history, bank_slot);
        assert_eq!(result, Err(VerifySlotHistoryError::InvalidNewestSlot));
    }

    #[test]
    fn test_verify_slot_history_bad_invalid_num_entries() {
        let mut slot_history = SlotHistory::default();
        slot_history.bits.truncate(slot_history.bits.len() - 1);

        let bank_slot = 0;
        let result = verify_slot_history(&slot_history, bank_slot);
        assert_eq!(result, Err(VerifySlotHistoryError::InvalidNumEntries));
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

    #[test_case(SnapshotConfig::new_load_only())]
    #[test_case(SnapshotConfig::default())]
    fn test_get_highest_loadable_bank_snapshot(snapshot_config: SnapshotConfig) {
        let bank_snapshots_dir = TempDir::new().unwrap();
        let snapshot_archives_dir = TempDir::new().unwrap();

        let snapshot_config = SnapshotConfig {
            bank_snapshots_dir: bank_snapshots_dir.as_ref().to_path_buf(),
            full_snapshot_archives_dir: snapshot_archives_dir.as_ref().to_path_buf(),
            incremental_snapshot_archives_dir: snapshot_archives_dir.as_ref().to_path_buf(),
            ..snapshot_config
        };

        let num_snapshots_to_create = snapshot_config
            .maximum_full_snapshot_archives_to_retain
            .get();

        // Take some snapshots. Do not flush or hard link storages so that get highest loadable
        // can be tested when the snapshot has not been marked loadable
        let _bank = create_snapshot_dirs_for_tests(
            &GenesisConfig::default(),
            &snapshot_config.bank_snapshots_dir,
            num_snapshots_to_create,
            false,
        );

        let highest_bank_snapshot = get_highest_bank_snapshot(&bank_snapshots_dir).unwrap();

        // 1. call get_highest_loadable() but bad snapshot dir, so returns None
        assert!(get_highest_loadable_bank_snapshot(&SnapshotConfig::default()).is_none());

        // 2. the bank snapshot has not been marked as loadable, so get_highest_loadable() should return NONE
        assert!(get_highest_loadable_bank_snapshot(&snapshot_config).is_none());

        // 3. Mark the bank snapshot as loadable, get_highest_loadable() should return highest_bank_snapshot_slot
        snapshot_utils::mark_bank_snapshot_as_loadable(&highest_bank_snapshot.snapshot_dir)
            .unwrap();
        let bank_snapshot = get_highest_loadable_bank_snapshot(&snapshot_config).unwrap();
        assert_eq!(bank_snapshot.slot, highest_bank_snapshot.slot);

        // 4. delete highest bank snapshot, get_highest_loadable() should return NONE
        fs::remove_dir_all(&highest_bank_snapshot.snapshot_dir).unwrap();
        assert!(get_highest_loadable_bank_snapshot(&snapshot_config).is_none());

        // 5. Mark the bank snapshot as loadable, get_highest_loadable() should return Some() again, with slot-1
        snapshot_utils::mark_bank_snapshot_as_loadable(get_bank_snapshot_dir(
            &snapshot_config.bank_snapshots_dir,
            highest_bank_snapshot.slot - 1,
        ))
        .unwrap();
        let bank_snapshot = get_highest_loadable_bank_snapshot(&snapshot_config).unwrap();
        assert_eq!(bank_snapshot.slot, highest_bank_snapshot.slot - 1);
    }
}
