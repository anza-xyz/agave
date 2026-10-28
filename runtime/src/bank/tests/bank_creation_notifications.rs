use {
    super::*,
    solana_accounts_db::accounts_update_notifier_interface::{
        AccountForGeyser, AccountsUpdateNotifierInterface,
    },
};

type BankNotifications = (Slot, Option<Slot>, usize);

#[derive(Debug, Default)]
struct BankCreationNotifier {
    // Each bank must be announced once, before its first account update.
    banks: Mutex<HashMap<BankId, BankNotifications>>,
}

impl AccountsUpdateNotifierInterface for BankCreationNotifier {
    fn snapshot_notifications_enabled(&self) -> bool {
        false
    }

    fn notify_bank_creation(&self, slot: Slot, parent: Option<Slot>, bank_id: BankId) {
        assert!(
            self.banks
                .lock()
                .unwrap()
                .insert(bank_id, (slot, parent, 0))
                .is_none()
        );
    }

    fn notify_account_update(
        &self,
        slot: Slot,
        bank_id: BankId,
        _account: &AccountSharedData,
        _txn: &Option<&SanitizedTransaction>,
        _pubkey: &Pubkey,
        _write_version: u64,
    ) {
        let mut banks = self.banks.lock().unwrap();
        let (bank_slot, _, updates) = banks
            .get_mut(&bank_id)
            .expect("bank must be announced first");
        assert_eq!(*bank_slot, slot);
        *updates += 1;
    }

    fn notify_account_restore_from_snapshot(
        &self,
        _slot: Slot,
        _write_version: u64,
        _account: &AccountForGeyser<'_>,
    ) {
        panic!("snapshot account notifications disabled");
    }

    fn notify_end_of_restore_from_snapshot(&self) {}
}

fn genesis_bank(genesis_config: &GenesisConfig, notifier: Arc<BankCreationNotifier>) -> Bank {
    Bank::new_from_genesis(
        genesis_config,
        Arc::default(),
        Vec::new(),
        None,
        ACCOUNTS_DB_CONFIG_FOR_TESTING,
        Some(notifier),
        None,
        Arc::new(AtomicBool::new(false)),
        None,
        None,
    )
}

#[test]
fn test_bank_creation_precedes_constructor_writes() {
    let GenesisConfigInfo { genesis_config, .. } =
        create_genesis_config_with_tower_leader(100_000, &Pubkey::new_unique(), 3);
    let notifier = Arc::new(BankCreationNotifier::default());
    let (genesis, _bank_forks) =
        genesis_bank(&genesis_config, Arc::clone(&notifier)).wrap_with_bank_forks_for_tests();
    let child = Arc::new(Bank::new_from_parent(
        Arc::clone(&genesis),
        SlotLeader::default(),
        1,
    ));
    // These constructors are also used by local-ledger replay at startup.
    let epoch_slot = genesis.epoch_schedule().get_first_slot_in_epoch(1);
    let epoch_bank = Bank::new_from_parent(Arc::clone(&child), SlotLeader::default(), epoch_slot);
    let original_id = epoch_bank.bank_id();
    assert_eq!(epoch_bank.epoch(), 1);
    drop(epoch_bank);
    let replacement = Bank::new_from_parent(Arc::clone(&child), SlotLeader::default(), epoch_slot);
    assert_ne!(original_id, replacement.bank_id());

    let banks = notifier.banks.lock().unwrap();
    assert_eq!(banks.len(), 4);
    for (bank_id, expected_slot, expected_parent) in [
        (genesis.bank_id(), 0, None),
        (child.bank_id(), 1, Some(0)),
        (original_id, epoch_slot, Some(1)),
        (replacement.bank_id(), epoch_slot, Some(1)),
    ] {
        let (slot, parent, updates) = banks[&bank_id];
        assert_eq!((slot, parent), (expected_slot, expected_parent));
        assert!(
            updates > 0,
            "must exercise construction-time account writes"
        );
    }
}

#[test]
fn test_bank_creation_from_snapshot() {
    let GenesisConfigInfo { genesis_config, .. } =
        create_genesis_config_with_tower_leader(100_000, &Pubkey::new_unique(), 3);
    let notifier = Arc::new(BankCreationNotifier::default());
    let (genesis, _bank_forks) =
        genesis_bank(&genesis_config, Arc::clone(&notifier)).wrap_with_bank_forks_for_tests();
    let child = Bank::new_from_parent(Arc::clone(&genesis), SlotLeader::default(), 1);

    for (bank, parent) in [(&*genesis, None), (&child, Some(0))] {
        bank.set_block_id(Some(BlockId::default()));
        bank.squash();
        bank.force_flush_accounts_cache();
        let mut buf = Vec::new();
        crate::serde_snapshot::bank_to_stream(&mut std::io::BufWriter::new(&mut buf), bank)
            .unwrap();
        let (mut fields, _) =
            fields_from_stream(&mut std::io::BufReader::new(Cursor::new(buf))).unwrap();
        let epoch_stakes = std::mem::take(&mut fields.versioned_epoch_stakes)
            .into_iter()
            .map(|(epoch, stakes)| (epoch, stakes.into()))
            .collect();
        let restored = Bank::new_from_snapshot(
            BankRc {
                accounts: Arc::clone(&bank.rc.accounts),
                parent: RwLock::new(None),
                bank_id_generator: Arc::clone(&bank.rc.bank_id_generator),
            },
            &genesis_config,
            Arc::default(),
            fields,
            Some(*bank.leader()),
            None,
            bank.load_accounts_data_size(),
            bank.stakes_cache
                .stakes()
                .stake_delegations()
                .keys()
                .copied()
                .collect(),
            epoch_stakes,
        );
        assert_ne!(restored.bank_id(), bank.bank_id());
        let banks = notifier.banks.lock().unwrap();
        let (slot, actual_parent, _) = banks[&restored.bank_id()];
        assert_eq!((slot, actual_parent), (bank.slot(), parent));
    }
}
