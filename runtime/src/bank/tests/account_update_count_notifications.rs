use {
    super::*,
    solana_accounts_db::accounts_update_notifier_interface::{
        AccountForGeyser, AccountsUpdateNotifierInterface,
    },
};

#[derive(Debug, Default)]
struct ObservedBank {
    slot: Slot,
    updates: Vec<(Pubkey, u64)>,
    count: Option<u64>,
}

#[derive(Debug)]
struct CountNotifier {
    enabled: bool,
    accounts_enabled: bool,
    banks: Mutex<HashMap<BankId, ObservedBank>>,
    hash_lock_probe: Mutex<Option<std::sync::Weak<Bank>>>,
}

impl AccountsUpdateNotifierInterface for CountNotifier {
    fn account_notifications_enabled(&self) -> bool {
        self.accounts_enabled
    }

    fn account_update_count_notifications_enabled(&self) -> bool {
        self.enabled
    }

    fn snapshot_notifications_enabled(&self) -> bool {
        false
    }

    fn notify_bank_creation(&self, slot: Slot, _parent: Option<Slot>, bank_id: BankId) {
        assert!(
            self.banks
                .lock()
                .unwrap()
                .insert(
                    bank_id,
                    ObservedBank {
                        slot,
                        ..ObservedBank::default()
                    }
                )
                .is_none()
        );
    }

    fn notify_account_update(
        &self,
        slot: Slot,
        bank_id: BankId,
        _account: &AccountSharedData,
        _txn: &Option<&SanitizedTransaction>,
        pubkey: &Pubkey,
        write_version: u64,
    ) {
        let mut banks = self.banks.lock().unwrap();
        let bank = banks.get_mut(&bank_id).unwrap();
        assert_eq!(bank.slot, slot);
        assert!(bank.count.is_none(), "account update after final count");
        bank.updates.push((*pubkey, write_version));
    }

    fn notify_account_update_count(&self, slot: Slot, bank_id: BankId, count: u64) {
        if let Some(bank) = self
            .hash_lock_probe
            .lock()
            .unwrap()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
        {
            assert_eq!(bank.bank_id(), bank_id);
            let hash = bank
                .hash
                .try_read()
                .expect("plugin callback must not hold the hash lock");
            assert_ne!(*hash, Hash::default());
        }
        let mut banks = self.banks.lock().unwrap();
        let bank = banks.get_mut(&bank_id).unwrap();
        assert_eq!(bank.slot, slot);
        assert!(bank.count.replace(count).is_none(), "duplicate final count");
    }

    fn notify_account_restore_from_snapshot(
        &self,
        _slot: Slot,
        _write_version: u64,
        _account: &AccountForGeyser<'_>,
    ) {
        panic!("snapshot notifications disabled");
    }

    fn notify_end_of_restore_from_snapshot(&self) {}
}

struct Fixture {
    genesis_config: GenesisConfig,
    bank: Arc<Bank>,
    _forks: Arc<RwLock<BankForks>>,
    mint: Keypair,
    notifier: Arc<CountNotifier>,
}

impl Fixture {
    fn new(enabled: bool, accounts_enabled: bool) -> Self {
        let GenesisConfigInfo {
            mut genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_with_tower_leader(
            10_000_000_000,
            &Pubkey::new_unique(),
            1_000_000,
        );
        genesis_config.fee_rate_governor = FeeRateGovernor::default();
        let notifier = Arc::new(CountNotifier {
            enabled,
            accounts_enabled,
            banks: Mutex::default(),
            hash_lock_probe: Mutex::default(),
        });
        let bank = Bank::new_from_genesis(
            &genesis_config,
            Arc::default(),
            Vec::new(),
            None,
            ACCOUNTS_DB_CONFIG_FOR_TESTING,
            Some(notifier.clone()),
            None,
            Arc::new(AtomicBool::new(false)),
            None,
            None,
        );
        let (bank, forks) = bank.wrap_with_bank_forks_for_tests();
        Self {
            genesis_config,
            bank,
            _forks: forks,
            mint: mint_keypair,
            notifier,
        }
    }

    fn assert_complete(&self, bank: &Bank) {
        let banks = self.notifier.banks.lock().unwrap();
        let observed = &banks[&bank.bank_id()];
        let versions: HashSet<_> = observed
            .updates
            .iter()
            .map(|(_, version)| version)
            .collect();
        assert_eq!(versions.len(), observed.updates.len());
        assert_eq!(observed.count, Some(versions.len() as u64));
    }
}

#[test]
fn test_count_includes_rewrites_deletions_and_freeze() {
    let f = Fixture::new(true, true);
    f.bank.freeze();
    f.assert_complete(&f.bank);
    let bank = Arc::new(Bank::new_from_parent(
        Arc::clone(&f.bank),
        *f.bank.leader(),
        1,
    ));
    *f.notifier.hash_lock_probe.lock().unwrap() = Some(Arc::downgrade(&bank));
    let key = Pubkey::new_unique();
    for lamports in [42, 43, 43, 0, 0] {
        bank.store_account(
            &key,
            &AccountSharedData::empty(lamports, &Pubkey::default()),
        );
    }
    let before_freeze = {
        let banks = f.notifier.banks.lock().unwrap();
        let observed = &banks[&bank.bank_id()];
        assert_eq!(
            observed.updates.iter().filter(|(k, _)| *k == key).count(),
            5
        );
        assert!(observed.count.is_none());
        observed.updates.len()
    };
    bank.freeze();
    f.assert_complete(&bank);
    let banks = f.notifier.banks.lock().unwrap();
    let observed = &banks[&bank.bank_id()];
    assert!(observed.updates.len() > before_freeze);
    // Dropping an intermediate update stays visible even if a later one is delivered twice.
    let dropped = observed.updates.iter().find(|(k, _)| *k == key).unwrap().1;
    let mut received: Vec<_> = observed
        .updates
        .iter()
        .map(|(_, v)| *v)
        .filter(|v| *v != dropped)
        .collect();
    received.push(*received.last().unwrap());
    let distinct: HashSet<_> = received.into_iter().collect();
    assert_eq!(observed.count.unwrap(), distinct.len() as u64 + 1);
}

#[test]
fn test_count_transactions_and_rollbacks() {
    let f = Fixture::new(true, true);
    let bank = Bank::new_from_parent(Arc::clone(&f.bank), *f.bank.leader(), 1);
    let recipient = Pubkey::new_unique();
    bank.process_transaction(&system_transaction::transfer(
        &f.mint,
        &recipient,
        1_000_000,
        bank.last_blockhash(),
    ))
    .unwrap();
    assert!(matches!(
        bank.process_transaction(&system_transaction::transfer(
            &f.mint,
            &recipient,
            u64::MAX,
            bank.last_blockhash()
        )),
        Err(TransactionError::InstructionError(..))
    ));
    let missing_program = Transaction::new_signed_with_payer(
        &[Instruction {
            program_id: Pubkey::new_unique(),
            accounts: vec![],
            data: vec![],
        }],
        Some(&f.mint.pubkey()),
        &[&f.mint],
        bank.last_blockhash(),
    );
    assert_eq!(
        bank.process_transaction(&missing_program),
        Err(TransactionError::ProgramAccountNotFound)
    );
    let before_rejected = f.notifier.banks.lock().unwrap()[&bank.bank_id()]
        .updates
        .len();
    assert_eq!(
        bank.process_transaction(&system_transaction::transfer(
            &f.mint,
            &recipient,
            1,
            Hash::new_unique()
        )),
        Err(TransactionError::BlockhashNotFound)
    );
    assert_eq!(
        f.notifier.banks.lock().unwrap()[&bank.bank_id()]
            .updates
            .len(),
        before_rejected
    );
    bank.freeze();
    f.assert_complete(&bank);
}

#[test]
fn test_count_once_per_bank_across_epoch_and_replacement() {
    let f = Fixture::new(true, true);
    let slot = f.bank.epoch_schedule().get_first_slot_in_epoch(1);
    let original = Bank::new_from_parent(Arc::clone(&f.bank), *f.bank.leader(), slot);
    original.store_account(
        &Pubkey::new_unique(),
        &AccountSharedData::empty(42, &Pubkey::default()),
    );
    // Concurrent freeze calls must still deliver only one count.
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| original.freeze());
        }
    });
    original.freeze();
    f.assert_complete(&original);
    let replacement = Bank::new_from_parent(Arc::clone(&f.bank), *f.bank.leader(), slot);
    assert_ne!(original.bank_id(), replacement.bank_id());
    replacement.freeze();
    f.assert_complete(&replacement);
    let banks = f.notifier.banks.lock().unwrap();
    assert_eq!(
        banks[&original.bank_id()].count.unwrap(),
        banks[&replacement.bank_id()].count.unwrap() + 1
    );
}

#[test]
fn test_count_subscription_is_independent_of_account_subscription() {
    for (counts, accounts) in [(false, true), (true, false)] {
        let f = Fixture::new(counts, accounts);
        f.bank.freeze();
        let banks = f.notifier.banks.lock().unwrap();
        let observed = &banks[&f.bank.bank_id()];
        assert_eq!(observed.count.is_some(), counts);
        assert_eq!(!observed.updates.is_empty(), accounts);
        if counts {
            assert!(observed.count.unwrap() > 0);
        }
    }
}

#[test]
fn test_restored_frozen_bank_does_not_emit_live_count() {
    let f = Fixture::new(true, true);
    f.bank.set_block_id(Some(BlockId::default()));
    f.bank.squash();
    f.bank.force_flush_accounts_cache();
    let mut buf = Vec::new();
    crate::serde_snapshot::bank_to_stream(&mut std::io::BufWriter::new(&mut buf), &f.bank).unwrap();
    let (mut fields, _) =
        fields_from_stream(&mut std::io::BufReader::new(Cursor::new(buf))).unwrap();
    fields.bank_hash_stats = f.bank.get_bank_hash_stats();
    let epoch_stakes = std::mem::take(&mut fields.versioned_epoch_stakes)
        .into_iter()
        .map(|(epoch, stakes)| (epoch, stakes.into()))
        .collect();
    let restored = Bank::new_from_snapshot(
        BankRc {
            accounts: Arc::clone(&f.bank.rc.accounts),
            parent: RwLock::new(None),
            bank_id_generator: Arc::clone(&f.bank.rc.bank_id_generator),
        },
        &f.genesis_config,
        Arc::default(),
        fields,
        Some(*f.bank.leader()),
        None,
        f.bank.load_accounts_data_size(),
        pubkeys_by_owner(&f.bank.stakes_cache.stakes()),
        epoch_stakes,
    );
    assert!(restored.is_frozen());
    restored.freeze();
    let banks = f.notifier.banks.lock().unwrap();
    let observed = &banks[&restored.bank_id()];
    assert!(observed.updates.is_empty());
    assert!(observed.count.is_none());
}
