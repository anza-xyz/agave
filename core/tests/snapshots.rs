#![allow(clippy::arithmetic_side_effects)]

use {
    crate::snapshot_utils::create_tmp_accounts_dir_for_tests,
    agave_snapshots::{
        paths as snapshot_paths, snapshot_archive_info::FullSnapshotArchiveInfo,
        snapshot_config::SnapshotConfig, SnapshotInterval,
    },
    crossbeam_channel::unbounded,
    itertools::Itertools,
    log::{info, trace},
    solana_accounts_db::accounts_db::ACCOUNTS_DB_CONFIG_FOR_TESTING,
    solana_clock::Slot,
    solana_core::snapshot_packager_service::SnapshotPackagerService,
    solana_genesis_config::GenesisConfig,
    solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
    solana_keypair::Keypair,
    solana_net_utils::SocketAddrSpace,
    solana_pubkey::Pubkey,
    solana_runtime::{
        accounts_background_service::{
            AbsRequestHandlers, AccountsBackgroundService, PendingSnapshotPackages,
            PrunedBanksRequestHandler, SendDroppedBankCallback, SnapshotRequestHandler,
        },
        bank::{Bank, BankTestConfig},
        bank_forks::BankForks,
        genesis_utils::{create_genesis_config_with_leader, GenesisConfigInfo},
        runtime_config::RuntimeConfig,
        snapshot_bank_utils,
        snapshot_controller::SnapshotController,
        snapshot_utils,
        status_cache::MAX_CACHE_ENTRIES,
    },
    solana_sha256_hasher::hashv,
    solana_signer::Signer,
    solana_system_transaction as system_transaction,
    solana_time_utils::timestamp,
    std::{
        num::NonZeroU64,
        path::PathBuf,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex, RwLock,
        },
        time::{Duration, Instant},
    },
    tempfile::TempDir,
};

struct SnapshotTestConfig {
    bank_forks: Arc<RwLock<BankForks>>,
    genesis_config_info: GenesisConfigInfo,
    snapshot_config: SnapshotConfig,
    incremental_snapshot_archives_dir: TempDir,
    full_snapshot_archives_dir: TempDir,
    bank_snapshots_dir: TempDir,
    accounts_dir: PathBuf,
    // as the underscore prefix indicates, this isn't explicitly used; but it's needed to keep
    // TempDir::drop from running to retain that dir for the duration of test
    _accounts_tmp_dir: TempDir,
}

impl SnapshotTestConfig {
    fn new(
        full_snapshot_archive_interval: SnapshotInterval,
        incremental_snapshot_archive_interval: SnapshotInterval,
    ) -> SnapshotTestConfig {
        let (accounts_tmp_dir, accounts_dir) = create_tmp_accounts_dir_for_tests();
        let bank_snapshots_dir = TempDir::new().unwrap();
        let full_snapshot_archives_dir = TempDir::new().unwrap();
        let incremental_snapshot_archives_dir = TempDir::new().unwrap();
        // validator_stake_lamports should be non-zero otherwise stake
        // account will not be stored in accounts-db but still cached in
        // bank stakes which results in mismatch when banks are loaded from
        // snapshots.
        let genesis_config_info = create_genesis_config_with_leader(
            10_000,                     // mint_lamports
            &solana_pubkey::new_rand(), // validator_pubkey
            1,                          // validator_stake_lamports
        );
        let bank0 = Bank::new_with_paths_for_tests(
            &genesis_config_info.genesis_config,
            Arc::<RuntimeConfig>::default(),
            BankTestConfig::default(),
            vec![accounts_dir.clone()],
        );
        bank0.freeze();
        let bank_forks_arc = BankForks::new_rw_arc(bank0);

        let snapshot_config = SnapshotConfig {
            full_snapshot_archive_interval,
            incremental_snapshot_archive_interval,
            full_snapshot_archives_dir: full_snapshot_archives_dir.path().to_path_buf(),
            incremental_snapshot_archives_dir: incremental_snapshot_archives_dir
                .path()
                .to_path_buf(),
            bank_snapshots_dir: bank_snapshots_dir.path().to_path_buf(),
            ..SnapshotConfig::default()
        };
        SnapshotTestConfig {
            bank_forks: bank_forks_arc.clone(),
            genesis_config_info,
            snapshot_config,
            incremental_snapshot_archives_dir,
            full_snapshot_archives_dir,
            bank_snapshots_dir,
            accounts_dir,
            _accounts_tmp_dir: accounts_tmp_dir,
        }
    }
}

fn restore_from_snapshot(
    old_bank_forks: Arc<RwLock<BankForks>>,
    old_last_slot: Slot,
    old_genesis_config: &GenesisConfig,
    snapshot_config: &SnapshotConfig,
    account_paths: &[PathBuf],
) {
    let old_bank_forks = old_bank_forks.read().unwrap();
    let old_last_bank = old_bank_forks.get(old_last_slot).unwrap();

    let full_snapshot_archive_path = snapshot_paths::build_full_snapshot_archive_path(
        &snapshot_config.full_snapshot_archives_dir,
        old_last_bank.slot(),
        &old_last_bank.get_snapshot_hash(),
        snapshot_config.archive_format,
    );
    let full_snapshot_archive_info =
        FullSnapshotArchiveInfo::new_from_path(full_snapshot_archive_path).unwrap();

    let deserialized_bank = snapshot_bank_utils::bank_from_snapshot_archives(
        account_paths,
        &snapshot_config.bank_snapshots_dir,
        &full_snapshot_archive_info,
        None,
        old_genesis_config,
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

    let bank = old_bank_forks.get(deserialized_bank.slot()).unwrap();
    assert_eq!(bank.as_ref(), &deserialized_bank);
}

// creates banks up to "last_slot" and runs the input function `f` on each bank created
// also marks each bank as root and generates snapshots
// finally tries to restore from the last bank's snapshot and compares the restored bank to the
// `last_slot` bank
fn run_bank_forks_snapshot_n<F>(last_slot: Slot, f: F, set_root_interval: u64)
where
    F: Fn(&Bank, &Keypair),
{
    agave_logger::setup();
    // Set up snapshotting config
    let snapshot_test_config = SnapshotTestConfig::new(
        SnapshotInterval::Slots(NonZeroU64::new(set_root_interval).unwrap()),
        SnapshotInterval::Disabled,
    );

    let bank_forks = snapshot_test_config.bank_forks.clone();
    let mint_keypair = &snapshot_test_config.genesis_config_info.mint_keypair;

    let pending_snapshot_packages = Arc::new(Mutex::new(PendingSnapshotPackages::default()));
    let (snapshot_request_sender, snapshot_request_receiver) = unbounded();
    let snapshot_controller = Arc::new(SnapshotController::new(
        snapshot_request_sender.clone(),
        snapshot_test_config.snapshot_config.clone(),
        bank_forks.read().unwrap().root(),
    ));
    let snapshot_request_handler = SnapshotRequestHandler {
        snapshot_controller: snapshot_controller.clone(),
        snapshot_request_receiver,
        pending_snapshot_packages,
    };
    for slot in 1..=last_slot {
        let bank = Bank::new_from_parent(
            bank_forks.read().unwrap().get(slot - 1).unwrap().clone(),
            &Pubkey::default(),
            slot,
        );
        let bank = bank_forks.write().unwrap().insert(bank);
        f(bank.clone_without_scheduler().as_ref(), mint_keypair);
        // Set root to make sure we don't end up with too many account storage entries
        // and to allow snapshotting of bank and the purging logic on status_cache to
        // kick in
        if slot % set_root_interval == 0 || slot == last_slot {
            if !bank.is_complete() {
                bank.fill_bank_with_ticks_for_tests();
            }
            bank_forks.read().unwrap().prune_program_cache(bank.slot());
            // set_root should send a snapshot request
            bank_forks
                .write()
                .unwrap()
                .set_root(bank.slot(), Some(&snapshot_controller), None);
            snapshot_request_handler.handle_snapshot_requests(0);
        }
    }

    // Generate a snapshot package for last bank
    let snapshot_config = snapshot_controller.snapshot_config();
    let last_bank = bank_forks.read().unwrap().get(last_slot).unwrap();
    snapshot_bank_utils::bank_to_full_snapshot_archive(
        &snapshot_config.bank_snapshots_dir,
        &last_bank,
        Some(snapshot_config.snapshot_version),
        &snapshot_config.full_snapshot_archives_dir,
        &snapshot_config.incremental_snapshot_archives_dir,
        snapshot_config.archive_format,
    )
    .unwrap();

    // Restore bank from snapshot
    let (_tmp_dir, temporary_accounts_dir) = create_tmp_accounts_dir_for_tests();
    let account_paths = &[temporary_accounts_dir];
    let genesis_config = &snapshot_test_config.genesis_config_info.genesis_config;
    restore_from_snapshot(
        snapshot_test_config.bank_forks.clone(),
        last_slot,
        genesis_config,
        snapshot_config,
        account_paths,
    );
}

#[test]
fn test_bank_forks_snapshot() {
    // create banks up to slot 4 and create 1 new account in each bank. test that bank 4 snapshots
    // and restores correctly
    run_bank_forks_snapshot_n(
        4,
        |bank, mint_keypair| {
            let key1 = Keypair::new().pubkey();
            let tx = system_transaction::transfer(mint_keypair, &key1, 1, bank.last_blockhash());
            assert_eq!(bank.process_transaction(&tx), Ok(()));

            let key2 = Keypair::new().pubkey();
            let tx = system_transaction::transfer(mint_keypair, &key2, 0, bank.last_blockhash());
            assert_eq!(bank.process_transaction(&tx), Ok(()));
        },
        1,
    );
}

fn goto_end_of_slot(bank: &Bank) {
    let mut tick_hash = bank.last_blockhash();
    loop {
        tick_hash = hashv(&[tick_hash.as_ref(), &[42]]);
        bank.register_tick_for_test(&tick_hash);
        if tick_hash == bank.last_blockhash() {
            bank.freeze();
            return;
        }
    }
}

#[test]
fn test_slots_to_snapshot() {
    agave_logger::setup();
    let num_set_roots = MAX_CACHE_ENTRIES * 2;

    for add_root_interval in &[1, 3, 9] {
        let (snapshot_sender, _snapshot_receiver) = unbounded();
        // Make sure this test never clears bank.slots_since_snapshot
        let snapshot_test_config = SnapshotTestConfig::new(
            SnapshotInterval::Slots(
                NonZeroU64::new((*add_root_interval * num_set_roots * 2) as Slot).unwrap(),
            ),
            SnapshotInterval::Disabled,
        );
        let bank_forks = snapshot_test_config.bank_forks.clone();
        let bank_forks_r = bank_forks.read().unwrap();
        let mut current_bank = bank_forks_r[0].clone();
        drop(bank_forks_r);
        let snapshot_controller = SnapshotController::new(
            snapshot_sender,
            snapshot_test_config.snapshot_config.clone(),
            bank_forks.read().unwrap().root(),
        );
        for _ in 0..num_set_roots {
            for _ in 0..*add_root_interval {
                let new_slot = current_bank.slot() + 1;
                let new_bank = Bank::new_from_parent(current_bank, &Pubkey::default(), new_slot);
                current_bank = bank_forks.write().unwrap().insert(new_bank).clone();
            }
            bank_forks
                .read()
                .unwrap()
                .prune_program_cache(current_bank.slot());
            bank_forks.write().unwrap().set_root(
                current_bank.slot(),
                Some(&snapshot_controller),
                None,
            );
        }

        let num_old_slots = num_set_roots * *add_root_interval - MAX_CACHE_ENTRIES + 1;
        let expected_slots_to_snapshot =
            num_old_slots as u64..=num_set_roots as u64 * *add_root_interval as u64;

        let slots_to_snapshot = bank_forks
            .read()
            .unwrap()
            .root_bank()
            .status_cache
            .read()
            .unwrap()
            .roots()
            .iter()
            .cloned()
            .sorted();
        assert!(slots_to_snapshot.into_iter().eq(expected_slots_to_snapshot));
    }
}

#[test]
fn test_bank_forks_status_cache_snapshot() {
    // create banks up to slot (MAX_CACHE_ENTRIES * 2) + 1 while transferring 1 lamport into 2 different accounts each time
    // this is done to ensure the AccountStorageEntries keep getting cleaned up as the root moves
    // ahead. Also tests the status_cache purge and status cache snapshotting.
    // Makes sure that the last bank is restored correctly
    let key1 = Keypair::new().pubkey();
    let key2 = Keypair::new().pubkey();
    for set_root_interval in &[1, 4] {
        run_bank_forks_snapshot_n(
            (MAX_CACHE_ENTRIES * 2) as u64,
            |bank, mint_keypair| {
                let tx = system_transaction::transfer(
                    mint_keypair,
                    &key1,
                    1,
                    bank.parent().unwrap().last_blockhash(),
                );
                assert_eq!(bank.process_transaction(&tx), Ok(()));
                let tx = system_transaction::transfer(
                    mint_keypair,
                    &key2,
                    1,
                    bank.parent().unwrap().last_blockhash(),
                );
                assert_eq!(bank.process_transaction(&tx), Ok(()));
                goto_end_of_slot(bank);
            },
            *set_root_interval,
        );
    }
}

#[test]
fn test_bank_forks_incremental_snapshot() {
    agave_logger::setup();

    const SET_ROOT_INTERVAL: Slot = 2;
    const INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS: Slot = SET_ROOT_INTERVAL * 2;
    const FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS: Slot =
        INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS * 5;
    const LAST_SLOT: Slot = FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS * 2 - 1;

    info!(
        "Running bank forks incremental snapshot test, full snapshot interval: \
         {FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS} slots, incremental snapshot interval: \
         {INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS} slots, last slot: {LAST_SLOT}, set root \
         interval: {SET_ROOT_INTERVAL} slots"
    );

    let snapshot_test_config = SnapshotTestConfig::new(
        SnapshotInterval::Slots(NonZeroU64::new(FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS).unwrap()),
        SnapshotInterval::Slots(
            NonZeroU64::new(INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS).unwrap(),
        ),
    );
    trace!(
        "SnapshotTestConfig:\naccounts_dir: {}\nbank_snapshots_dir: \
         {}\nfull_snapshot_archives_dir: {}\nincremental_snapshot_archives_dir: {}",
        snapshot_test_config.accounts_dir.display(),
        snapshot_test_config.bank_snapshots_dir.path().display(),
        snapshot_test_config
            .full_snapshot_archives_dir
            .path()
            .display(),
        snapshot_test_config
            .incremental_snapshot_archives_dir
            .path()
            .display()
    );

    let bank_forks = snapshot_test_config.bank_forks.clone();
    let mint_keypair = &snapshot_test_config.genesis_config_info.mint_keypair;

    let pending_snapshot_packages = Arc::new(Mutex::new(PendingSnapshotPackages::default()));
    let (snapshot_request_sender, snapshot_request_receiver) = unbounded();
    let snapshot_controller = Arc::new(SnapshotController::new(
        snapshot_request_sender.clone(),
        snapshot_test_config.snapshot_config.clone(),
        bank_forks.read().unwrap().root(),
    ));
    let snapshot_request_handler = SnapshotRequestHandler {
        snapshot_controller: snapshot_controller.clone(),
        snapshot_request_receiver,
        pending_snapshot_packages,
    };

    let mut latest_full_snapshot_slot = None;
    for slot in 1..=LAST_SLOT {
        // Make a new bank and perform some transactions
        let bank = {
            let parent = bank_forks.read().unwrap().get(slot - 1).unwrap();
            let bank = Bank::new_from_parent(parent, &Pubkey::default(), slot);
            let bank_scheduler = bank_forks.write().unwrap().insert(bank);
            let bank = bank_scheduler.clone_without_scheduler();

            let key = solana_pubkey::new_rand();
            let tx = system_transaction::transfer(mint_keypair, &key, 1, bank.last_blockhash());
            assert_eq!(bank.process_transaction(&tx), Ok(()));

            let key = solana_pubkey::new_rand();
            let tx = system_transaction::transfer(mint_keypair, &key, 0, bank.last_blockhash());
            assert_eq!(bank.process_transaction(&tx), Ok(()));

            bank.fill_bank_with_ticks_for_tests();

            bank_scheduler
        };

        // Set root to make sure we don't end up with too many account storage entries
        // and to allow snapshotting of bank and the purging logic on status_cache to
        // kick in
        if slot % SET_ROOT_INTERVAL == 0 {
            // set_root sends a snapshot request
            bank_forks.read().unwrap().prune_program_cache(bank.slot());
            bank_forks
                .write()
                .unwrap()
                .set_root(bank.slot(), Some(&snapshot_controller), None);
            snapshot_request_handler.handle_snapshot_requests(0);
        }

        // Since AccountsBackgroundService isn't running, manually make a full snapshot archive
        // at the right interval
        if snapshot_utils::should_take_full_snapshot(slot, FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS) {
            make_full_snapshot_archive(&bank, snapshot_controller.snapshot_config()).unwrap();
            latest_full_snapshot_slot = Some(slot);
        }
        // Similarly, make an incremental snapshot archive at the right interval, but only if
        // there's been at least one full snapshot first, and a full snapshot wasn't already
        // taken at this slot.
        //
        // Then, after making an incremental snapshot, restore the bank and verify it is correct
        else if snapshot_utils::should_take_incremental_snapshot(
            slot,
            INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS,
            latest_full_snapshot_slot,
        ) && slot != latest_full_snapshot_slot.unwrap()
        {
            make_incremental_snapshot_archive(
                &bank,
                latest_full_snapshot_slot.unwrap(),
                snapshot_controller.snapshot_config(),
            )
            .unwrap();

            // Accounts directory needs to be separate from the active accounts directory
            // so that dropping append vecs in the active accounts directory doesn't
            // delete the unpacked appendvecs in the snapshot
            let (_tmp_dir, temporary_accounts_dir) = create_tmp_accounts_dir_for_tests();
            restore_from_snapshots_and_check_banks_are_equal(
                &bank,
                snapshot_controller.snapshot_config(),
                temporary_accounts_dir,
                &snapshot_test_config.genesis_config_info.genesis_config,
            )
            .unwrap();
        }
    }
}

fn make_full_snapshot_archive(
    bank: &Bank,
    snapshot_config: &SnapshotConfig,
) -> agave_snapshots::Result<()> {
    info!(
        "Making full snapshot archive from bank at slot: {}",
        bank.slot(),
    );
    snapshot_bank_utils::bank_to_full_snapshot_archive(
        &snapshot_config.bank_snapshots_dir,
        bank,
        Some(snapshot_config.snapshot_version),
        &snapshot_config.full_snapshot_archives_dir,
        &snapshot_config.incremental_snapshot_archives_dir,
        snapshot_config.archive_format,
    )?;
    Ok(())
}

fn make_incremental_snapshot_archive(
    bank: &Bank,
    incremental_snapshot_base_slot: Slot,
    snapshot_config: &SnapshotConfig,
) -> agave_snapshots::Result<()> {
    info!(
        "Making incremental snapshot archive from bank at slot: {}, and base slot: {}",
        bank.slot(),
        incremental_snapshot_base_slot,
    );
    snapshot_bank_utils::bank_to_incremental_snapshot_archive(
        &snapshot_config.bank_snapshots_dir,
        bank,
        incremental_snapshot_base_slot,
        Some(snapshot_config.snapshot_version),
        &snapshot_config.full_snapshot_archives_dir,
        &snapshot_config.incremental_snapshot_archives_dir,
        snapshot_config.archive_format,
    )?;
    Ok(())
}

fn restore_from_snapshots_and_check_banks_are_equal(
    bank: &Bank,
    snapshot_config: &SnapshotConfig,
    accounts_dir: PathBuf,
    genesis_config: &GenesisConfig,
) -> agave_snapshots::Result<()> {
    let (deserialized_bank, ..) = snapshot_bank_utils::bank_from_latest_snapshot_archives(
        &snapshot_config.bank_snapshots_dir,
        &snapshot_config.full_snapshot_archives_dir,
        &snapshot_config.incremental_snapshot_archives_dir,
        &[accounts_dir],
        genesis_config,
        &RuntimeConfig::default(),
        None,
        None,
        false,
        false,
        false,
        ACCOUNTS_DB_CONFIG_FOR_TESTING,
        None,
        Arc::default(),
    )?;

    assert_eq!(bank, &deserialized_bank);

    Ok(())
}

/// Spin up the background services fully then test taking & verifying snapshots
#[test]
fn test_snapshots_with_background_services() {
    agave_logger::setup();

    const SET_ROOT_INTERVAL_SLOTS: Slot = 2;
    const BANK_SNAPSHOT_INTERVAL_SLOTS: Slot = SET_ROOT_INTERVAL_SLOTS * 2;
    const INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS: Slot = BANK_SNAPSHOT_INTERVAL_SLOTS * 3;
    const FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS: Slot =
        INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS * 5;
    const LAST_SLOT: Slot =
        FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS * 3 + INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS * 2;

    // Maximum amount of time to wait for each snapshot archive to be created.
    // This should be enough time, but if it times-out in CI, try increasing it.
    const MAX_WAIT_DURATION: Duration = Duration::from_secs(10);

    info!("Running snapshots with background services test...");
    #[rustfmt::skip]
    trace!(
        "Test configuration parameters:\
         \n\tfull snapshot archive interval: {FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS} slots\
         \n\tincremental snapshot archive interval: {INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS} slots\
         \n\tbank snapshot interval: {BANK_SNAPSHOT_INTERVAL_SLOTS} slots\
         \n\tset root interval: {SET_ROOT_INTERVAL_SLOTS} slots\
         \n\tlast slot: {LAST_SLOT}"
    );

    let snapshot_test_config = SnapshotTestConfig::new(
        SnapshotInterval::Slots(NonZeroU64::new(FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS).unwrap()),
        SnapshotInterval::Slots(
            NonZeroU64::new(INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS).unwrap(),
        ),
    );

    let node_keypair = Arc::new(Keypair::new());
    let cluster_info = Arc::new(ClusterInfo::new(
        ContactInfo::new_localhost(&node_keypair.pubkey(), timestamp()),
        node_keypair,
        SocketAddrSpace::Unspecified,
    ));

    let (pruned_banks_sender, pruned_banks_receiver) = unbounded();
    let (snapshot_request_sender, snapshot_request_receiver) = unbounded();
    let pending_snapshot_packages = Arc::new(Mutex::new(PendingSnapshotPackages::default()));

    let bank_forks = snapshot_test_config.bank_forks.clone();
    bank_forks
        .read()
        .unwrap()
        .root_bank()
        .rc
        .accounts
        .accounts_db
        .enable_bank_drop_callback();
    let callback = SendDroppedBankCallback::new(pruned_banks_sender);
    for bank in bank_forks.read().unwrap().banks().values() {
        bank.set_callback(Some(Box::new(callback.clone())));
    }

    let snapshot_controller = Arc::new(SnapshotController::new(
        snapshot_request_sender.clone(),
        snapshot_test_config.snapshot_config.clone(),
        bank_forks.read().unwrap().root(),
    ));
    let snapshot_request_handler = SnapshotRequestHandler {
        snapshot_controller: snapshot_controller.clone(),
        snapshot_request_receiver,
        pending_snapshot_packages: pending_snapshot_packages.clone(),
    };
    let pruned_banks_request_handler = PrunedBanksRequestHandler {
        pruned_banks_receiver,
    };
    let abs_request_handler = AbsRequestHandlers {
        snapshot_request_handler,
        pruned_banks_request_handler,
    };

    let exit = Arc::new(AtomicBool::new(false));
    let snapshot_packager_service = SnapshotPackagerService::new(
        pending_snapshot_packages.clone(),
        None,
        exit.clone(),
        None,
        cluster_info.clone(),
        snapshot_controller.clone(),
        false,
    );

    let accounts_background_service =
        AccountsBackgroundService::new(bank_forks.clone(), exit.clone(), abs_request_handler);

    let mut latest_full_snapshot_slot = None;
    let mut latest_incremental_snapshot_slot = None;
    let mint_keypair = &snapshot_test_config.genesis_config_info.mint_keypair;
    for slot in 1..=LAST_SLOT {
        // Make a new bank and process some transactions
        {
            let bank = Bank::new_from_parent(
                bank_forks.read().unwrap().get(slot - 1).unwrap(),
                &Pubkey::default(),
                slot,
            );
            let bank = bank_forks
                .write()
                .unwrap()
                .insert(bank)
                .clone_without_scheduler();

            let key = solana_pubkey::new_rand();
            let tx = system_transaction::transfer(mint_keypair, &key, 1, bank.last_blockhash());
            assert_eq!(bank.process_transaction(&tx), Ok(()));

            let key = solana_pubkey::new_rand();
            let tx = system_transaction::transfer(mint_keypair, &key, 0, bank.last_blockhash());
            assert_eq!(bank.process_transaction(&tx), Ok(()));

            bank.fill_bank_with_ticks_for_tests();
        }

        // Call `BankForks::set_root()` to cause snapshots to be taken
        if slot % SET_ROOT_INTERVAL_SLOTS == 0 {
            bank_forks
                .write()
                .unwrap()
                .set_root(slot, Some(&snapshot_controller), None);
        }

        // If a snapshot should be taken this slot, wait for it to complete
        if slot % FULL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS == 0 {
            let timer = Instant::now();
            while snapshot_paths::get_highest_full_snapshot_archive_slot(
                &snapshot_test_config
                    .snapshot_config
                    .full_snapshot_archives_dir,
            ) != Some(slot)
            {
                assert!(
                    timer.elapsed() < MAX_WAIT_DURATION,
                    "Waiting for full snapshot {slot} exceeded the {MAX_WAIT_DURATION:?} maximum \
                     wait duration!",
                );
                std::thread::sleep(Duration::from_secs(1));
            }
            latest_full_snapshot_slot = Some(slot);
        } else if slot % INCREMENTAL_SNAPSHOT_ARCHIVE_INTERVAL_SLOTS == 0
            && latest_full_snapshot_slot.is_some()
        {
            let timer = Instant::now();
            while snapshot_paths::get_highest_incremental_snapshot_archive_slot(
                &snapshot_test_config
                    .snapshot_config
                    .incremental_snapshot_archives_dir,
                latest_full_snapshot_slot.unwrap(),
            ) != Some(slot)
            {
                assert!(
                    timer.elapsed() < MAX_WAIT_DURATION,
                    "Waiting for incremental snapshot {slot} exceeded the {MAX_WAIT_DURATION:?} \
                     maximum wait duration!",
                );
                std::thread::sleep(Duration::from_secs(1));
            }
            latest_incremental_snapshot_slot = Some(slot);
        }
    }

    // Load the snapshot and ensure it matches what's in BankForks
    let (_tmp_dir, temporary_accounts_dir) = create_tmp_accounts_dir_for_tests();
    let snapshot_config = snapshot_controller.snapshot_config();
    let (deserialized_bank, ..) = snapshot_bank_utils::bank_from_latest_snapshot_archives(
        &snapshot_config.bank_snapshots_dir,
        &snapshot_config.full_snapshot_archives_dir,
        &snapshot_config.incremental_snapshot_archives_dir,
        &[temporary_accounts_dir],
        &snapshot_test_config.genesis_config_info.genesis_config,
        &RuntimeConfig::default(),
        None,
        None,
        false,
        false,
        false,
        ACCOUNTS_DB_CONFIG_FOR_TESTING,
        None,
        exit.clone(),
    )
    .unwrap();

    assert_eq!(
        deserialized_bank.slot(),
        latest_incremental_snapshot_slot.unwrap()
    );
    assert_eq!(
        &deserialized_bank,
        bank_forks
            .read()
            .unwrap()
            .get(deserialized_bank.slot())
            .unwrap()
            .as_ref()
    );

    // Stop the background services, ignore any errors
    info!("Shutting down background services...");
    exit.store(true, Ordering::Relaxed);
    _ = accounts_background_service.join();
    _ = snapshot_packager_service.join();
}
