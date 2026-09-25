use {
    crate::{
        blockstore::Blockstore,
        blockstore_processor::{self, BlockstoreProcessorError, ProcessOptions},
        entry_notifier_service::EntryNotifierSender,
        use_snapshot_archives_at_startup::{self, UseSnapshotArchivesAtStartup},
    },
    agave_snapshots::{
        error::SnapshotError,
        paths as snapshot_paths,
        snapshot_archive_info::{
            FullSnapshotArchiveInfo, IncrementalSnapshotArchiveInfo, SnapshotArchiveInfoGetter,
        },
        snapshot_config::SnapshotConfig,
        snapshot_hash::{FullSnapshotHash, IncrementalSnapshotHash, StartingSnapshotHashes},
    },
    log::*,
    solana_accounts_db::{
        accounts_update_notifier_interface::AccountsUpdateNotifier,
        utils::move_and_async_delete_path_contents,
    },
    solana_clock::Slot,
    solana_genesis_config::GenesisConfig,
    solana_runtime::{
        bank::Bank, bank_forks::BankForks, snapshot_bank_utils, snapshot_utils,
        transaction_execution::TransactionStatusSender,
    },
    std::{
        collections::HashSet,
        path::{Path, PathBuf},
        sync::{Arc, RwLock, atomic::AtomicBool},
    },
    thiserror::Error,
};

#[derive(Error, Debug)]
pub enum BankForksUtilsError {
    #[error("accounts path(s) not present when booting from snapshot")]
    AccountPathsNotPresent,

    #[error(
        "failed to load bank: {source}, full snapshot archive: {full_snapshot_archive}, \
         incremental snapshot archive: {incremental_snapshot_archive}"
    )]
    BankFromSnapshotsArchive {
        source: Box<SnapshotError>,
        full_snapshot_archive: String,
        incremental_snapshot_archive: String,
    },

    #[error(
        "there is no local state to startup from. Ensure --{flag} is NOT set to \"{value}\" and \
         restart"
    )]
    NoBankSnapshotDirectory { flag: String, value: String },

    #[error("failed to load bank from snapshot '{path}': {source}")]
    BankFromSnapshotsDirectory {
        source: SnapshotError,
        path: PathBuf,
    },

    #[error("failed to process blockstore from genesis: {0}")]
    ProcessBlockstoreFromGenesis(#[source] BlockstoreProcessorError),

    #[error(
        "hard fork at slot {slot} is already registered on the bank loaded at slot {bank_slot}; \
         registering it again changes the shred version. Drop --hard-fork, or load a snapshot \
         that does not already carry it"
    )]
    HardForkAlreadyRegistered { slot: Slot, bank_slot: Slot },

    #[error(
        "hard fork at slot {slot} cannot be registered on the bank loaded at slot {bank_slot}, so \
         it would be left out of the snapshot entirely"
    )]
    HardForkIgnored { slot: Slot, bank_slot: Slot },
}

pub type BankAndHashes = (Arc<RwLock<BankForks>>, Option<StartingSnapshotHashes>);

/// Tear down state left over from a previous run: purges old bank snapshots, wipes the account
/// run dirs, and wipes the legacy snapshot sibling trees.
///
/// Call from any load path that commits to NOT fastbooting (archive load, genesis boot) —
/// i.e. anywhere the existing storages must not be loaded.
pub fn discard_previous_run_state(bank_snapshots_dir: &Path, account_run_paths: &[PathBuf]) {
    snapshot_utils::purge_all_bank_snapshots(bank_snapshots_dir);
    for account_run_path in account_run_paths {
        move_and_async_delete_path_contents(account_run_path);
    }
    snapshot_utils::wipe_account_snapshot_dirs(account_run_paths);
}

fn register_hard_forks(
    bank: &Bank,
    process_options: &ProcessOptions,
) -> Result<(), BankForksUtilsError> {
    let Some(new_hard_forks) = process_options.new_hard_forks.as_ref() else {
        return Ok(());
    };

    let bank_slot = bank.slot();
    let loaded_hard_forks: HashSet<Slot> =
        bank.hard_forks().iter().map(|(slot, _)| *slot).collect();

    for &slot in new_hard_forks {
        let conflict = if loaded_hard_forks.contains(&slot) {
            Some(BankForksUtilsError::HardForkAlreadyRegistered { slot, bank_slot })
        } else if slot < bank_slot || (slot == bank_slot && bank.is_frozen()) {
            Some(BankForksUtilsError::HardForkIgnored { slot, bank_slot })
        } else {
            None
        };

        if let Some(conflict) = conflict {
            if process_options.fail_on_hard_fork_conflict {
                return Err(conflict);
            }
            // Bank::register_hard_fork already warns about the forks it ignores, but says
            // nothing about the ones it registers a second time.
            if matches!(
                conflict,
                BankForksUtilsError::HardForkAlreadyRegistered { .. }
            ) {
                warn!("{conflict}");
            }
        }
    }

    bank.register_hard_forks(Some(new_hard_forks));

    Ok(())
}

/// Load the banks via genesis
pub fn load_bank_forks_from_genesis(
    genesis_config: &GenesisConfig,
    blockstore: &Blockstore,
    account_paths: Vec<PathBuf>,
    process_options: &ProcessOptions,
    transaction_status_sender: Option<&TransactionStatusSender>,
    entry_notification_sender: Option<&EntryNotifierSender>,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> Result<BankAndHashes, BankForksUtilsError> {
    info!("Processing ledger from genesis");
    let bank_forks = blockstore_processor::process_blockstore_for_bank_0(
        genesis_config,
        blockstore,
        account_paths,
        process_options,
        transaction_status_sender,
        entry_notification_sender,
        accounts_update_notifier,
        exit,
    )
    .map_err(BankForksUtilsError::ProcessBlockstoreFromGenesis)?;

    let root_bank = bank_forks.read().unwrap().root_bank();
    register_hard_forks(&root_bank, process_options)?;

    Ok((bank_forks, None))
}

fn get_snapshots_to_load(
    snapshot_config: &SnapshotConfig,
) -> Option<(
    FullSnapshotArchiveInfo,
    Option<IncrementalSnapshotArchiveInfo>,
)> {
    if !snapshot_config.should_load_snapshots() {
        info!("Snapshots disabled");
        return None;
    };

    let Some(full_snapshot_archive_info) = snapshot_paths::get_highest_full_snapshot_archive_info(
        &snapshot_config.full_snapshot_archives_dir,
    ) else {
        warn!(
            "No snapshot package found in directory: {}",
            snapshot_config.full_snapshot_archives_dir.display()
        );
        return None;
    };

    let incremental_snapshot_archive_info =
        snapshot_paths::get_highest_incremental_snapshot_archive_info(
            &snapshot_config.incremental_snapshot_archives_dir,
            full_snapshot_archive_info.slot(),
        );

    Some((
        full_snapshot_archive_info,
        incremental_snapshot_archive_info,
    ))
}

/// Load the banks via snapshot if snapshots are available, otherwise return `Ok(None)`
pub fn try_load_bank_forks_from_snapshot(
    genesis_config: &GenesisConfig,
    account_paths: &[PathBuf],
    snapshot_config: &SnapshotConfig,
    process_options: &ProcessOptions,
    accounts_update_notifier: Option<AccountsUpdateNotifier>,
    exit: Arc<AtomicBool>,
) -> Result<Option<BankAndHashes>, BankForksUtilsError> {
    let Some((full_snapshot_archive_info, incremental_snapshot_archive_info)) =
        get_snapshots_to_load(snapshot_config)
    else {
        return Ok(None);
    };

    info!(
        "Initializing bank snapshots dir: {}",
        snapshot_config.bank_snapshots_dir.display()
    );
    std::fs::create_dir_all(&snapshot_config.bank_snapshots_dir)
        .expect("create bank snapshots dir");

    // Fail hard here if snapshot fails to load, don't silently continue
    if account_paths.is_empty() {
        return Err(BankForksUtilsError::AccountPathsNotPresent);
    }

    let latest_snapshot_archive_slot = std::cmp::max(
        full_snapshot_archive_info.slot(),
        incremental_snapshot_archive_info
            .as_ref()
            .map(SnapshotArchiveInfoGetter::slot)
            .unwrap_or(0),
    );

    let fastboot_snapshot = match process_options.use_snapshot_archives_at_startup {
        UseSnapshotArchivesAtStartup::Always => None,
        UseSnapshotArchivesAtStartup::Never => {
            let Some(bank_snapshot) =
                snapshot_utils::get_highest_loadable_bank_snapshot(snapshot_config)
            else {
                return Err(BankForksUtilsError::NoBankSnapshotDirectory {
                    flag: use_snapshot_archives_at_startup::cli::LONG_ARG.to_string(),
                    value: UseSnapshotArchivesAtStartup::Never.to_string(),
                });
            };
            // If a newer snapshot archive was downloaded, it is possible that its slot is
            // higher than the local state we will load.  Did the user intend for this?
            if bank_snapshot.slot < latest_snapshot_archive_slot {
                warn!(
                    "Starting up from local state at slot {}, which is *older* than the latest \
                     snapshot archive at slot {}. If this is not desired, change the --{} CLI \
                     option to *not* \"{}\" and restart.",
                    bank_snapshot.slot,
                    latest_snapshot_archive_slot,
                    use_snapshot_archives_at_startup::cli::LONG_ARG,
                    UseSnapshotArchivesAtStartup::Never,
                );
            }
            Some(bank_snapshot)
        }
        UseSnapshotArchivesAtStartup::WhenNewest => {
            snapshot_utils::get_highest_loadable_bank_snapshot(snapshot_config)
                .filter(|bank_snapshot| bank_snapshot.slot >= latest_snapshot_archive_slot)
        }
    };

    let bank = if let Some(fastboot_snapshot) = fastboot_snapshot {
        snapshot_bank_utils::bank_from_snapshot_dir(
            account_paths,
            &fastboot_snapshot,
            genesis_config,
            &process_options.runtime_config,
            process_options.debug_keys.clone(),
            None, // leader_for_tests
            process_options.limit_load_slot_count_from_snapshot,
            process_options.verify_index,
            process_options.accounts_db_config.clone(),
            accounts_update_notifier,
            exit,
        )
        .map_err(|err| BankForksUtilsError::BankFromSnapshotsDirectory {
            source: err,
            path: fastboot_snapshot.snapshot_path(),
        })?
    } else {
        // Committed to loading from a snapshot archive — the existing storages a previous run
        // left around (kept for fastboot) are now orphans, and the archive will be extracted
        // into the (cleared) run dirs.
        discard_previous_run_state(&snapshot_config.bank_snapshots_dir, account_paths);

        snapshot_bank_utils::bank_from_snapshot_archives(
            account_paths,
            &full_snapshot_archive_info,
            incremental_snapshot_archive_info.as_ref(),
            snapshot_config,
            genesis_config,
            &process_options.runtime_config,
            process_options.debug_keys.clone(),
            None, // leader_for_tests
            process_options.limit_load_slot_count_from_snapshot,
            process_options.accounts_db_force_initial_clean,
            process_options.verify_index,
            process_options.accounts_db_config.clone(),
            accounts_update_notifier,
            exit,
        )
        .map_err(|err| BankForksUtilsError::BankFromSnapshotsArchive {
            source: Box::new(err),
            full_snapshot_archive: full_snapshot_archive_info.path().display().to_string(),
            incremental_snapshot_archive: incremental_snapshot_archive_info
                .as_ref()
                .map(|archive| archive.path().display().to_string())
                .unwrap_or("none".to_string()),
        })?
    };

    // We must inform accounts-db of the latest full snapshot slot, which is used by the background
    // processes to handle zero lamport accounts.  Since we've now successfully loaded the bank
    // from snapshots, this is a good time to do that update.
    // Note, this must only be set if we should generate snapshots, so that we correctly
    // handle (i.e. purge) zero lamport accounts.
    if snapshot_config.should_generate_snapshots() {
        bank.rc
            .accounts
            .accounts_db
            .set_latest_full_snapshot_slot(full_snapshot_archive_info.slot());
        // Set the last swept slot so the first full snapshot only triggers
        // cleaning of zero lamport single ref accounts between the previous
        // full snapshot and the new full snapshot
        bank.rc
            .accounts
            .accounts_db
            .set_last_swept_full_snapshot_slot(full_snapshot_archive_info.slot());
    } else {
        assert!(
            bank.rc
                .accounts
                .accounts_db
                .latest_full_snapshot_slot()
                .is_none()
        );
    }

    let full_snapshot_hash = FullSnapshotHash((
        full_snapshot_archive_info.slot(),
        *full_snapshot_archive_info.hash(),
    ));
    let incremental_snapshot_hash =
        incremental_snapshot_archive_info.map(|incremental_snapshot_archive_info| {
            IncrementalSnapshotHash((
                incremental_snapshot_archive_info.slot(),
                *incremental_snapshot_archive_info.hash(),
            ))
        });
    let starting_snapshot_hashes = StartingSnapshotHashes {
        full: full_snapshot_hash,
        incremental: incremental_snapshot_hash,
    };
    register_hard_forks(&bank, process_options)?;

    Ok(Some((
        BankForks::new_rw_arc(bank),
        Some(starting_snapshot_hashes),
    )))
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::genesis_utils::{GenesisConfigInfo, create_genesis_config},
        solana_runtime::bank::SlotLeader,
    };

    fn bank_at_slot(slot: Slot) -> Arc<Bank> {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(100);
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        if slot == 0 {
            return bank0;
        }

        let child = Bank::new_from_parent(bank0, SlotLeader::default(), slot);

        bank_forks
            .write()
            .unwrap()
            .insert(child)
            .clone_without_scheduler()
    }

    fn process_options(new_hard_forks: Vec<Slot>, fail_on_conflict: bool) -> ProcessOptions {
        ProcessOptions {
            new_hard_forks: Some(new_hard_forks),
            fail_on_hard_fork_conflict: fail_on_conflict,
            ..ProcessOptions::default()
        }
    }

    fn hard_fork_slots(bank: &Bank) -> Vec<(Slot, usize)> {
        bank.hard_forks().iter().copied().collect()
    }

    #[test]
    fn test_register_hard_forks_ahead_of_bank() {
        let bank = bank_at_slot(10);
        register_hard_forks(&bank, &process_options(vec![11], true)).unwrap();

        assert_eq!(hard_fork_slots(&bank), vec![(11, 1)]);
    }

    #[test]
    fn test_register_hard_forks_none_requested() {
        let bank = bank_at_slot(10);
        register_hard_forks(&bank, &ProcessOptions::default()).unwrap();

        assert!(hard_fork_slots(&bank).is_empty());
    }

    #[test]
    fn test_register_hard_forks_repeated_on_one_command_line() {
        let bank = bank_at_slot(10);
        register_hard_forks(&bank, &process_options(vec![11, 11], true)).unwrap();

        assert_eq!(hard_fork_slots(&bank), vec![(11, 2)]);
    }

    #[test]
    fn test_register_hard_forks_already_registered() {
        let bank = bank_at_slot(10);
        bank.register_hard_fork(11);

        let err = register_hard_forks(&bank, &process_options(vec![11], true)).unwrap_err();
        assert!(matches!(
            err,
            BankForksUtilsError::HardForkAlreadyRegistered {
                slot: 11,
                bank_slot: 10
            }
        ));
        assert_eq!(hard_fork_slots(&bank), vec![(11, 1)]);
    }

    #[test]
    fn test_register_hard_forks_already_registered_without_fail_on_conflict() {
        let bank = bank_at_slot(10);
        bank.register_hard_fork(11);

        register_hard_forks(&bank, &process_options(vec![11], false)).unwrap();
        assert_eq!(hard_fork_slots(&bank), vec![(11, 2)]);
    }

    #[test]
    fn test_register_hard_forks_at_frozen_bank_slot() {
        let bank = bank_at_slot(10);
        bank.freeze();

        let err = register_hard_forks(&bank, &process_options(vec![10], true)).unwrap_err();
        assert!(matches!(
            err,
            BankForksUtilsError::HardForkIgnored {
                slot: 10,
                bank_slot: 10
            }
        ));
        assert!(hard_fork_slots(&bank).is_empty());
    }

    #[test]
    fn test_register_hard_forks_behind_bank() {
        let bank = bank_at_slot(10);

        let err = register_hard_forks(&bank, &process_options(vec![9], true)).unwrap_err();
        assert!(matches!(
            err,
            BankForksUtilsError::HardForkIgnored {
                slot: 9,
                bank_slot: 10
            }
        ));
        assert!(hard_fork_slots(&bank).is_empty());
    }
}
