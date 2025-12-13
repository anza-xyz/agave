#![cfg(test)]
use {
    super::{
        test_utils::{goto_end_of_slot, update_vote_account_timestamp},
        *,
    },
    crate::{
        accounts_background_service::{PrunedBanksRequestHandler, SendDroppedBankCallback},
        bank_client::BankClient,
        bank_forks::BankForks,
        genesis_utils::{
            self, activate_all_features, activate_feature, bootstrap_validator_stake_lamports,
            create_genesis_config_with_leader, create_genesis_config_with_vote_accounts,
            create_lockup_stake_account, genesis_sysvar_and_builtin_program_lamports,
            GenesisConfigInfo, ValidatorVoteKeypairs,
        },
        stake_history::StakeHistory,
        stake_utils,
        stakes::InvalidCacheEntryReason,
        status_cache::MAX_CACHE_ENTRIES,
    },
    agave_feature_set::{self as feature_set, FeatureSet},
    agave_reserved_account_keys::ReservedAccount,
    agave_transaction_view::static_account_keys_frame::MAX_STATIC_ACCOUNTS_PER_PACKET,
    ahash::AHashMap,
    assert_matches::assert_matches,
    crossbeam_channel::{bounded, unbounded},
    ed25519_dalek::ed25519::signature::Signer as EdSigner,
    itertools::Itertools,
    rand::Rng,
    rayon::{iter::IntoParallelIterator, ThreadPool, ThreadPoolBuilder},
    serde::{Deserialize, Serialize},
    solana_account::{
        accounts_equal, create_account_shared_data_with_fields as create_account, from_account,
        state_traits::StateMut, Account, AccountSharedData, ReadableAccount, WritableAccount,
    },
    solana_account_info::MAX_PERMITTED_DATA_INCREASE,
    solana_accounts_db::{
        accounts::AccountAddressFilter,
        accounts_index::{
            AccountIndex, AccountSecondaryIndexes, IndexKey, ScanConfig, ScanError, ITER_BATCH_SIZE,
        },
        ancestors::Ancestors,
    },
    solana_client_traits::SyncClient,
    solana_clock::{
        BankId, Epoch, Slot, UnixTimestamp, DEFAULT_TICKS_PER_SLOT, INITIAL_RENT_EPOCH,
        MAX_PROCESSING_AGE, MAX_RECENT_BLOCKHASHES,
    },
    solana_cluster_type::ClusterType,
    solana_compute_budget::{
        compute_budget::ComputeBudget, compute_budget_limits::ComputeBudgetLimits,
    },
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_cost_model::block_cost_limits::{
        MAX_BLOCK_UNITS, MAX_BLOCK_UNITS_SIMD_0286, MAX_WRITABLE_ACCOUNT_UNITS,
    },
    solana_cpi::MAX_RETURN_DATA,
    solana_epoch_schedule::{EpochSchedule, MINIMUM_SLOTS_PER_EPOCH},
    solana_feature_gate_interface::{self as feature, Feature},
    solana_fee_calculator::FeeRateGovernor,
    solana_fee_structure::FeeStructure,
    solana_genesis_config::GenesisConfig,
    solana_hash::Hash,
    solana_instruction::{error::InstructionError, AccountMeta, Instruction},
    solana_keypair::{keypair_from_seed, Keypair},
    solana_loader_v3_interface::{
        instruction::UpgradeableLoaderInstruction, state::UpgradeableLoaderState,
    },
    solana_loader_v4_interface::{instruction as loader_v4, state::LoaderV4State},
    solana_message::{
        compiled_instruction::CompiledInstruction, Message, MessageHeader, SanitizedMessage,
    },
    solana_native_token::LAMPORTS_PER_SOL,
    solana_nonce::{self as nonce, state::DurableNonce},
    solana_packet::PACKET_DATA_SIZE,
    solana_poh_config::PohConfig,
    solana_program_runtime::{
        declare_process_instruction,
        execution_budget::{self, MAX_COMPUTE_UNIT_LIMIT},
        loaded_programs::{ProgramCacheEntry, ProgramCacheEntryType},
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_reward_info::RewardType,
    solana_sdk_ids::{
        bpf_loader, bpf_loader_upgradeable, ed25519_program, incinerator, native_loader,
        secp256k1_program,
    },
    solana_sha256_hasher::hash,
    solana_signature::Signature,
    solana_signer::Signer,
    solana_stake_interface::{
        instruction as stake_instruction, program as stake_program,
        state::{Authorized, Delegation, Lockup, Stake, StakeStateV2},
    },
    solana_svm::{
        account_loader::{FeesOnlyTransaction, LoadedTransaction, TRANSACTION_ACCOUNT_BASE_SIZE},
        rollback_accounts::RollbackAccounts,
        transaction_commit_result::TransactionCommitResultExtensions,
        transaction_execution_result::ExecutedTransaction,
    },
    solana_svm_timings::ExecuteTimings,
    solana_svm_transaction::svm_message::SVMMessage,
    solana_system_interface::{
        error::SystemError,
        instruction::{self as system_instruction},
        program as system_program, MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION,
        MAX_PERMITTED_DATA_LENGTH,
    },
    solana_system_transaction as system_transaction, solana_sysvar as sysvar,
    solana_transaction::{
        sanitized::SanitizedTransaction, Transaction, TransactionVerificationMode,
    },
    solana_transaction_error::{TransactionError, TransactionResult as Result},
    solana_vote_interface::state::TowerSync,
    solana_vote_program::{
        vote_instruction,
        vote_state::{
            self, create_v4_account_with_authorized, BlockTimestamp, VoteAuthorize, VoteInit,
            VoteStateV4, VoteStateVersions, MAX_LOCKOUT_HISTORY,
        },
    },
    spl_generic_token::token,
    std::{
        collections::{BTreeMap, HashMap, HashSet},
        convert::TryInto,
        fs::File,
        io::Read,
        str::FromStr,
        sync::{
            atomic::{
                AtomicBool, AtomicU64, AtomicUsize,
                Ordering::{Relaxed, Release},
            },
            Arc,
        },
        thread::Builder,
        time::{Duration, Instant},
    },
    test_case::test_case,
};

impl VoteReward {
    pub fn new_random() -> Self {
        let mut rng = rand::rng();

        let validator_pubkey = solana_pubkey::new_rand();
        let validator_stake_lamports = rng.random_range(1..200);
        let validator_voting_keypair = Keypair::new();
        let commission: u8 = rng.random_range(1..20);
        let commission_bps = u16::from(commission) * 100;

        let validator_vote_account = vote_state::create_v4_account_with_authorized(
            &validator_pubkey,
            &validator_voting_keypair.pubkey(),
            &validator_voting_keypair.pubkey(),
            None,
            commission_bps,
            validator_stake_lamports,
        );

        Self {
            vote_account: validator_vote_account,
            commission,
            vote_rewards: rng.random_range(1..200),
        }
    }
}

fn create_genesis_config_no_tx_fee_no_rent(lamports: u64) -> (GenesisConfig, Keypair) {
    // genesis_util creates config with no tx fee and no rent
    let genesis_config_info = solana_runtime::genesis_utils::create_genesis_config(lamports);
    (
        genesis_config_info.genesis_config,
        genesis_config_info.mint_keypair,
    )
}

fn create_genesis_config_no_tx_fee(lamports: u64) -> (GenesisConfig, Keypair) {
    // genesis_config creates config with default fee rate and default rent
    // override to set fee rate to zero.
    let (mut genesis_config, mint_keypair) = solana_genesis_config::create_genesis_config(lamports);
    genesis_config.fee_rate_governor = FeeRateGovernor::new(0, 0);
    (genesis_config, mint_keypair)
}

pub(in crate::bank) fn create_genesis_config(lamports: u64) -> (GenesisConfig, Keypair) {
    solana_genesis_config::create_genesis_config(lamports)
}

pub(in crate::bank) fn new_sanitized_message(message: Message) -> SanitizedMessage {
    SanitizedMessage::try_from_legacy_message(message, &ReservedAccountKeys::empty_key_set())
        .unwrap()
}

#[test]
fn test_race_register_tick_freeze() {
    agave_logger::setup();

    let (mut genesis_config, _) = create_genesis_config(50);
    genesis_config.ticks_per_slot = 1;
    let p = solana_pubkey::new_rand();
    let hash = hash(p.as_ref());

    for _ in 0..1000 {
        let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
        let bank0_ = bank0.clone();
        let freeze_thread = Builder::new()
            .name("freeze".to_string())
            .spawn(move || loop {
                if bank0_.is_complete() {
                    assert_eq!(bank0_.last_blockhash(), hash);
                    break;
                }
            })
            .unwrap();

        let bank0_ = bank0.clone();
        let register_tick_thread = Builder::new()
            .name("register_tick".to_string())
            .spawn(move || {
                bank0_.register_tick_for_test(&hash);
            })
            .unwrap();

        register_tick_thread.join().unwrap();
        freeze_thread.join().unwrap();
    }
}

fn new_executed_processing_result(
    status: Result<()>,
    fee_details: FeeDetails,
) -> TransactionProcessingResult {
    Ok(ProcessedTransaction::Executed(Box::new(
        ExecutedTransaction {
            loaded_transaction: LoadedTransaction {
                fee_details,
                ..LoadedTransaction::default()
            },
            execution_details: TransactionExecutionDetails {
                status,
                log_messages: None,
                inner_instructions: None,
                return_data: None,
                executed_units: 0,
                accounts_data_len_delta: 0,
            },
            programs_modified_by_tx: HashMap::new(),
        },
    )))
}

impl Bank {
    fn clean_accounts_for_tests(&self) {
        self.rc.accounts.accounts_db.clean_accounts_for_tests()
    }
}

#[test]
fn test_bank_unix_timestamp_from_genesis() {
    let (genesis_config, _mint_keypair) = create_genesis_config(1);
    let mut bank = Arc::new(Bank::new_for_tests(&genesis_config));

    assert_eq!(
        genesis_config.creation_time,
        bank.unix_timestamp_from_genesis()
    );
    let slots_per_sec = (genesis_config.poh_config.target_tick_duration.as_secs_f32()
        * genesis_config.ticks_per_slot as f32)
        .recip();

    for _i in 0..slots_per_sec as usize + 1 {
        bank = Arc::new(new_from_parent(bank));
    }

    assert!(bank.unix_timestamp_from_genesis() - genesis_config.creation_time >= 1);
}

#[test]
fn test_bank_new() {
    let dummy_leader_pubkey = solana_pubkey::new_rand();
    let dummy_leader_stake_lamports = bootstrap_validator_stake_lamports();
    let mint_lamports = 10_000;
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        voting_keypair,
        ..
    } = create_genesis_config_with_leader(
        mint_lamports,
        &dummy_leader_pubkey,
        dummy_leader_stake_lamports,
    );

    genesis_config.rent = Rent {
        lamports_per_byte_year: 5,
        exemption_threshold: 1.2,
        burn_percent: 5,
    };

    let bank = Bank::new_for_tests(&genesis_config);
    assert_eq!(bank.get_balance(&mint_keypair.pubkey()), mint_lamports);
    assert_eq!(
        bank.get_balance(&voting_keypair.pubkey()),
        dummy_leader_stake_lamports /* 1 token goes to the vote account associated with dummy_leader_lamports */
    );

    let rent_account = bank.get_account(&sysvar::rent::id()).unwrap();
    let rent = from_account::<sysvar::rent::Rent, _>(&rent_account).unwrap();

    assert_eq!(rent.burn_percent, 5);
    assert_eq!(rent.exemption_threshold, 1.2);
    assert_eq!(rent.lamports_per_byte_year, 5);
}

pub(crate) fn create_simple_test_bank(lamports: u64) -> Bank {
    let (genesis_config, _mint_keypair) = create_genesis_config(lamports);
    Bank::new_for_tests(&genesis_config)
}

fn create_simple_test_arc_bank(lamports: u64) -> (Arc<Bank>, Arc<RwLock<BankForks>>) {
    let bank = create_simple_test_bank(lamports);
    bank.wrap_with_bank_forks_for_tests()
}

#[test]
fn test_bank_block_height() {
    let (bank0, _bank_forks) = create_simple_test_arc_bank(1);
    assert_eq!(bank0.block_height(), 0);
    let bank1 = Arc::new(new_from_parent(bank0));
    assert_eq!(bank1.block_height(), 1);
}

#[test]
fn test_bank_update_epoch_stakes() {
    #[allow(non_local_definitions)]
    impl Bank {
        fn epoch_stake_keys(&self) -> Vec<Epoch> {
            let mut keys: Vec<Epoch> = self.epoch_stakes.keys().copied().collect();
            keys.sort_unstable();
            keys
        }

        fn epoch_stake_key_info(&self) -> (Epoch, Epoch, usize) {
            let mut keys: Vec<Epoch> = self.epoch_stakes.keys().copied().collect();
            keys.sort_unstable();
            (*keys.first().unwrap(), *keys.last().unwrap(), keys.len())
        }
    }

    let mut bank = create_simple_test_bank(100_000);

    let initial_epochs = bank.epoch_stake_keys();
    assert_eq!(initial_epochs, vec![0, 1]);

    for existing_epoch in &initial_epochs {
        bank.update_epoch_stakes(*existing_epoch);
        assert_eq!(bank.epoch_stake_keys(), initial_epochs);
    }

    for epoch in (initial_epochs.len() as Epoch)..(MAX_LEADER_SCHEDULE_STAKES - 1) {
        bank.update_epoch_stakes(dbg!(epoch));
        assert_eq!(bank.epoch_stakes.len() as Epoch, epoch + 1);
    }

    assert_eq!(
        bank.epoch_stake_key_info(),
        (
            0,
            MAX_LEADER_SCHEDULE_STAKES - 2,
            MAX_LEADER_SCHEDULE_STAKES as usize - 1,
        )
    );

    bank.update_epoch_stakes(MAX_LEADER_SCHEDULE_STAKES - 1);
    assert_eq!(
        bank.epoch_stake_key_info(),
        (
            0,
            MAX_LEADER_SCHEDULE_STAKES - 1,
            MAX_LEADER_SCHEDULE_STAKES as usize,
        )
    );

    bank.update_epoch_stakes(MAX_LEADER_SCHEDULE_STAKES);
    assert_eq!(
        bank.epoch_stake_key_info(),
        (
            1,
            MAX_LEADER_SCHEDULE_STAKES,
            MAX_LEADER_SCHEDULE_STAKES as usize,
        )
    );
}

fn bank0_sysvar_delta() -> u64 {
    const SLOT_HISTORY_SYSVAR_MIN_BALANCE: u64 = 913_326_000;
    SLOT_HISTORY_SYSVAR_MIN_BALANCE
}

fn bank1_sysvar_delta() -> u64 {
    const SLOT_HASHES_SYSVAR_MIN_BALANCE: u64 = 143_487_360;
    SLOT_HASHES_SYSVAR_MIN_BALANCE
}

fn bank2_sysvar_delta() -> u64 {
    const EPOCH_REWARDS_SYSVAR_MIN_BALANCE: u64 = 1_454_640;
    EPOCH_REWARDS_SYSVAR_MIN_BALANCE
}

#[test]
fn test_bank_capitalization() {
    let bank0 = Arc::new(Bank::new_for_tests(&GenesisConfig {
        accounts: (0..42)
            .map(|_| {
                (
                    solana_pubkey::new_rand(),
                    Account::new(42, 0, &Pubkey::default()),
                )
            })
            .collect(),
        cluster_type: ClusterType::MainnetBeta,
        ..GenesisConfig::default()
    }));

    assert_eq!(
        bank0.capitalization(),
        42 * 42 + genesis_sysvar_and_builtin_program_lamports(),
    );

    bank0.freeze();

    assert_eq!(
        bank0.capitalization(),
        42 * 42 + genesis_sysvar_and_builtin_program_lamports() + bank0_sysvar_delta(),
    );

    let bank1 = Bank::new_from_parent(bank0, &Pubkey::default(), 1);
    assert_eq!(
        bank1.capitalization(),
        42 * 42
            + genesis_sysvar_and_builtin_program_lamports()
            + bank0_sysvar_delta()
            + bank1_sysvar_delta(),
    );
}

/// if asserter returns true, check the capitalization
/// Checking the capitalization requires that the bank be a root and the slot be flushed.
/// All tests are getting converted to use the write cache, so over time, each caller will be visited to throttle this input.
/// Flushing the cache has a side effects on the test, so often the test has to be restarted to continue to function correctly.
fn assert_capitalization_diff(
    bank: &Bank,
    updater: impl Fn(),
    asserter: impl Fn(u64, u64) -> bool,
) {
    let old = bank.capitalization();
    updater();
    let new = bank.capitalization();
    if asserter(old, new) {
        add_root_and_flush_write_cache(bank);
        assert_eq!(
            bank.capitalization(),
            bank.calculate_capitalization_for_tests()
        );
    }
}

declare_process_instruction!(MockBuiltin, 1, |_invoke_context| {
    // Default for all tests which don't bring their own processor
    Ok(())
});

#[test]
fn test_store_account_and_update_capitalization_missing() {
    let bank = create_simple_test_bank(0);
    let pubkey = solana_pubkey::new_rand();

    let some_lamports = 400;
    let account = AccountSharedData::new(some_lamports, 0, &system_program::id());

    assert_capitalization_diff(
        &bank,
        || bank.store_account_and_update_capitalization(&pubkey, &account),
        |old, new| {
            assert_eq!(old + some_lamports, new);
            true
        },
    );
    assert_eq!(account, bank.get_account(&pubkey).unwrap());
}

#[test]
fn test_store_account_and_update_capitalization_increased() {
    let old_lamports = 400;
    let (genesis_config, mint_keypair) = create_genesis_config(old_lamports);
    let bank = Bank::new_for_tests(&genesis_config);
    let pubkey = mint_keypair.pubkey();

    let new_lamports = 500;
    let account = AccountSharedData::new(new_lamports, 0, &system_program::id());

    assert_capitalization_diff(
        &bank,
        || bank.store_account_and_update_capitalization(&pubkey, &account),
        |old, new| {
            assert_eq!(old + 100, new);
            true
        },
    );
    assert_eq!(account, bank.get_account(&pubkey).unwrap());
}

#[test]
fn test_store_account_and_update_capitalization_decreased() {
    let old_lamports = 400;
    let (genesis_config, mint_keypair) = create_genesis_config(old_lamports);
    let bank = Bank::new_for_tests(&genesis_config);
    let pubkey = mint_keypair.pubkey();

    let new_lamports = 100;
    let account = AccountSharedData::new(new_lamports, 0, &system_program::id());

    assert_capitalization_diff(
        &bank,
        || bank.store_account_and_update_capitalization(&pubkey, &account),
        |old, new| {
            assert_eq!(old - 300, new);
            true
        },
    );
    assert_eq!(account, bank.get_account(&pubkey).unwrap());
}

#[test]
fn test_store_account_and_update_capitalization_unchanged() {
    let lamports = 400;
    let (genesis_config, mint_keypair) = create_genesis_config(lamports);
    let bank = Bank::new_for_tests(&genesis_config);
    let pubkey = mint_keypair.pubkey();

    let account = AccountSharedData::new(lamports, 1, &system_program::id());

    assert_capitalization_diff(
        &bank,
        || bank.store_account_and_update_capitalization(&pubkey, &account),
        |old, new| {
            assert_eq!(old, new);
            true
        },
    );
    assert_eq!(account, bank.get_account(&pubkey).unwrap());
}

/// Ensure that store_account_and_update_capitalization() correctly updates accounts_data_size
#[test]
fn test_store_account_and_update_capitalization_accounts_data_size() {
    let (genesis_config, _mint_keypair) = create_genesis_config(100 * LAMPORTS_PER_SOL);
    let bank = Bank::new_for_tests(&genesis_config);

    let data_size = 123;
    let mut account = AccountSharedData::new(LAMPORTS_PER_SOL, data_size, &system_program::id());
    let address = Pubkey::new_unique();

    // test 1: store a new account
    let accounts_data_size_pre = bank.load_accounts_data_size();
    bank.store_account_and_update_capitalization(&address, &account);
    let accounts_data_size_post = bank.load_accounts_data_size();
    assert_eq!(
        accounts_data_size_pre + data_size as u64,
        accounts_data_size_post,
    );

    // test 2: change the account's data
    let data_size_delta = 42;
    account.set_data(vec![0; data_size + data_size_delta]);
    let accounts_data_size_pre = bank.load_accounts_data_size();
    bank.store_account_and_update_capitalization(&address, &account);
    let accounts_data_size_post = bank.load_accounts_data_size();
    assert_eq!(
        accounts_data_size_pre + data_size_delta as u64,
        accounts_data_size_post,
    );

    // test 3: close the account
    account.set_lamports(0);
    let accounts_data_size_pre = bank.load_accounts_data_size();
    bank.store_account_and_update_capitalization(&address, &account);
    let accounts_data_size_post = bank.load_accounts_data_size();
    assert_eq!(
        accounts_data_size_pre - (data_size + data_size_delta) as u64,
        accounts_data_size_post,
    );
}

pub(in crate::bank) fn new_from_parent_next_epoch(
    parent: Arc<Bank>,
    bank_forks: &RwLock<BankForks>,
    epochs: Epoch,
) -> Arc<Bank> {
    let mut slot = parent.slot();
    let mut epoch = parent.epoch();
    for _ in 0..epochs {
        slot += parent.epoch_schedule().get_slots_in_epoch(epoch);
        epoch = parent.epoch_schedule().get_epoch(slot);
    }

    Bank::new_from_parent_with_bank_forks(bank_forks, parent, &Pubkey::default(), slot)
}

#[test]
fn test_bank_update_vote_stake_rewards() {
    let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    check_bank_update_vote_stake_rewards(|bank: &Bank| {
        bank._load_vote_and_stake_accounts(&thread_pool, null_tracer())
    });
}

impl Bank {
    fn _load_vote_and_stake_accounts(
        &self,
        thread_pool: &ThreadPool,
        reward_calc_tracer: Option<impl RewardCalcTracer>,
    ) -> StakeDelegationsMap {
        let stakes = self.stakes_cache.stakes();
        let stake_delegations = stakes.stake_delegations_vec();
        let stake_delegations = self.filter_stake_delegations(stake_delegations);
        // Obtain all unique voter pubkeys from stake delegations.
        fn merge(mut acc: HashSet<Pubkey>, other: HashSet<Pubkey>) -> HashSet<Pubkey> {
            if acc.len() < other.len() {
                return merge(other, acc);
            }
            acc.extend(other);
            acc
        }
        let voter_pubkeys = thread_pool.install(|| {
            stake_delegations
                .par_iter()
                .filter_map(|stake_delegation| stake_delegation)
                .fold(
                    HashSet::default,
                    |mut voter_pubkeys, (_stake_pubkey, stake_account)| {
                        voter_pubkeys.insert(stake_account.delegation().voter_pubkey);
                        voter_pubkeys
                    },
                )
                .reduce(HashSet::default, merge)
        });
        // Obtain vote-accounts for unique voter pubkeys.
        let cached_vote_accounts = stakes.vote_accounts();
        let solana_vote_program: Pubkey = solana_vote_program::id();
        let vote_accounts_cache_miss_count = AtomicUsize::default();
        let get_vote_account = |vote_pubkey: &Pubkey| -> Option<VoteAccount> {
            if let Some(vote_account) = cached_vote_accounts.get(vote_pubkey) {
                return Some(vote_account.clone());
            }
            // If accounts-db contains a valid vote account, then it should
            // already have been cached in cached_vote_accounts; so the code
            // below is only for sanity check in tests, and should not be hit
            // in practice.
            let account = self.get_account_with_fixed_root(vote_pubkey)?;
            if account.owner() == &solana_vote_program
                && VoteStateV4::deserialize(account.data(), vote_pubkey).is_ok()
            {
                vote_accounts_cache_miss_count.fetch_add(1, Relaxed);
            }
            VoteAccount::try_from(account).ok()
        };
        let invalid_vote_keys = DashMap::<Pubkey, InvalidCacheEntryReason>::new();
        let make_vote_delegations_entry = |vote_pubkey| {
            let Some(vote_account) = get_vote_account(&vote_pubkey) else {
                invalid_vote_keys.insert(vote_pubkey, InvalidCacheEntryReason::Missing);
                return None;
            };
            if vote_account.owner() != &solana_vote_program {
                invalid_vote_keys.insert(vote_pubkey, InvalidCacheEntryReason::WrongOwner);
                return None;
            }
            let stake_delegations = Vec::default();
            Some((vote_pubkey, stake_delegations))
        };
        let stake_delegations_map: DashMap<Pubkey, StakeDelegations> = thread_pool.install(|| {
            voter_pubkeys
                .into_par_iter()
                .filter_map(make_vote_delegations_entry)
                .collect()
        });
        // Join stake accounts with vote-accounts.
        let push_stake_delegation = |(stake_pubkey, stake_account): (&Pubkey, &StakeAccount<_>)| {
            let delegation = stake_account.delegation();
            let Some(mut vote_delegations) =
                stake_delegations_map.get_mut(&delegation.voter_pubkey)
            else {
                return;
            };
            if let Some(reward_calc_tracer) = reward_calc_tracer.as_ref() {
                let delegation =
                    InflationPointCalculationEvent::Delegation(*delegation, solana_vote_program);
                let event = RewardCalculationEvent::Staking(stake_pubkey, &delegation);
                reward_calc_tracer(&event);
            }
            let stake_delegation = (*stake_pubkey, stake_account.clone());
            vote_delegations.push(stake_delegation);
        };
        thread_pool.install(|| {
            stake_delegations
                .par_iter()
                .filter_map(|stake_delegation| stake_delegation)
                .for_each(push_stake_delegation);
        });
        stake_delegations_map
    }
}

type StakeDelegations = Vec<(Pubkey, StakeAccount<Delegation>)>;
type StakeDelegationsMap = DashMap<Pubkey, StakeDelegations>;

#[cfg(test)]
fn check_bank_update_vote_stake_rewards<F>(load_vote_and_stake_accounts: F)
where
    F: Fn(&Bank) -> StakeDelegationsMap,
{
    agave_logger::setup();

    // create a bank that ticks really slowly...
    let bank0 = Arc::new(Bank::new_for_tests(&GenesisConfig {
        accounts: (0..42)
            .map(|_| {
                (
                    solana_pubkey::new_rand(),
                    Account::new(1_000_000_000, 0, &Pubkey::default()),
                )
            })
            .collect(),
        // set it up so the first epoch is a full year long
        poh_config: PohConfig {
            target_tick_duration: Duration::from_secs(
                SECONDS_PER_YEAR as u64 / MINIMUM_SLOTS_PER_EPOCH / DEFAULT_TICKS_PER_SLOT,
            ),
            hashes_per_tick: None,
            target_tick_count: None,
        },
        cluster_type: ClusterType::MainnetBeta,

        ..GenesisConfig::default()
    }));

    assert_eq!(
        bank0.capitalization(),
        42 * 1_000_000_000 + genesis_sysvar_and_builtin_program_lamports(),
    );

    let ((vote_id, mut vote_account), (stake_id, stake_account)) =
        crate::stakes::tests::create_staked_node_accounts(10_000);
    let starting_vote_and_stake_balance = 10_000 + 1;

    // set up accounts
    bank0.store_account_and_update_capitalization(&stake_id, &stake_account);

    // generate some rewards
    let mut vote_state = Some(VoteStateV4::deserialize(vote_account.data(), &vote_id).unwrap());
    for i in 0..MAX_LOCKOUT_HISTORY + 42 {
        if let Some(v) = vote_state.as_mut() {
            vote_state::process_slot_vote_unchecked(v, i as u64)
        }
        let versioned = VoteStateVersions::V4(Box::new(vote_state.take().unwrap()));
        vote_account.set_state(&versioned).unwrap();
        bank0.store_account_and_update_capitalization(&vote_id, &vote_account);
        match versioned {
            VoteStateVersions::V4(v) => {
                vote_state = Some(*v);
            }
            _ => panic!("Has to be of type V4"),
        };
    }
    bank0.store_account_and_update_capitalization(&vote_id, &vote_account);
    bank0.freeze();

    assert_eq!(
        bank0.capitalization(),
        42 * 1_000_000_000
            + genesis_sysvar_and_builtin_program_lamports()
            + starting_vote_and_stake_balance
            + bank0_sysvar_delta(),
    );
    assert!(bank0.rewards.read().unwrap().is_empty());

    load_vote_and_stake_accounts(&bank0);

    // put a child bank in epoch 1
    let bank1 = Arc::new(Bank::new_from_parent(
        bank0.clone(),
        &Pubkey::default(),
        bank0.get_slots_in_epoch(bank0.epoch()) + 1,
    ));

    // verify that there's inflation
    assert_ne!(bank1.capitalization(), bank0.capitalization());

    // check voting rewards show up in rewards vector
    assert_eq!(
        *bank1.rewards.read().unwrap(),
        vec![(
            vote_id,
            RewardInfo {
                reward_type: RewardType::Voting,
                lamports: 0,
                post_balance: bank1.get_balance(&vote_id),
                commission: Some(0),
            }
        ),]
    );
    bank1.freeze();

    // advance past partitioned epoch staking rewards delivery
    let bank2 = Arc::new(Bank::new_from_parent(
        bank1.clone(),
        &Pubkey::default(),
        bank1.slot() + 1,
    ));
    // verify that there's inflation
    assert_ne!(bank2.capitalization(), bank0.capitalization());

    // verify the inflation is represented in validator_points
    let paid_rewards = bank2.capitalization()
        - bank0.capitalization()
        - bank1_sysvar_delta()
        - bank2_sysvar_delta();

    // this assumes that no new builtins or precompiles were activated in bank1 or bank2
    let PrevEpochInflationRewards {
        validator_rewards, ..
    } = bank2.calculate_previous_epoch_inflation_rewards(bank0.capitalization(), bank0.epoch());

    // verify the stake and vote accounts are the right size
    assert!(
        ((bank2.get_balance(&stake_id) - stake_account.lamports() + bank2.get_balance(&vote_id)
            - vote_account.lamports()) as f64
            - validator_rewards as f64)
            .abs()
            < 1.0
    );

    // verify the rewards are the right size
    assert!((validator_rewards as f64 - paid_rewards as f64).abs() < 1.0); // rounding, truncating

    // verify validator rewards show up in rewards vectors
    assert_eq!(
        *bank2.rewards.read().unwrap(),
        vec![(
            stake_id,
            RewardInfo {
                reward_type: RewardType::Staking,
                lamports: validator_rewards as i64,
                post_balance: bank2.get_balance(&stake_id),
                commission: Some(0),
            }
        )]
    );
    bank2.freeze();
    add_root_and_flush_write_cache(&bank0);
    add_root_and_flush_write_cache(&bank1);
    add_root_and_flush_write_cache(&bank2);
    assert_eq!(
        bank2.capitalization(),
        bank2.calculate_capitalization_for_tests()
    );
}

fn do_test_bank_update_rewards_determinism() -> u64 {
    // create a bank that ticks really slowly...
    let bank = Arc::new(Bank::new_for_tests(&GenesisConfig {
        accounts: (0..42)
            .map(|_| {
                (
                    solana_pubkey::new_rand(),
                    Account::new(1_000_000_000, 0, &Pubkey::default()),
                )
            })
            .collect(),
        // set it up so the first epoch is a full year long
        poh_config: PohConfig {
            target_tick_duration: Duration::from_secs(
                SECONDS_PER_YEAR as u64 / MINIMUM_SLOTS_PER_EPOCH / DEFAULT_TICKS_PER_SLOT,
            ),
            hashes_per_tick: None,
            target_tick_count: None,
        },
        cluster_type: ClusterType::MainnetBeta,

        ..GenesisConfig::default()
    }));

    assert_eq!(
        bank.capitalization(),
        42 * 1_000_000_000 + genesis_sysvar_and_builtin_program_lamports()
    );

    let vote_id = solana_pubkey::new_rand();
    let node_pubkey = solana_pubkey::new_rand();
    let mut vote_account = vote_state::create_v4_account_with_authorized(
        &node_pubkey,
        &vote_id,
        &vote_id,
        None,
        0,
        100,
    );
    let stake_id1 = solana_pubkey::new_rand();
    let stake_account1 = crate::stakes::tests::create_stake_account(123, &vote_id, &stake_id1);
    let stake_id2 = solana_pubkey::new_rand();
    let stake_account2 = crate::stakes::tests::create_stake_account(456, &vote_id, &stake_id2);

    // set up accounts
    bank.store_account_and_update_capitalization(&stake_id1, &stake_account1);
    bank.store_account_and_update_capitalization(&stake_id2, &stake_account2);

    // generate some rewards
    let mut vote_state = Some(VoteStateV4::deserialize(vote_account.data(), &vote_id).unwrap());
    for i in 0..MAX_LOCKOUT_HISTORY + 42 {
        if let Some(v) = vote_state.as_mut() {
            vote_state::process_slot_vote_unchecked(v, i as u64)
        }
        let versioned = VoteStateVersions::V4(Box::new(vote_state.take().unwrap()));
        vote_account.set_state(&versioned).unwrap();
        bank.store_account_and_update_capitalization(&vote_id, &vote_account);
        match versioned {
            VoteStateVersions::V4(v) => {
                vote_state = Some(*v);
            }
            _ => panic!("Has to be of type V4"),
        };
    }
    bank.store_account_and_update_capitalization(&vote_id, &vote_account);

    // put a child bank in epoch 1, which calls update_rewards()...
    let bank1 = Arc::new(Bank::new_from_parent(
        bank.clone(),
        &Pubkey::default(),
        bank.get_slots_in_epoch(bank.epoch()) + 1,
    ));
    // verify that there's inflation
    assert_ne!(bank1.capitalization(), bank.capitalization());

    bank1.freeze();
    add_root_and_flush_write_cache(&bank);
    add_root_and_flush_write_cache(&bank1);
    assert_eq!(
        bank1.capitalization(),
        bank1.calculate_capitalization_for_tests()
    );

    // verify voting and staking rewards are recorded
    let rewards = bank1.rewards.read().unwrap();
    rewards
        .iter()
        .find(|(_address, reward)| reward.reward_type == RewardType::Voting)
        .unwrap();

    // put another child bank, since partitioned staking rewards are delivered
    // after the epoch-boundary slot
    let bank2 = Bank::new_from_parent(bank1.clone(), &Pubkey::default(), bank1.slot() + 1);
    let rewards = bank2.rewards.read().unwrap();
    rewards
        .iter()
        .find(|(_address, reward)| reward.reward_type == RewardType::Staking)
        .unwrap();

    bank1.capitalization()
}

#[test]
fn test_bank_update_rewards_determinism() {
    agave_logger::setup();

    // The same reward should be distributed given same credits
    let expected_capitalization = do_test_bank_update_rewards_determinism();
    // Repeat somewhat large number of iterations to expose possible different behavior
    // depending on the randomly-seeded HashMap ordering
    for _ in 0..30 {
        let actual_capitalization = do_test_bank_update_rewards_determinism();
        assert_eq!(actual_capitalization, expected_capitalization);
    }
}

impl VerifyAccountsHashConfig {
    fn default_for_test() -> Self {
        Self {
            require_rooted_bank: false,
        }
    }
}

// Test that purging 0 lamports accounts works.
#[test]
fn test_purge_empty_accounts() {
    // When using the write cache, flushing is destructive/cannot be undone
    //  so we have to stop at various points and restart to actively test.
    for pass in 0..3 {
        agave_logger::setup();
        let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee(LAMPORTS_PER_SOL);
        let amount = genesis_config.rent.minimum_balance(0);
        let (mut bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();

        for _ in 0..10 {
            let blockhash = bank.last_blockhash();
            let pubkey = solana_pubkey::new_rand();
            let tx = system_transaction::transfer(&mint_keypair, &pubkey, 0, blockhash);
            bank.process_transaction(&tx).unwrap();
            bank.freeze();
            bank.squash();
            bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
        }

        bank.freeze();
        bank.squash();
        bank.force_flush_accounts_cache();
        let hash = bank.calculate_accounts_lt_hash_for_tests();
        bank.clean_accounts_for_tests();
        assert_eq!(bank.calculate_accounts_lt_hash_for_tests(), hash);

        let bank0 = new_from_parent_with_fork_next_slot(bank.clone(), bank_forks.as_ref());
        let blockhash = bank.last_blockhash();
        let keypair = Keypair::new();
        let tx = system_transaction::transfer(&mint_keypair, &keypair.pubkey(), amount, blockhash);
        bank0.process_transaction(&tx).unwrap();

        let bank1 = new_from_parent_with_fork_next_slot(bank0.clone(), bank_forks.as_ref());
        let pubkey = solana_pubkey::new_rand();
        let blockhash = bank.last_blockhash();
        let tx = system_transaction::transfer(&keypair, &pubkey, amount, blockhash);
        bank1.process_transaction(&tx).unwrap();

        assert_eq!(
            bank0.get_account(&keypair.pubkey()).unwrap().lamports(),
            amount
        );
        assert_eq!(bank1.get_account(&keypair.pubkey()), None);

        info!("bank0 purge");
        let hash = bank0.calculate_accounts_lt_hash_for_tests();
        bank0.clean_accounts_for_tests();
        assert_eq!(bank0.calculate_accounts_lt_hash_for_tests(), hash);

        assert_eq!(
            bank0.get_account(&keypair.pubkey()).unwrap().lamports(),
            amount
        );
        assert_eq!(bank1.get_account(&keypair.pubkey()), None);

        info!("bank1 purge");
        bank1.clean_accounts_for_tests();

        assert_eq!(
            bank0.get_account(&keypair.pubkey()).unwrap().lamports(),
            amount
        );
        assert_eq!(bank1.get_account(&keypair.pubkey()), None);

        if pass == 0 {
            add_root_and_flush_write_cache(&bank0);
            assert!(bank0.verify_accounts(VerifyAccountsHashConfig::default_for_test(), None));
            continue;
        }

        // Squash and then verify hash_internal value
        bank0.freeze();
        bank0.squash();
        add_root_and_flush_write_cache(&bank0);
        if pass == 1 {
            assert!(bank0.verify_accounts(VerifyAccountsHashConfig::default_for_test(), None));
            continue;
        }

        bank1.freeze();
        bank1.squash();
        add_root_and_flush_write_cache(&bank1);
        assert!(bank1.verify_accounts(VerifyAccountsHashConfig::default_for_test(), None));

        // keypair should have 0 tokens on both forks
        assert_eq!(bank0.get_account(&keypair.pubkey()), None);
        assert_eq!(bank1.get_account(&keypair.pubkey()), None);

        bank1.clean_accounts_for_tests();

        assert!(bank1.verify_accounts(VerifyAccountsHashConfig::default_for_test(), None));
    }
}

#[test]
fn test_two_payments_to_one_party() {
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let pubkey = solana_pubkey::new_rand();
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let amount = genesis_config.rent.minimum_balance(0);
    assert_eq!(bank.last_blockhash(), genesis_config.hash());

    bank.transfer(amount, &mint_keypair, &pubkey).unwrap();
    assert_eq!(bank.get_balance(&pubkey), amount);

    bank.transfer(amount * 2, &mint_keypair, &pubkey).unwrap();
    assert_eq!(bank.get_balance(&pubkey), amount * 3);
    assert_eq!(bank.transaction_count(), 2);
    assert_eq!(bank.non_vote_transaction_count_since_restart(), 2);
}

#[test]
fn test_one_source_two_tx_one_batch() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee(LAMPORTS_PER_SOL);
    let key1 = solana_pubkey::new_rand();
    let key2 = solana_pubkey::new_rand();
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let amount = genesis_config.rent.minimum_balance(0);
    assert_eq!(bank.last_blockhash(), genesis_config.hash());

    let t1 = system_transaction::transfer(&mint_keypair, &key1, amount, genesis_config.hash());
    let t2 = system_transaction::transfer(&mint_keypair, &key2, amount, genesis_config.hash());
    let txs = [t1.clone(), t2.clone()];
    let res = bank.process_transactions(txs.iter());

    assert_eq!(res.len(), 2);
    assert_eq!(res[0], Ok(()));
    assert_eq!(res[1], Err(TransactionError::AccountInUse));
    assert_eq!(
        bank.get_balance(&mint_keypair.pubkey()),
        LAMPORTS_PER_SOL - amount
    );
    assert_eq!(bank.get_balance(&key1), amount);
    assert_eq!(bank.get_balance(&key2), 0);
    assert_eq!(bank.get_signature_status(&t1.signatures[0]), Some(Ok(())));
    // TODO: Transactions that fail to pay a fee could be dropped silently.
    // Non-instruction errors don't get logged in the signature cache
    assert_eq!(bank.get_signature_status(&t2.signatures[0]), None);
}

#[test]
fn test_one_tx_two_out_atomic_fail() {
    let amount = LAMPORTS_PER_SOL;
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee_no_rent(amount);
    let key1 = solana_pubkey::new_rand();
    let key2 = solana_pubkey::new_rand();
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let instructions = system_instruction::transfer_many(
        &mint_keypair.pubkey(),
        &[(key1, amount), (key2, amount)],
    );
    let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
    let tx = Transaction::new(&[&mint_keypair], message, genesis_config.hash());
    assert_eq!(
        bank.process_transaction(&tx).unwrap_err(),
        TransactionError::InstructionError(1, SystemError::ResultWithNegativeLamports.into())
    );
    assert_eq!(bank.get_balance(&mint_keypair.pubkey()), amount);
    assert_eq!(bank.get_balance(&key1), 0);
    assert_eq!(bank.get_balance(&key2), 0);
}

#[test]
fn test_one_tx_two_out_atomic_pass() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee(LAMPORTS_PER_SOL);
    let key1 = solana_pubkey::new_rand();
    let key2 = solana_pubkey::new_rand();
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let amount = genesis_config.rent.minimum_balance(0);
    let instructions = system_instruction::transfer_many(
        &mint_keypair.pubkey(),
        &[(key1, amount), (key2, amount)],
    );
    let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
    let tx = Transaction::new(&[&mint_keypair], message, genesis_config.hash());
    bank.process_transaction(&tx).unwrap();
    assert_eq!(
        bank.get_balance(&mint_keypair.pubkey()),
        LAMPORTS_PER_SOL - (2 * amount)
    );
    assert_eq!(bank.get_balance(&key1), amount);
    assert_eq!(bank.get_balance(&key2), amount);
}

// This test demonstrates that fees are paid even when a program fails.
#[test]
fn test_detect_failed_duplicate_transactions() {
    let (mut genesis_config, mint_keypair) = create_genesis_config(10_000);
    genesis_config.fee_rate_governor = FeeRateGovernor::new(5_000, 0);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    let dest = Keypair::new();

    // source with 0 program context
    let tx =
        system_transaction::transfer(&mint_keypair, &dest.pubkey(), 10_000, genesis_config.hash());
    let signature = tx.signatures[0];
    assert!(!bank.has_signature(&signature));

    assert_eq!(
        bank.process_transaction(&tx),
        Err(TransactionError::InstructionError(
            0,
            SystemError::ResultWithNegativeLamports.into(),
        ))
    );

    // The lamports didn't move, but the from address paid the transaction fee.
    assert_eq!(bank.get_balance(&dest.pubkey()), 0);

    // This should be the original balance minus the transaction fee.
    assert_eq!(bank.get_balance(&mint_keypair.pubkey()), 5000);
}

#[test]
fn test_account_not_found() {
    agave_logger::setup();
    let (genesis_config, mint_keypair) = create_genesis_config(0);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let keypair = Keypair::new();
    assert_eq!(
        bank.transfer(
            genesis_config.rent.minimum_balance(0),
            &keypair,
            &mint_keypair.pubkey()
        ),
        Err(TransactionError::AccountNotFound)
    );
    assert_eq!(bank.transaction_count(), 0);
    assert_eq!(bank.non_vote_transaction_count_since_restart(), 0);
}

#[test]
fn test_insufficient_funds() {
    let mint_amount = LAMPORTS_PER_SOL;
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee(mint_amount);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let pubkey = solana_pubkey::new_rand();
    let amount = genesis_config.rent.minimum_balance(0);
    bank.transfer(amount, &mint_keypair, &pubkey).unwrap();
    assert_eq!(bank.transaction_count(), 1);
    assert_eq!(bank.non_vote_transaction_count_since_restart(), 1);
    assert_eq!(bank.get_balance(&pubkey), amount);
    assert_eq!(
        bank.transfer((mint_amount - amount) + 1, &mint_keypair, &pubkey),
        Err(TransactionError::InstructionError(
            0,
            SystemError::ResultWithNegativeLamports.into(),
        ))
    );
    // transaction_count returns the count of all committed transactions since
    // bank_transaction_count_fix was activated, regardless of success
    assert_eq!(bank.transaction_count(), 2);
    assert_eq!(bank.non_vote_transaction_count_since_restart(), 2);

    let mint_pubkey = mint_keypair.pubkey();
    assert_eq!(bank.get_balance(&mint_pubkey), mint_amount - amount);
    assert_eq!(bank.get_balance(&pubkey), amount);
}

#[test]
fn test_executed_transaction_count_post_bank_transaction_count_fix() {
    let mint_amount = LAMPORTS_PER_SOL;
    let (genesis_config, mint_keypair) = create_genesis_config(mint_amount);
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let pubkey = solana_pubkey::new_rand();
    let amount = genesis_config.rent.minimum_balance(0);
    bank.transfer(amount, &mint_keypair, &pubkey).unwrap();
    assert_eq!(
        bank.transfer((mint_amount - amount) + 1, &mint_keypair, &pubkey),
        Err(TransactionError::InstructionError(
            0,
            SystemError::ResultWithNegativeLamports.into(),
        ))
    );

    // With bank_transaction_count_fix, transaction_count should include both the successful and
    // failed transactions.
    assert_eq!(bank.transaction_count(), 2);
    assert_eq!(bank.executed_transaction_count(), 2);
    assert_eq!(bank.transaction_error_count(), 1);

    let bank2 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank,
        &Pubkey::default(),
        genesis_config.epoch_schedule.first_normal_slot,
    );

    assert_eq!(
        bank2.transfer((mint_amount - amount) + 2, &mint_keypair, &pubkey),
        Err(TransactionError::InstructionError(
            0,
            SystemError::ResultWithNegativeLamports.into(),
        ))
    );

    // The transaction_count inherited from parent bank is 3: 2 from the parent bank and 1 at this bank2
    assert_eq!(bank2.transaction_count(), 3);
    assert_eq!(bank2.executed_transaction_count(), 1);
    assert_eq!(bank2.transaction_error_count(), 1);
}

#[test]
fn test_transfer_to_newb() {
    agave_logger::setup();
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let amount = genesis_config.rent.minimum_balance(0);
    let pubkey = solana_pubkey::new_rand();
    bank.transfer(amount, &mint_keypair, &pubkey).unwrap();
    assert_eq!(bank.get_balance(&pubkey), amount);
}

#[test]
fn test_transfer_to_sysvar() {
    agave_logger::setup();
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let amount = genesis_config.rent.minimum_balance(0);

    let normal_pubkey = solana_pubkey::new_rand();
    let sysvar_pubkey = sysvar::clock::id();
    assert_eq!(bank.get_balance(&normal_pubkey), 0);
    assert_eq!(bank.get_balance(&sysvar_pubkey), 1_169_280);

    bank.transfer(amount, &mint_keypair, &normal_pubkey)
        .unwrap();
    bank.transfer(amount, &mint_keypair, &sysvar_pubkey)
        .unwrap_err();
    assert_eq!(bank.get_balance(&normal_pubkey), amount);
    assert_eq!(bank.get_balance(&sysvar_pubkey), 1_169_280);

    let bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
    assert_eq!(bank.get_balance(&normal_pubkey), amount);
    assert_eq!(bank.get_balance(&sysvar_pubkey), 1_169_280);
}

#[test]
fn test_bank_withdraw() {
    let bank = create_simple_test_bank(100);

    // Test no account
    let key = solana_pubkey::new_rand();
    assert_eq!(
        bank.withdraw(&key, 10),
        Err(TransactionError::AccountNotFound)
    );

    test_utils::deposit(&bank, &key, 3).unwrap();
    assert_eq!(bank.get_balance(&key), 3);

    // Low balance
    assert_eq!(
        bank.withdraw(&key, 10),
        Err(TransactionError::InsufficientFundsForFee)
    );

    // Enough balance
    assert_eq!(bank.withdraw(&key, 2), Ok(()));
    assert_eq!(bank.get_balance(&key), 1);
}

#[test]
fn test_bank_withdraw_from_nonce_account() {
    let (mut genesis_config, _mint_keypair) = create_genesis_config(100_000);
    genesis_config.rent.lamports_per_byte_year = 42;
    let bank = Bank::new_for_tests(&genesis_config);

    let min_balance = bank.get_minimum_balance_for_rent_exemption(nonce::state::State::size());
    let nonce = Keypair::new();
    let nonce_account = AccountSharedData::new_data(
        min_balance + 42,
        &nonce::versions::Versions::new(nonce::state::State::Initialized(
            nonce::state::Data::default(),
        )),
        &system_program::id(),
    )
    .unwrap();
    bank.store_account(&nonce.pubkey(), &nonce_account);
    assert_eq!(bank.get_balance(&nonce.pubkey()), min_balance + 42);

    // Resulting in non-zero, but sub-min_balance balance fails
    assert_eq!(
        bank.withdraw(&nonce.pubkey(), min_balance / 2),
        Err(TransactionError::InsufficientFundsForFee)
    );
    assert_eq!(bank.get_balance(&nonce.pubkey()), min_balance + 42);

    // Resulting in exactly rent-exempt balance succeeds
    bank.withdraw(&nonce.pubkey(), 42).unwrap();
    assert_eq!(bank.get_balance(&nonce.pubkey()), min_balance);

    // Account closure fails
    assert_eq!(
        bank.withdraw(&nonce.pubkey(), min_balance),
        Err(TransactionError::InsufficientFundsForFee),
    );
}

#[test]
fn test_bank_tx_fee() {
    agave_logger::setup();

    let arbitrary_transfer_amount = 42_000;
    let mint = arbitrary_transfer_amount * 100;
    let leader = solana_pubkey::new_rand();
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(mint, &leader, 3);
    genesis_config.fee_rate_governor = FeeRateGovernor::new(5000, 0); // something divisible by 2

    let expected_fee_paid = genesis_config
        .fee_rate_governor
        .create_fee_calculator()
        .lamports_per_signature;
    let (expected_fee_collected, expected_fee_burned) =
        genesis_config.fee_rate_governor.burn(expected_fee_paid);

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    let capitalization = bank.capitalization();

    let key = solana_pubkey::new_rand();
    let tx = system_transaction::transfer(
        &mint_keypair,
        &key,
        arbitrary_transfer_amount,
        bank.last_blockhash(),
    );

    let initial_balance = bank.get_balance(&leader);
    assert_eq!(bank.process_transaction(&tx), Ok(()));
    assert_eq!(bank.get_balance(&key), arbitrary_transfer_amount);
    assert_eq!(
        bank.get_balance(&mint_keypair.pubkey()),
        mint - arbitrary_transfer_amount - expected_fee_paid
    );

    assert_eq!(bank.get_balance(&leader), initial_balance);
    goto_end_of_slot(bank.clone());
    assert_eq!(bank.signature_count(), 1);
    assert_eq!(
        bank.get_balance(&leader),
        initial_balance + expected_fee_collected
    ); // Leader collects fee after the bank is frozen

    // verify capitalization
    let sysvar_and_builtin_program_delta = 1;
    assert_eq!(
        capitalization - expected_fee_burned + sysvar_and_builtin_program_delta,
        bank.capitalization()
    );

    assert_eq!(
        *bank.rewards.read().unwrap(),
        vec![(
            leader,
            RewardInfo {
                reward_type: RewardType::Fee,
                lamports: expected_fee_collected as i64,
                post_balance: initial_balance + expected_fee_collected,
                commission: None,
            }
        )]
    );

    // Verify that an InstructionError collects fees, too
    let bank = Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank, &leader, 1);
    let mut tx = system_transaction::transfer(&mint_keypair, &key, 1, bank.last_blockhash());
    // Create a bogus instruction to system_program to cause an instruction error
    tx.message.instructions[0].data[0] = 40;

    bank.process_transaction(&tx)
        .expect_err("instruction error");
    assert_eq!(bank.get_balance(&key), arbitrary_transfer_amount); // no change
    assert_eq!(
        bank.get_balance(&mint_keypair.pubkey()),
        mint - arbitrary_transfer_amount - 2 * expected_fee_paid
    ); // mint_keypair still pays a fee
    goto_end_of_slot(bank.clone());
    assert_eq!(bank.signature_count(), 1);

    // Profit! 2 transaction signatures processed at 3 lamports each
    assert_eq!(
        bank.get_balance(&leader),
        initial_balance + 2 * expected_fee_collected
    );

    assert_eq!(
        *bank.rewards.read().unwrap(),
        vec![(
            leader,
            RewardInfo {
                reward_type: RewardType::Fee,
                lamports: expected_fee_collected as i64,
                post_balance: initial_balance + 2 * expected_fee_collected,
                commission: None,
            }
        )]
    );
}

#[test]
fn test_bank_tx_compute_unit_fee() {
    agave_logger::setup();

    let key = solana_pubkey::new_rand();
    let arbitrary_transfer_amount = 42;
    let mint = arbitrary_transfer_amount * 10_000_000;
    let leader = solana_pubkey::new_rand();
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(mint, &leader, 3);
    genesis_config.fee_rate_governor = FeeRateGovernor::new(4, 0); // something divisible by 2

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    let expected_fee_paid = calculate_test_fee(
        &new_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique()))),
        genesis_config
            .fee_rate_governor
            .create_fee_calculator()
            .lamports_per_signature,
        bank.fee_structure(),
    );

    let (expected_fee_collected, expected_fee_burned) =
        genesis_config.fee_rate_governor.burn(expected_fee_paid);

    let capitalization = bank.capitalization();

    let tx = system_transaction::transfer(
        &mint_keypair,
        &key,
        arbitrary_transfer_amount,
        bank.last_blockhash(),
    );

    let initial_balance = bank.get_balance(&leader);
    assert_eq!(bank.process_transaction(&tx), Ok(()));
    assert_eq!(bank.get_balance(&key), arbitrary_transfer_amount);
    assert_eq!(
        bank.get_balance(&mint_keypair.pubkey()),
        mint - arbitrary_transfer_amount - expected_fee_paid
    );

    assert_eq!(bank.get_balance(&leader), initial_balance);
    goto_end_of_slot(bank.clone());
    assert_eq!(bank.signature_count(), 1);
    assert_eq!(
        bank.get_balance(&leader),
        initial_balance + expected_fee_collected
    ); // Leader collects fee after the bank is frozen

    // verify capitalization
    let sysvar_and_builtin_program_delta = 1;
    assert_eq!(
        capitalization - expected_fee_burned + sysvar_and_builtin_program_delta,
        bank.capitalization()
    );

    assert_eq!(
        *bank.rewards.read().unwrap(),
        vec![(
            leader,
            RewardInfo {
                reward_type: RewardType::Fee,
                lamports: expected_fee_collected as i64,
                post_balance: initial_balance + expected_fee_collected,
                commission: None,
            }
        )]
    );

    // Verify that an InstructionError collects fees, too
    let bank = Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank, &leader, 1);
    let mut tx = system_transaction::transfer(&mint_keypair, &key, 1, bank.last_blockhash());
    // Create a bogus instruction to system_program to cause an instruction error
    tx.message.instructions[0].data[0] = 40;

    bank.process_transaction(&tx)
        .expect_err("instruction error");
    assert_eq!(bank.get_balance(&key), arbitrary_transfer_amount); // no change
    assert_eq!(
        bank.get_balance(&mint_keypair.pubkey()),
        mint - arbitrary_transfer_amount - 2 * expected_fee_paid
    ); // mint_keypair still pays a fee
    goto_end_of_slot(bank.clone());
    assert_eq!(bank.signature_count(), 1);

    // Profit! 2 transaction signatures processed at 3 lamports each
    assert_eq!(
        bank.get_balance(&leader),
        initial_balance + 2 * expected_fee_collected
    );

    assert_eq!(
        *bank.rewards.read().unwrap(),
        vec![(
            leader,
            RewardInfo {
                reward_type: RewardType::Fee,
                lamports: expected_fee_collected as i64,
                post_balance: initial_balance + 2 * expected_fee_collected,
                commission: None,
            }
        )]
    );
}

#[test]
fn test_bank_blockhash_fee_structure() {
    //agave_logger::setup();

    let leader = solana_pubkey::new_rand();
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(1_000_000, &leader, 3);
    genesis_config
        .fee_rate_governor
        .target_lamports_per_signature = 5000;
    genesis_config.fee_rate_governor.target_signatures_per_slot = 0;

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    goto_end_of_slot(bank.clone());
    let cheap_blockhash = bank.last_blockhash();
    let cheap_lamports_per_signature = bank.get_lamports_per_signature();
    assert_eq!(cheap_lamports_per_signature, 0);

    let bank = Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank, &leader, 1);
    goto_end_of_slot(bank.clone());
    let expensive_blockhash = bank.last_blockhash();
    let expensive_lamports_per_signature = bank.get_lamports_per_signature();
    assert!(cheap_lamports_per_signature < expensive_lamports_per_signature);

    let bank = Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank, &leader, 2);

    // Send a transfer using cheap_blockhash
    let key = solana_pubkey::new_rand();
    let initial_mint_balance = bank.get_balance(&mint_keypair.pubkey());
    let tx = system_transaction::transfer(&mint_keypair, &key, 1, cheap_blockhash);
    assert_eq!(bank.process_transaction(&tx), Ok(()));
    assert_eq!(bank.get_balance(&key), 1);
    let cheap_fee = calculate_test_fee(
        &new_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique()))),
        cheap_lamports_per_signature,
        bank.fee_structure(),
    );
    assert_eq!(
        bank.get_balance(&mint_keypair.pubkey()),
        initial_mint_balance - 1 - cheap_fee
    );

    // Send a transfer using expensive_blockhash
    let key = solana_pubkey::new_rand();
    let initial_mint_balance = bank.get_balance(&mint_keypair.pubkey());
    let tx = system_transaction::transfer(&mint_keypair, &key, 1, expensive_blockhash);
    assert_eq!(bank.process_transaction(&tx), Ok(()));
    assert_eq!(bank.get_balance(&key), 1);
    let expensive_fee = calculate_test_fee(
        &new_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique()))),
        expensive_lamports_per_signature,
        bank.fee_structure(),
    );
    assert_eq!(
        bank.get_balance(&mint_keypair.pubkey()),
        initial_mint_balance - 1 - expensive_fee
    );
}

#[test]
fn test_bank_blockhash_compute_unit_fee_structure() {
    //agave_logger::setup();

    let leader = solana_pubkey::new_rand();
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(1_000_000_000, &leader, 3);
    genesis_config
        .fee_rate_governor
        .target_lamports_per_signature = 1000;
    genesis_config.fee_rate_governor.target_signatures_per_slot = 1;

    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    goto_end_of_slot(bank.clone());
    let cheap_blockhash = bank.last_blockhash();
    let cheap_lamports_per_signature = bank.get_lamports_per_signature();
    assert_eq!(cheap_lamports_per_signature, 0);

    let bank = Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank, &leader, 1);
    goto_end_of_slot(bank.clone());
    let expensive_blockhash = bank.last_blockhash();
    let expensive_lamports_per_signature = bank.get_lamports_per_signature();
    assert!(cheap_lamports_per_signature < expensive_lamports_per_signature);

    let bank = Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank, &leader, 2);

    // Send a transfer using cheap_blockhash
    let key = solana_pubkey::new_rand();
    let initial_mint_balance = bank.get_balance(&mint_keypair.pubkey());
    let tx = system_transaction::transfer(&mint_keypair, &key, 1, cheap_blockhash);
    assert_eq!(bank.process_transaction(&tx), Ok(()));
    assert_eq!(bank.get_balance(&key), 1);
    let cheap_fee = calculate_test_fee(
        &new_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique()))),
        cheap_lamports_per_signature,
        bank.fee_structure(),
    );
    assert_eq!(
        bank.get_balance(&mint_keypair.pubkey()),
        initial_mint_balance - 1 - cheap_fee
    );

    // Send a transfer using expensive_blockhash
    let key = solana_pubkey::new_rand();
    let initial_mint_balance = bank.get_balance(&mint_keypair.pubkey());
    let tx = system_transaction::transfer(&mint_keypair, &key, 1, expensive_blockhash);
    assert_eq!(bank.process_transaction(&tx), Ok(()));
    assert_eq!(bank.get_balance(&key), 1);
    let expensive_fee = calculate_test_fee(
        &new_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique()))),
        expensive_lamports_per_signature,
        bank.fee_structure(),
    );
    assert_eq!(
        bank.get_balance(&mint_keypair.pubkey()),
        initial_mint_balance - 1 - expensive_fee
    );
}

#[test]
fn test_debits_before_credits() {
    let (genesis_config, mint_keypair) =
        create_genesis_config_no_tx_fee_no_rent(2 * LAMPORTS_PER_SOL);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let keypair = Keypair::new();
    let tx0 = system_transaction::transfer(
        &keypair,
        &mint_keypair.pubkey(),
        LAMPORTS_PER_SOL,
        genesis_config.hash(),
    );
    let tx1 = system_transaction::transfer(
        &mint_keypair,
        &keypair.pubkey(),
        2 * LAMPORTS_PER_SOL,
        genesis_config.hash(),
    );
    let txs = [tx0, tx1];
    let results = bank.process_transactions(txs.iter());
    assert!(results[0].is_err());

    // Assert bad transactions aren't counted.
    assert_eq!(bank.transaction_count(), 1);
    assert_eq!(bank.non_vote_transaction_count_since_restart(), 1);
}

#[test_case(false; "old")]
#[test_case(true; "simd83")]
fn test_readonly_accounts(relax_intrabatch_account_locks: bool) {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(500, &solana_pubkey::new_rand(), 0);
    let mut bank = Bank::new_for_tests(&genesis_config);
    if !relax_intrabatch_account_locks {
        bank.deactivate_feature(&feature_set::relax_intrabatch_account_locks::id());
    }

    let next_slot = bank.slot() + 1;
    let bank = Bank::new_from_parent(Arc::new(bank), &Pubkey::default(), next_slot);
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();

    let vote_pubkey0 = solana_pubkey::new_rand();
    let vote_pubkey1 = solana_pubkey::new_rand();
    let vote_pubkey2 = solana_pubkey::new_rand();
    let authorized_voter = Keypair::new();
    let payer0 = Keypair::new();
    let payer1 = Keypair::new();

    // Create vote accounts
    let vote_account0 = vote_state::create_v4_account_with_authorized(
        &vote_pubkey0,
        &authorized_voter.pubkey(),
        &authorized_voter.pubkey(),
        None,
        0,
        100,
    );
    let vote_account1 = vote_state::create_v4_account_with_authorized(
        &vote_pubkey1,
        &authorized_voter.pubkey(),
        &authorized_voter.pubkey(),
        None,
        0,
        100,
    );
    let vote_account2 = vote_state::create_v4_account_with_authorized(
        &vote_pubkey2,
        &authorized_voter.pubkey(),
        &authorized_voter.pubkey(),
        None,
        0,
        100,
    );
    bank.store_account(&vote_pubkey0, &vote_account0);
    bank.store_account(&vote_pubkey1, &vote_account1);
    bank.store_account(&vote_pubkey2, &vote_account2);

    // Fund payers
    bank.transfer(10, &mint_keypair, &payer0.pubkey()).unwrap();
    bank.transfer(10, &mint_keypair, &payer1.pubkey()).unwrap();
    bank.transfer(1, &mint_keypair, &authorized_voter.pubkey())
        .unwrap();

    let vote = TowerSync::new_from_slot(bank.parent_slot, bank.parent_hash);
    let ix0 = vote_instruction::tower_sync(&vote_pubkey0, &authorized_voter.pubkey(), vote.clone());
    let tx0 = Transaction::new_signed_with_payer(
        &[ix0],
        Some(&payer0.pubkey()),
        &[&payer0, &authorized_voter],
        bank.last_blockhash(),
    );
    let ix1 = vote_instruction::tower_sync(&vote_pubkey1, &authorized_voter.pubkey(), vote.clone());
    let tx1 = Transaction::new_signed_with_payer(
        &[ix1],
        Some(&payer1.pubkey()),
        &[&payer1, &authorized_voter],
        bank.last_blockhash(),
    );
    let txs = [tx0, tx1];
    let results = bank.process_transactions(txs.iter());

    // If multiple transactions attempt to read the same account, they should succeed.
    // Vote authorized_voter and sysvar accounts are given read-only handling
    assert_eq!(results[0], Ok(()));
    assert_eq!(results[1], Ok(()));

    let ix0 = vote_instruction::tower_sync(&vote_pubkey2, &authorized_voter.pubkey(), vote);
    let tx0 = Transaction::new_signed_with_payer(
        &[ix0],
        Some(&payer0.pubkey()),
        &[&payer0, &authorized_voter],
        bank.last_blockhash(),
    );
    let tx1 = system_transaction::transfer(
        &authorized_voter,
        &solana_pubkey::new_rand(),
        1,
        bank.last_blockhash(),
    );
    let txs = [tx0, tx1];
    let results = bank.process_transactions(txs.iter());
    // Whether an account can be locked as read-only and writable at the same time depends on features.
    assert_eq!(results[0], Ok(()));
    assert_eq!(
        results[1],
        if relax_intrabatch_account_locks {
            Ok(())
        } else {
            Err(TransactionError::AccountInUse)
        }
    );
}

#[test]
fn test_interleaving_locks() {
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let alice = Keypair::new();
    let bob = Keypair::new();
    let amount = genesis_config.rent.minimum_balance(0);

    let tx1 = system_transaction::transfer(
        &mint_keypair,
        &alice.pubkey(),
        amount,
        genesis_config.hash(),
    );
    let pay_alice = vec![tx1];

    let lock_result = bank.prepare_batch_for_tests(pay_alice);
    let commit_results = bank
        .load_execute_and_commit_transactions(
            &lock_result,
            MAX_PROCESSING_AGE,
            ExecutionRecordingConfig::new_single_setting(false),
            &mut ExecuteTimings::default(),
            None,
        )
        .0;
    assert!(commit_results[0].is_ok());

    // try executing an interleaved transfer twice
    assert_eq!(
        bank.transfer(amount, &mint_keypair, &bob.pubkey()),
        Err(TransactionError::AccountInUse)
    );
    // the second time should fail as well
    // this verifies that `unlock_accounts` doesn't unlock `AccountInUse` accounts
    assert_eq!(
        bank.transfer(amount, &mint_keypair, &bob.pubkey()),
        Err(TransactionError::AccountInUse)
    );

    drop(lock_result);

    assert!(bank
        .transfer(2 * amount, &mint_keypair, &bob.pubkey())
        .is_ok());
}

#[test]
fn test_load_and_execute_commit_transactions_fees_only() {
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = genesis_utils::create_genesis_config(100 * LAMPORTS_PER_SOL);
    genesis_config.rent = Rent::default();
    genesis_config.fee_rate_governor = FeeRateGovernor::new(5000, 0);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let bank = Bank::new_from_parent(
        bank,
        &Pubkey::new_unique(),
        genesis_config.epoch_schedule.get_first_slot_in_epoch(1),
    );

    // Use rent-paying fee payer to show that rent is not collected for fees
    // only transactions even when they use a rent-paying account.
    let rent_paying_fee_payer = Pubkey::new_unique();
    bank.store_account(
        &rent_paying_fee_payer,
        &AccountSharedData::new(
            genesis_config.rent.minimum_balance(0) - 1,
            0,
            &system_program::id(),
        ),
    );

    // Use nonce to show that loaded account stats also included loaded
    // nonce account size
    let nonce_size = nonce::state::State::size();
    let nonce_balance = genesis_config.rent.minimum_balance(nonce_size);
    let nonce_pubkey = Pubkey::new_unique();
    let nonce_authority = rent_paying_fee_payer;
    let nonce_initial_hash = DurableNonce::from_blockhash(&Hash::new_unique());
    let nonce_data = nonce::state::Data::new(nonce_authority, nonce_initial_hash, 5000);
    let nonce_account = AccountSharedData::new_data(
        nonce_balance,
        &nonce::versions::Versions::new(nonce::state::State::Initialized(nonce_data.clone())),
        &system_program::id(),
    )
    .unwrap();
    bank.store_account(&nonce_pubkey, &nonce_account);

    // Invoke missing program to trigger load error in order to commit a
    // fees-only transaction
    let missing_program_id = Pubkey::new_unique();
    let transaction = Transaction::new_unsigned(Message::new_with_blockhash(
        &[
            system_instruction::advance_nonce_account(&nonce_pubkey, &rent_paying_fee_payer),
            Instruction::new_with_bincode(missing_program_id, &0, vec![]),
        ],
        Some(&rent_paying_fee_payer),
        &nonce_data.blockhash(),
    ));

    let batch = bank.prepare_batch_for_tests(vec![transaction]);
    let commit_results = bank
        .load_execute_and_commit_transactions(
            &batch,
            MAX_PROCESSING_AGE,
            ExecutionRecordingConfig::new_single_setting(true),
            &mut ExecuteTimings::default(),
            None,
        )
        .0;

    assert_eq!(
        commit_results,
        vec![Ok(CommittedTransaction {
            status: Err(TransactionError::ProgramAccountNotFound),
            log_messages: None,
            inner_instructions: None,
            return_data: None,
            executed_units: 0,
            fee_details: FeeDetails::new(5000, 0),
            loaded_account_stats: TransactionLoadedAccountsStats {
                loaded_accounts_count: 2,
                loaded_accounts_data_size: nonce_size as u32,
            },
            fee_payer_post_balance: genesis_config.rent.minimum_balance(0) - 1 - 5000,
        })]
    );
}

#[test]
fn test_load_and_execute_commit_transactions_failure() {
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = genesis_utils::create_genesis_config(100 * LAMPORTS_PER_SOL);
    genesis_config.rent = Rent::default();
    genesis_config.fee_rate_governor = FeeRateGovernor::new(5000, 0);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let bank = Bank::new_from_parent(
        bank,
        &Pubkey::new_unique(),
        genesis_config.epoch_schedule.get_first_slot_in_epoch(1),
    );

    let fee_payer = Pubkey::new_unique();
    let starting_balance = 2 * genesis_config.rent.minimum_balance(0) + 10_000;
    bank.store_account(
        &fee_payer,
        &AccountSharedData::new(starting_balance, 0, &system_program::id()),
    );

    let recipient = Pubkey::new_unique();
    let transfer_amount = genesis_config.rent.minimum_balance(0);

    // Invoke transaction with valid system-program instruction followed by a
    // failing instruction to trigger a failed execution.
    // The system transfer is used to modify the loaded account state to verify the
    // fee payer post balance is correct.
    let transaction = Transaction::new_unsigned(Message::new_with_blockhash(
        &[
            system_instruction::transfer(&fee_payer, &recipient, transfer_amount),
            Instruction::new_with_bincode(system_program::id(), &(), vec![]),
        ],
        Some(&fee_payer),
        &bank.last_blockhash(),
    ));

    let batch = bank.prepare_batch_for_tests(vec![transaction]);
    let commit_results = bank
        .load_execute_and_commit_transactions(
            &batch,
            MAX_PROCESSING_AGE,
            ExecutionRecordingConfig::new_single_setting(true),
            &mut ExecuteTimings::default(),
            None,
        )
        .0;

    assert_eq!(
        commit_results,
        vec![Ok(CommittedTransaction {
            status: Err(TransactionError::InstructionError(
                1,
                InstructionError::InvalidInstructionData
            )),
            log_messages: Some(vec![
                "Program 11111111111111111111111111111111 invoke [1]".to_string(),
                "Program 11111111111111111111111111111111 success".to_string(),
                "Program 11111111111111111111111111111111 invoke [1]".to_string(),
                "Program 11111111111111111111111111111111 failed: invalid instruction data"
                    .to_string()
            ]),
            inner_instructions: Some(vec![vec![], vec![]]),
            return_data: None,
            executed_units: 300,
            fee_details: FeeDetails::new(5000, 0),
            loaded_account_stats: TransactionLoadedAccountsStats {
                loaded_accounts_count: 3,
                loaded_accounts_data_size: 142, // size of system account (initially recipient does not exist)
            },
            fee_payer_post_balance: starting_balance - 5000,
        })]
    );
}

#[test]
fn test_load_and_execute_commit_transactions_success() {
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = genesis_utils::create_genesis_config(100 * LAMPORTS_PER_SOL);
    genesis_config.rent = Rent::default();
    genesis_config.fee_rate_governor = FeeRateGovernor::new(5000, 0);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let bank = Bank::new_from_parent(
        bank,
        &Pubkey::new_unique(),
        genesis_config.epoch_schedule.get_first_slot_in_epoch(1),
    );

    let fee_payer = Pubkey::new_unique();
    let starting_balance = 2 * genesis_config.rent.minimum_balance(0) + 10_000;
    bank.store_account(
        &fee_payer,
        &AccountSharedData::new(starting_balance, 0, &system_program::id()),
    );

    let recipient = Pubkey::new_unique();
    let transfer_amount = genesis_config.rent.minimum_balance(0);

    // Invoke transaction with valid system-program instruction to trigger
    // a successful execution
    let transaction = Transaction::new_unsigned(Message::new_with_blockhash(
        &[system_instruction::transfer(
            &fee_payer,
            &recipient,
            transfer_amount,
        )],
        Some(&fee_payer),
        &bank.last_blockhash(),
    ));

    let batch = bank.prepare_batch_for_tests(vec![transaction]);
    let commit_results = bank
        .load_execute_and_commit_transactions(
            &batch,
            MAX_PROCESSING_AGE,
            ExecutionRecordingConfig::new_single_setting(true),
            &mut ExecuteTimings::default(),
            None,
        )
        .0;

    assert_eq!(
        commit_results,
        vec![Ok(CommittedTransaction {
            status: Ok(()),
            log_messages: Some(vec![
                "Program 11111111111111111111111111111111 invoke [1]".to_string(),
                "Program 11111111111111111111111111111111 success".to_string(),
            ]),
            inner_instructions: Some(vec![vec![]]),
            return_data: None,
            executed_units: 150,
            fee_details: FeeDetails::new(5000, 0),
            loaded_account_stats: TransactionLoadedAccountsStats {
                loaded_accounts_count: 3,
                loaded_accounts_data_size: 142, // size of system account (initially recipient does not exist)
            },
            fee_payer_post_balance: starting_balance - 5000 - transfer_amount,
        })]
    );
}

#[test]
fn test_readonly_relaxed_locks() {
    let (genesis_config, _) = create_genesis_config(3);
    let bank = Bank::new_for_tests(&genesis_config);
    let key0 = Keypair::new();
    let key1 = Keypair::new();
    let key2 = Keypair::new();
    let key3 = solana_pubkey::new_rand();

    let message = Message {
        header: MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 1,
        },
        account_keys: vec![key0.pubkey(), key3],
        recent_blockhash: Hash::default(),
        instructions: vec![],
    };
    let tx = Transaction::new(&[&key0], message, genesis_config.hash());
    let txs = vec![tx];

    let batch0 = bank.prepare_batch_for_tests(txs);
    assert!(batch0.lock_results()[0].is_ok());

    // Try locking accounts, locking a previously read-only account as writable
    // should fail
    let message = Message {
        header: MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 0,
        },
        account_keys: vec![key1.pubkey(), key3],
        recent_blockhash: Hash::default(),
        instructions: vec![],
    };
    let tx = Transaction::new(&[&key1], message, genesis_config.hash());
    let txs = vec![tx];

    let batch1 = bank.prepare_batch_for_tests(txs);
    assert!(batch1.lock_results()[0].is_err());

    // Try locking a previously read-only account a 2nd time; should succeed
    let message = Message {
        header: MessageHeader {
            num_required_signatures: 1,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: 1,
        },
        account_keys: vec![key2.pubkey(), key3],
        recent_blockhash: Hash::default(),
        instructions: vec![],
    };
    let tx = Transaction::new(&[&key2], message, genesis_config.hash());
    let txs = vec![tx];

    let batch2 = bank.prepare_batch_for_tests(txs);
    assert!(batch2.lock_results()[0].is_ok());
}

#[test]
fn test_bank_invalid_account_index() {
    let (genesis_config, mint_keypair) = create_genesis_config(1);
    let keypair = Keypair::new();
    let bank = Bank::new_for_tests(&genesis_config);

    let tx =
        system_transaction::transfer(&mint_keypair, &keypair.pubkey(), 1, genesis_config.hash());

    let mut tx_invalid_program_index = tx.clone();
    tx_invalid_program_index.message.instructions[0].program_id_index = 42;
    assert_eq!(
        bank.process_transaction(&tx_invalid_program_index),
        Err(TransactionError::SanitizeFailure)
    );

    let mut tx_invalid_account_index = tx;
    tx_invalid_account_index.message.instructions[0].accounts[0] = 42;
    assert_eq!(
        bank.process_transaction(&tx_invalid_account_index),
        Err(TransactionError::SanitizeFailure)
    );
}

#[test]
fn test_bank_pay_to_self() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee(LAMPORTS_PER_SOL);
    let key1 = Keypair::new();
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let amount = genesis_config.rent.minimum_balance(0);

    bank.transfer(amount, &mint_keypair, &key1.pubkey())
        .unwrap();
    assert_eq!(bank.get_balance(&key1.pubkey()), amount);
    let tx = system_transaction::transfer(&key1, &key1.pubkey(), amount, genesis_config.hash());
    let _res = bank.process_transaction(&tx);

    assert_eq!(bank.get_balance(&key1.pubkey()), amount);
    bank.get_signature_status(&tx.signatures[0])
        .unwrap()
        .unwrap();
}

fn new_from_parent(parent: Arc<Bank>) -> Bank {
    let slot = parent.slot() + 1;
    let leader_id = Pubkey::default();
    Bank::new_from_parent(parent, &leader_id, slot)
}

fn new_from_parent_with_fork_next_slot(parent: Arc<Bank>, fork: &RwLock<BankForks>) -> Arc<Bank> {
    let slot = parent.slot() + 1;
    Bank::new_from_parent_with_bank_forks(fork, parent, &Pubkey::default(), slot)
}

/// Verify that the parent's vector is computed correctly
#[test]
fn test_bank_parents() {
    let (genesis_config, _) = create_genesis_config(1);
    let parent = Arc::new(Bank::new_for_tests(&genesis_config));

    let bank = new_from_parent(parent.clone());
    assert!(Arc::ptr_eq(&bank.parents()[0], &parent));
}

/// Verifies that transactions are dropped if they have already been processed
#[test]
fn test_tx_already_processed() {
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    let key1 = Keypair::new();
    let mut tx = system_transaction::transfer(
        &mint_keypair,
        &key1.pubkey(),
        genesis_config.rent.minimum_balance(0),
        genesis_config.hash(),
    );

    // First process `tx` so that the status cache is updated
    assert_eq!(bank.process_transaction(&tx), Ok(()));

    // Ensure that signature check works
    assert_eq!(
        bank.process_transaction(&tx),
        Err(TransactionError::AlreadyProcessed)
    );

    // Change transaction signature to simulate processing a transaction with a different signature
    // for the same message.
    tx.signatures[0] = Signature::default();

    // Ensure that message hash check works
    assert_eq!(
        bank.process_transaction(&tx),
        Err(TransactionError::AlreadyProcessed)
    );
}

/// Verifies that last ids and status cache are correctly referenced from parent
#[test]
fn test_bank_parent_already_processed() {
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let key1 = Keypair::new();
    let (parent, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let amount = genesis_config.rent.minimum_balance(0);

    let tx =
        system_transaction::transfer(&mint_keypair, &key1.pubkey(), amount, genesis_config.hash());
    assert_eq!(parent.process_transaction(&tx), Ok(()));
    let bank = new_from_parent_with_fork_next_slot(parent, bank_forks.as_ref());
    assert_eq!(
        bank.process_transaction(&tx),
        Err(TransactionError::AlreadyProcessed)
    );
}

/// Verifies that last ids and accounts are correctly referenced from parent
#[test]
fn test_bank_parent_account_spend() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee(LAMPORTS_PER_SOL);
    let key1 = Keypair::new();
    let key2 = Keypair::new();
    let (parent, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let amount = genesis_config.rent.minimum_balance(0);

    let tx =
        system_transaction::transfer(&mint_keypair, &key1.pubkey(), amount, genesis_config.hash());
    assert_eq!(parent.process_transaction(&tx), Ok(()));
    let bank = new_from_parent_with_fork_next_slot(parent.clone(), bank_forks.as_ref());
    let tx = system_transaction::transfer(&key1, &key2.pubkey(), amount, genesis_config.hash());
    assert_eq!(bank.process_transaction(&tx), Ok(()));
    assert_eq!(parent.get_signature_status(&tx.signatures[0]), None);
}

#[test]
fn test_bank_hash_internal_state() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee_no_rent(LAMPORTS_PER_SOL);
    let (bank0, _bank_forks0) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let (bank1, bank_forks1) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    let initial_state = bank0.hash_internal_state();
    assert_eq!(bank1.hash_internal_state(), initial_state);

    // Ensure calling hash_internal_state() again does *not* change the bank hash value
    assert_eq!(bank1.hash_internal_state(), initial_state);

    let amount = genesis_config.rent.minimum_balance(0);
    let pubkey = solana_pubkey::new_rand();
    bank0.transfer(amount, &mint_keypair, &pubkey).unwrap();
    bank0.freeze();
    assert_ne!(bank0.hash_internal_state(), initial_state);
    bank1.transfer(amount, &mint_keypair, &pubkey).unwrap();
    bank1.freeze();
    assert_eq!(bank0.hash_internal_state(), bank1.hash_internal_state());

    // Checkpointing should always result in a new state
    let bank2 = new_from_parent_with_fork_next_slot(bank1, &bank_forks1);
    assert_ne!(bank0.hash_internal_state(), bank2.hash_internal_state());

    let pubkey2 = solana_pubkey::new_rand();
    bank2.transfer(amount, &mint_keypair, &pubkey2).unwrap();
    bank2.squash();
    bank2.force_flush_accounts_cache();
    assert!(bank2.verify_accounts(VerifyAccountsHashConfig::default_for_test(), None));
}

#[test]
fn test_bank_hash_internal_state_verify() {
    for pass in 0..4 {
        let (genesis_config, mint_keypair) =
            create_genesis_config_no_tx_fee_no_rent(LAMPORTS_PER_SOL);
        let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

        let amount = genesis_config.rent.minimum_balance(0);
        let pubkey = solana_pubkey::new_rand();
        bank0.transfer(amount, &mint_keypair, &pubkey).unwrap();

        let bank0_state = bank0.hash_internal_state();
        // Checkpointing should result in a new state while freezing the parent
        let bank2 = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank0.clone(),
            &solana_pubkey::new_rand(),
            2,
        );
        assert_ne!(bank0_state, bank2.hash_internal_state());

        // Checkpointing should modify the checkpoint's state when freezed
        assert_ne!(bank0_state, bank0.hash_internal_state());

        // Checkpointing should never modify the checkpoint's state once frozen
        add_root_and_flush_write_cache(&bank0);
        let bank0_state = bank0.hash_internal_state();
        if pass == 0 {
            // we later modify bank 2, so this flush is destructive to the test
            bank2.freeze();
            add_root_and_flush_write_cache(&bank2);
            assert!(bank2.verify_accounts(VerifyAccountsHashConfig::default_for_test(), None));
        }
        let bank3 = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank0.clone(),
            &solana_pubkey::new_rand(),
            3,
        );
        assert_eq!(bank0_state, bank0.hash_internal_state());
        if pass == 0 {
            // this relies on us having set bank2's accounts hash in the pass==0 if above
            assert!(bank2.verify_accounts(VerifyAccountsHashConfig::default_for_test(), None));
            continue;
        }
        if pass == 1 {
            // flushing slot 3 here causes us to mark it as a root. Marking it as a root
            // prevents us from marking slot 2 as a root later since slot 2 is < slot 3.
            // Doing so throws an assert. So, we can't flush 3 until 2 is flushed.
            bank3.freeze();
            add_root_and_flush_write_cache(&bank3);
            assert!(bank3.verify_accounts(VerifyAccountsHashConfig::default_for_test(), None));
            continue;
        }

        let pubkey2 = solana_pubkey::new_rand();
        bank2.transfer(amount, &mint_keypair, &pubkey2).unwrap();
        bank2.freeze(); // <-- keep freeze() *outside* `if pass == 2 {}`
        if pass == 2 {
            add_root_and_flush_write_cache(&bank2);
            assert!(bank2.verify_accounts(VerifyAccountsHashConfig::default_for_test(), None));

            // Verifying the accounts lt hash is only intended to be called at startup, and
            // normally in the background.  Since here we're *not* at startup, and doing it
            // in the foreground, the verification uses the accounts index.  The test just
            // rooted bank2, and will root bank3 next; but they are on different forks,
            // which is not valid.  This causes the accounts index to see accounts from
            // bank2 and bank3, which causes verifying bank3's accounts lt hash to fail.
            // To workaround this "issue", we cannot root bank2 when the accounts lt hash
            // is enabled.
            continue;
        }

        bank3.freeze();
        add_root_and_flush_write_cache(&bank3);
        assert!(bank3.verify_accounts(VerifyAccountsHashConfig::default_for_test(), None));
    }
}

#[test]
#[should_panic(expected = "self.is_frozen()")]
fn test_verify_hash_unfrozen() {
    let bank = create_simple_test_bank(2_000);
    assert!(bank.verify_hash());
}

#[test]
fn test_verify_snapshot_bank() {
    agave_logger::setup();
    let pubkey = solana_pubkey::new_rand();
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    bank.transfer(
        genesis_config.rent.minimum_balance(0),
        &mint_keypair,
        &pubkey,
    )
    .unwrap();
    bank.freeze();
    add_root_and_flush_write_cache(&bank);
    assert!(bank.verify_snapshot_bank(false, false, bank.slot(), None));

    // tamper the bank after freeze!
    bank.increment_signature_count(1);
    assert!(!bank.verify_snapshot_bank(false, false, bank.slot(), None));
}

// Test that two bank forks with the same transactions should not hash to the same value.
#[test]
fn test_bank_hash_same_transactions_different_fork() {
    agave_logger::setup();
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    bank0.freeze();

    // send the same transfer to both forks
    let pubkey = solana_pubkey::new_rand();
    let amount = genesis_config.rent.minimum_balance(0);

    let bank1 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank0.clone(),
        &Pubkey::default(),
        1,
    );
    bank1.transfer(amount, &mint_keypair, &pubkey).unwrap();
    bank1.freeze();

    let bank2 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank0.clone(),
        &Pubkey::default(),
        2,
    );
    bank2.transfer(amount, &mint_keypair, &pubkey).unwrap();
    bank2.freeze();

    let bank0_hash = bank0.hash();
    let bank1_hash = bank1.hash();
    let bank2_hash = bank2.hash();
    assert_ne!(bank0_hash, bank1_hash);
    assert_ne!(bank0_hash, bank2_hash);
    assert_ne!(bank1_hash, bank2_hash);
}

#[test]
fn test_hash_internal_state_genesis() {
    let bank0 = Bank::new_for_tests(&create_genesis_config(10).0);
    let bank1 = Bank::new_for_tests(&create_genesis_config(20).0);
    assert_ne!(bank0.hash_internal_state(), bank1.hash_internal_state());
}

// See that the order of two transfers does not affect the result
// of hash_internal_state
#[test]
fn test_hash_internal_state_order() {
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let amount = genesis_config.rent.minimum_balance(0);
    let (bank0, _bank_forks0) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let (bank1, _bank_forks1) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    assert_eq!(bank0.hash_internal_state(), bank1.hash_internal_state());
    let key0 = solana_pubkey::new_rand();
    let key1 = solana_pubkey::new_rand();
    bank0.transfer(amount, &mint_keypair, &key0).unwrap();
    bank0.transfer(amount * 2, &mint_keypair, &key1).unwrap();

    bank1.transfer(amount * 2, &mint_keypair, &key1).unwrap();
    bank1.transfer(amount, &mint_keypair, &key0).unwrap();

    assert_eq!(bank0.hash_internal_state(), bank1.hash_internal_state());
}

#[test]
fn test_hash_internal_state_error() {
    agave_logger::setup();
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let amount = genesis_config.rent.minimum_balance(0);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let key0 = solana_pubkey::new_rand();
    bank.transfer(amount, &mint_keypair, &key0).unwrap();
    let orig = bank.hash_internal_state();

    // Transfer will error but still take a fee
    assert!(bank
        .transfer(LAMPORTS_PER_SOL, &mint_keypair, &key0)
        .is_err());
    assert_ne!(orig, bank.hash_internal_state());

    let orig = bank.hash_internal_state();
    let empty_keypair = Keypair::new();
    assert!(bank.transfer(amount, &empty_keypair, &key0).is_err());
    assert_eq!(orig, bank.hash_internal_state());
}

#[test]
fn test_bank_hash_internal_state_squash() {
    let leader_id = Pubkey::default();
    let bank0 = Arc::new(Bank::new_for_tests(&create_genesis_config(10).0));
    let hash0 = bank0.hash_internal_state();
    // save hash0 because new_from_parent
    // updates sysvar entries

    let bank1 = Bank::new_from_parent(bank0, &leader_id, 1);

    // no delta in bank1, hashes should always update
    assert_ne!(hash0, bank1.hash_internal_state());

    // remove parent
    bank1.squash();
    assert!(bank1.parents().is_empty());
}

/// Verifies that last ids and accounts are correctly referenced from parent
#[test]
fn test_bank_squash() {
    agave_logger::setup();
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee(2 * LAMPORTS_PER_SOL);
    let key1 = Keypair::new();
    let key2 = Keypair::new();
    let (parent, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let amount = genesis_config.rent.minimum_balance(0);

    let tx_transfer_mint_to_1 =
        system_transaction::transfer(&mint_keypair, &key1.pubkey(), amount, genesis_config.hash());
    trace!("parent process tx ");
    assert_eq!(parent.process_transaction(&tx_transfer_mint_to_1), Ok(()));
    trace!("done parent process tx ");
    assert_eq!(parent.transaction_count(), 1);
    assert_eq!(parent.non_vote_transaction_count_since_restart(), 1);
    assert_eq!(
        parent.get_signature_status(&tx_transfer_mint_to_1.signatures[0]),
        Some(Ok(()))
    );

    trace!("new from parent");
    let bank = new_from_parent_with_fork_next_slot(parent.clone(), bank_forks.as_ref());
    trace!("done new from parent");
    assert_eq!(
        bank.get_signature_status(&tx_transfer_mint_to_1.signatures[0]),
        Some(Ok(()))
    );

    assert_eq!(bank.transaction_count(), parent.transaction_count());
    assert_eq!(
        bank.non_vote_transaction_count_since_restart(),
        parent.non_vote_transaction_count_since_restart()
    );
    let tx_transfer_1_to_2 =
        system_transaction::transfer(&key1, &key2.pubkey(), amount, genesis_config.hash());
    assert_eq!(bank.process_transaction(&tx_transfer_1_to_2), Ok(()));
    assert_eq!(bank.transaction_count(), 2);
    assert_eq!(bank.non_vote_transaction_count_since_restart(), 2);
    assert_eq!(parent.transaction_count(), 1);
    assert_eq!(parent.non_vote_transaction_count_since_restart(), 1);
    assert_eq!(
        parent.get_signature_status(&tx_transfer_1_to_2.signatures[0]),
        None
    );

    for _ in 0..3 {
        // first time these should match what happened above, assert that parents are ok
        assert_eq!(bank.get_balance(&key1.pubkey()), 0);
        assert_eq!(bank.get_account(&key1.pubkey()), None);
        assert_eq!(bank.get_balance(&key2.pubkey()), amount);
        trace!("start");
        assert_eq!(
            bank.get_signature_status(&tx_transfer_mint_to_1.signatures[0]),
            Some(Ok(()))
        );
        assert_eq!(
            bank.get_signature_status(&tx_transfer_1_to_2.signatures[0]),
            Some(Ok(()))
        );

        // works iteration 0, no-ops on iteration 1 and 2
        trace!("SQUASH");
        bank.squash();

        assert_eq!(parent.transaction_count(), 1);
        assert_eq!(parent.non_vote_transaction_count_since_restart(), 1);
        assert_eq!(bank.transaction_count(), 2);
        assert_eq!(bank.non_vote_transaction_count_since_restart(), 2);
    }
}

#[test]
fn test_bank_get_account_in_parent_after_squash() {
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let (parent, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let amount = genesis_config.rent.minimum_balance(0);

    let key1 = Keypair::new();

    parent
        .transfer(amount, &mint_keypair, &key1.pubkey())
        .unwrap();
    assert_eq!(parent.get_balance(&key1.pubkey()), amount);
    let bank = new_from_parent(parent.clone());
    bank.squash();
    assert_eq!(parent.get_balance(&key1.pubkey()), amount);
}

#[test]
fn test_bank_get_account_in_parent_after_squash2() {
    agave_logger::setup();
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let amount = genesis_config.rent.minimum_balance(0);

    let key1 = Keypair::new();

    bank0
        .transfer(amount, &mint_keypair, &key1.pubkey())
        .unwrap();
    assert_eq!(bank0.get_balance(&key1.pubkey()), amount);

    let bank1 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank0.clone(),
        &Pubkey::default(),
        1,
    );
    bank1
        .transfer(3 * amount, &mint_keypair, &key1.pubkey())
        .unwrap();
    let bank2 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank0.clone(),
        &Pubkey::default(),
        2,
    );
    bank2
        .transfer(2 * amount, &mint_keypair, &key1.pubkey())
        .unwrap();

    let bank3 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank1.clone(),
        &Pubkey::default(),
        3,
    );
    bank1.squash();

    // This picks up the values from 1 which is the highest root:
    // TODO: if we need to access rooted banks older than this,
    // need to fix the lookup.
    assert_eq!(bank0.get_balance(&key1.pubkey()), 4 * amount);
    assert_eq!(bank3.get_balance(&key1.pubkey()), 4 * amount);
    assert_eq!(bank2.get_balance(&key1.pubkey()), 3 * amount);
    bank3.squash();
    assert_eq!(bank1.get_balance(&key1.pubkey()), 4 * amount);

    let bank4 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank3.clone(),
        &Pubkey::default(),
        4,
    );
    bank4
        .transfer(4 * amount, &mint_keypair, &key1.pubkey())
        .unwrap();
    assert_eq!(bank4.get_balance(&key1.pubkey()), 8 * amount);
    assert_eq!(bank3.get_balance(&key1.pubkey()), 4 * amount);
    bank4.squash();
    let bank5 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank4.clone(),
        &Pubkey::default(),
        5,
    );
    bank5.squash();
    let bank6 =
        Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank5, &Pubkey::default(), 6);
    bank6.squash();

    // This picks up the values from 4 which is the highest root:
    // TODO: if we need to access rooted banks older than this,
    // need to fix the lookup.
    assert_eq!(bank3.get_balance(&key1.pubkey()), 8 * amount);
    assert_eq!(bank2.get_balance(&key1.pubkey()), 8 * amount);

    assert_eq!(bank4.get_balance(&key1.pubkey()), 8 * amount);
}

#[test]
fn test_bank_get_account_modified_since_parent_with_fixed_root() {
    let pubkey = solana_pubkey::new_rand();

    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let amount = genesis_config.rent.minimum_balance(0);
    let (bank1, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    bank1.transfer(amount, &mint_keypair, &pubkey).unwrap();
    let result = bank1.get_account_modified_since_parent_with_fixed_root(&pubkey);
    assert!(result.is_some());
    let (account, slot) = result.unwrap();
    assert_eq!(account.lamports(), amount);
    assert_eq!(slot, 0);

    let bank2 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank1.clone(),
        &Pubkey::default(),
        1,
    );
    assert!(bank2
        .get_account_modified_since_parent_with_fixed_root(&pubkey)
        .is_none());
    bank2.transfer(2 * amount, &mint_keypair, &pubkey).unwrap();
    let result = bank1.get_account_modified_since_parent_with_fixed_root(&pubkey);
    assert!(result.is_some());
    let (account, slot) = result.unwrap();
    assert_eq!(account.lamports(), amount);
    assert_eq!(slot, 0);
    let result = bank2.get_account_modified_since_parent_with_fixed_root(&pubkey);
    assert!(result.is_some());
    let (account, slot) = result.unwrap();
    assert_eq!(account.lamports(), 3 * amount);
    assert_eq!(slot, 1);

    bank1.squash();

    let bank3 =
        Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank2, &Pubkey::default(), 3);
    assert_eq!(
        None,
        bank3.get_account_modified_since_parent_with_fixed_root(&pubkey)
    );
}

#[test]
fn test_bank_update_sysvar_account() {
    agave_logger::setup();
    // flushing the write cache is destructive, so test has to restart each time we flush and want to do 'illegal' operations once flushed
    for pass in 0..5 {
        use sysvar::clock::Clock;

        let dummy_clock_id = Pubkey::from_str_const("64jsX5hwtsjsKR7eNcNU4yhgwjuXoU9KR2MpnV47iXXz");
        let dummy_rent_epoch = 44;
        let (mut genesis_config, _mint_keypair) = create_genesis_config(500);

        let expected_previous_slot = 3;
        let mut expected_next_slot = expected_previous_slot + 1;

        // First, initialize the clock sysvar
        for feature_id in FeatureSet::default().inactive() {
            activate_feature(&mut genesis_config, *feature_id);
        }
        let bank1 = Arc::new(Bank::new_for_tests(&genesis_config));
        if pass == 0 {
            add_root_and_flush_write_cache(&bank1);
            assert_eq!(
                bank1.calculate_capitalization_for_tests(),
                bank1.capitalization()
            );
            continue;
        }

        assert_capitalization_diff(
            &bank1,
            || {
                bank1.update_sysvar_account(&dummy_clock_id, |optional_account| {
                    assert!(optional_account.is_none());

                    let mut account = create_account(
                        &Clock {
                            slot: expected_previous_slot,
                            ..Clock::default()
                        },
                        bank1.inherit_specially_retained_account_fields(optional_account),
                    );
                    account.set_rent_epoch(dummy_rent_epoch);
                    account
                });
                let current_account = bank1.get_account(&dummy_clock_id).unwrap();
                assert_eq!(
                    expected_previous_slot,
                    from_account::<Clock, _>(&current_account).unwrap().slot
                );
                assert_eq!(dummy_rent_epoch, current_account.rent_epoch());
            },
            |old, new| {
                assert_eq!(
                    old + min_rent_exempt_balance_for_sysvars(&bank1, &[sysvar::clock::id()]),
                    new
                );
                pass == 1
            },
        );
        if pass == 1 {
            continue;
        }

        assert_capitalization_diff(
            &bank1,
            || {
                bank1.update_sysvar_account(&dummy_clock_id, |optional_account| {
                    assert!(optional_account.is_some());

                    create_account(
                        &Clock {
                            slot: expected_previous_slot,
                            ..Clock::default()
                        },
                        bank1.inherit_specially_retained_account_fields(optional_account),
                    )
                })
            },
            |old, new| {
                // creating new sysvar twice in a slot shouldn't increment capitalization twice
                assert_eq!(old, new);
                pass == 2
            },
        );
        if pass == 2 {
            continue;
        }

        // Updating should increment the clock's slot
        let bank2 = Arc::new(Bank::new_from_parent(bank1.clone(), &Pubkey::default(), 1));
        add_root_and_flush_write_cache(&bank1);
        assert_capitalization_diff(
            &bank2,
            || {
                bank2.update_sysvar_account(&dummy_clock_id, |optional_account| {
                    let slot = from_account::<Clock, _>(optional_account.as_ref().unwrap())
                        .unwrap()
                        .slot
                        + 1;

                    create_account(
                        &Clock {
                            slot,
                            ..Clock::default()
                        },
                        bank2.inherit_specially_retained_account_fields(optional_account),
                    )
                });
                let current_account = bank2.get_account(&dummy_clock_id).unwrap();
                assert_eq!(
                    expected_next_slot,
                    from_account::<Clock, _>(&current_account).unwrap().slot
                );
                assert_eq!(dummy_rent_epoch, current_account.rent_epoch());
            },
            |old, new| {
                // if existing, capitalization shouldn't change
                assert_eq!(old, new);
                pass == 3
            },
        );
        if pass == 3 {
            continue;
        }

        // Updating again should give bank2's sysvar to the closure not bank1's.
        // Thus, increment expected_next_slot accordingly
        expected_next_slot += 1;
        assert_capitalization_diff(
            &bank2,
            || {
                bank2.update_sysvar_account(&dummy_clock_id, |optional_account| {
                    let slot = from_account::<Clock, _>(optional_account.as_ref().unwrap())
                        .unwrap()
                        .slot
                        + 1;

                    create_account(
                        &Clock {
                            slot,
                            ..Clock::default()
                        },
                        bank2.inherit_specially_retained_account_fields(optional_account),
                    )
                });
                let current_account = bank2.get_account(&dummy_clock_id).unwrap();
                assert_eq!(
                    expected_next_slot,
                    from_account::<Clock, _>(&current_account).unwrap().slot
                );
            },
            |old, new| {
                // updating twice in a slot shouldn't increment capitalization twice
                assert_eq!(old, new);
                true
            },
        );
    }
}

#[test]
fn test_bank_epoch_vote_accounts() {
    let leader_pubkey = solana_pubkey::new_rand();
    let leader_lamports = 3;
    let mut genesis_config =
        create_genesis_config_with_leader(5, &leader_pubkey, leader_lamports).genesis_config;

    // set this up weird, forces future generation, odd mod(), etc.
    //  this says: "vote_accounts for epoch X should be generated at slot index 3 in epoch X-2...
    const SLOTS_PER_EPOCH: u64 = MINIMUM_SLOTS_PER_EPOCH;
    const LEADER_SCHEDULE_SLOT_OFFSET: u64 = SLOTS_PER_EPOCH * 3 - 3;
    // no warmup allows me to do the normal division stuff below
    genesis_config.epoch_schedule =
        EpochSchedule::custom(SLOTS_PER_EPOCH, LEADER_SCHEDULE_SLOT_OFFSET, false);

    let parent = Arc::new(Bank::new_for_tests(&genesis_config));
    let mut leader_vote_stake: Vec<_> = parent
        .epoch_vote_accounts(0)
        .map(|accounts| {
            accounts
                .iter()
                .filter_map(|(pubkey, (stake, account))| {
                    if account.node_pubkey() == &leader_pubkey {
                        Some((*pubkey, *stake))
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap();
    assert_eq!(leader_vote_stake.len(), 1);
    let (leader_vote_account, leader_stake) = leader_vote_stake.pop().unwrap();
    assert!(leader_stake > 0);

    let leader_stake = Stake {
        delegation: Delegation {
            stake: leader_lamports,
            activation_epoch: u64::MAX, // bootstrap
            ..Delegation::default()
        },
        ..Stake::default()
    };

    let mut epoch = 1;
    loop {
        if epoch > LEADER_SCHEDULE_SLOT_OFFSET / SLOTS_PER_EPOCH {
            break;
        }
        let vote_accounts = parent.epoch_vote_accounts(epoch);
        assert!(vote_accounts.is_some());

        // epoch_stakes are a snapshot at the leader_schedule_slot_offset boundary
        //   in the prior epoch (0 in this case)
        assert_eq!(
            leader_stake.stake(0, &StakeHistory::default(), None),
            vote_accounts.unwrap().get(&leader_vote_account).unwrap().0
        );

        epoch += 1;
    }

    // child crosses epoch boundary and is the first slot in the epoch
    let child = Bank::new_from_parent(
        parent.clone(),
        &leader_pubkey,
        SLOTS_PER_EPOCH - (LEADER_SCHEDULE_SLOT_OFFSET % SLOTS_PER_EPOCH),
    );

    assert!(child.epoch_vote_accounts(epoch).is_some());
    assert_eq!(
        leader_stake.stake(child.epoch(), &StakeHistory::default(), None),
        child
            .epoch_vote_accounts(epoch)
            .unwrap()
            .get(&leader_vote_account)
            .unwrap()
            .0
    );

    // child crosses epoch boundary but isn't the first slot in the epoch, still
    //  makes an epoch stakes snapshot at 1
    let child = Bank::new_from_parent(
        parent,
        &leader_pubkey,
        SLOTS_PER_EPOCH - (LEADER_SCHEDULE_SLOT_OFFSET % SLOTS_PER_EPOCH) + 1,
    );
    assert!(child.epoch_vote_accounts(epoch).is_some());
    assert_eq!(
        leader_stake.stake(child.epoch(), &StakeHistory::default(), None),
        child
            .epoch_vote_accounts(epoch)
            .unwrap()
            .get(&leader_vote_account)
            .unwrap()
            .0
    );
}

#[test]
fn test_zero_signatures() {
    agave_logger::setup();
    let (genesis_config, mint_keypair) = create_genesis_config(500);
    let mut bank = Bank::new_for_tests(&genesis_config);
    bank.fee_rate_governor.lamports_per_signature = 2;
    let key = solana_pubkey::new_rand();

    let mut transfer_instruction = system_instruction::transfer(&mint_keypair.pubkey(), &key, 0);
    transfer_instruction.accounts[0].is_signer = false;
    let message = Message::new(&[transfer_instruction], None);
    let tx = Transaction::new(&Vec::<&Keypair>::new(), message, bank.last_blockhash());

    assert_eq!(
        bank.process_transaction(&tx),
        Err(TransactionError::SanitizeFailure)
    );
    assert_eq!(bank.get_balance(&key), 0);
}

#[test]
fn test_bank_get_slots_in_epoch() {
    let (genesis_config, _) = create_genesis_config(500);

    let bank = Bank::new_for_tests(&genesis_config);

    assert_eq!(bank.get_slots_in_epoch(0), MINIMUM_SLOTS_PER_EPOCH);
    assert_eq!(bank.get_slots_in_epoch(2), (MINIMUM_SLOTS_PER_EPOCH * 4));
    assert_eq!(
        bank.get_slots_in_epoch(5000),
        genesis_config.epoch_schedule.slots_per_epoch
    );
}

#[test]
fn test_is_delta_true() {
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let key1 = Keypair::new();
    let tx_transfer_mint_to_1 = system_transaction::transfer(
        &mint_keypair,
        &key1.pubkey(),
        genesis_config.rent.minimum_balance(0),
        genesis_config.hash(),
    );
    assert_eq!(bank.process_transaction(&tx_transfer_mint_to_1), Ok(()));
    assert!(bank.is_delta.load(Relaxed));

    let bank1 = new_from_parent(bank.clone());
    let hash1 = bank1.hash_internal_state();
    assert!(!bank1.is_delta.load(Relaxed));
    assert_ne!(hash1, bank.hash());
    // ticks don't make a bank into a delta or change its state unless a block boundary is crossed
    bank1.register_default_tick_for_test();
    assert!(!bank1.is_delta.load(Relaxed));
    assert_eq!(bank1.hash_internal_state(), hash1);
}

#[test]
fn test_is_empty() {
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let (bank0, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let key1 = Keypair::new();

    // The zeroth bank is empty because there are no transactions
    assert!(bank0.is_empty());

    // Set is_delta to true, bank is no longer empty
    let tx_transfer_mint_to_1 = system_transaction::transfer(
        &mint_keypair,
        &key1.pubkey(),
        genesis_config.rent.minimum_balance(0),
        genesis_config.hash(),
    );
    assert_eq!(bank0.process_transaction(&tx_transfer_mint_to_1), Ok(()));
    assert!(!bank0.is_empty());
}

#[test]
fn test_bank_inherit_tx_count() {
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    // Bank 1
    let bank1 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank0.clone(),
        &solana_pubkey::new_rand(),
        1,
    );
    // Bank 2
    let bank2 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank0.clone(),
        &solana_pubkey::new_rand(),
        2,
    );

    // transfer a token
    assert_eq!(
        bank1.process_transaction(&system_transaction::transfer(
            &mint_keypair,
            &Keypair::new().pubkey(),
            genesis_config.rent.minimum_balance(0),
            genesis_config.hash(),
        )),
        Ok(())
    );

    assert_eq!(bank0.transaction_count(), 0);
    assert_eq!(bank0.non_vote_transaction_count_since_restart(), 0);
    assert_eq!(bank2.transaction_count(), 0);
    assert_eq!(bank2.non_vote_transaction_count_since_restart(), 0);
    assert_eq!(bank1.transaction_count(), 1);
    assert_eq!(bank1.non_vote_transaction_count_since_restart(), 1);

    bank1.squash();

    assert_eq!(bank0.transaction_count(), 0);
    assert_eq!(bank0.non_vote_transaction_count_since_restart(), 0);
    assert_eq!(bank2.transaction_count(), 0);
    assert_eq!(bank2.non_vote_transaction_count_since_restart(), 0);
    assert_eq!(bank1.transaction_count(), 1);
    assert_eq!(bank1.non_vote_transaction_count_since_restart(), 1);

    let bank6 = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank1.clone(),
        &solana_pubkey::new_rand(),
        3,
    );
    assert_eq!(bank1.transaction_count(), 1);
    assert_eq!(bank1.non_vote_transaction_count_since_restart(), 1);
    assert_eq!(bank6.transaction_count(), 1);
    assert_eq!(bank6.non_vote_transaction_count_since_restart(), 1);

    bank6.squash();
    assert_eq!(bank6.transaction_count(), 1);
    assert_eq!(bank6.non_vote_transaction_count_since_restart(), 1);
}

#[test]
fn test_bank_inherit_fee_rate_governor() {
    let (mut genesis_config, _mint_keypair) = create_genesis_config(500);
    genesis_config
        .fee_rate_governor
        .target_lamports_per_signature = 123;

    let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
    let bank1 = Arc::new(new_from_parent(bank0.clone()));
    assert_eq!(
        bank0.fee_rate_governor.target_lamports_per_signature / 2,
        bank1
            .fee_rate_governor
            .create_fee_calculator()
            .lamports_per_signature
    );
}

#[test]
fn test_bank_vote_accounts() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(500, &solana_pubkey::new_rand(), 1);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    let vote_accounts = bank.vote_accounts();
    assert_eq!(vote_accounts.len(), 1); // bootstrap validator has
                                        // to have a vote account

    let vote_keypair = Keypair::new();
    let instructions = vote_instruction::create_account_with_config(
        &mint_keypair.pubkey(),
        &vote_keypair.pubkey(),
        &VoteInit {
            node_pubkey: mint_keypair.pubkey(),
            authorized_voter: vote_keypair.pubkey(),
            authorized_withdrawer: vote_keypair.pubkey(),
            commission: 0,
        },
        10,
        vote_instruction::CreateVoteAccountConfig {
            space: VoteStateV4::size_of() as u64,
            ..vote_instruction::CreateVoteAccountConfig::default()
        },
    );

    let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
    let transaction = Transaction::new(
        &[&mint_keypair, &vote_keypair],
        message,
        bank.last_blockhash(),
    );

    bank.process_transaction(&transaction).unwrap();

    let vote_accounts = bank.vote_accounts();

    assert_eq!(vote_accounts.len(), 2);

    assert!(vote_accounts.get(&vote_keypair.pubkey()).is_some());

    assert!(bank.withdraw(&vote_keypair.pubkey(), 10).is_ok());

    let vote_accounts = bank.vote_accounts();

    assert_eq!(vote_accounts.len(), 1);
}

#[test]
fn test_bank_cloned_stake_delegations() {
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(
        123_456_000_000_000,
        &solana_pubkey::new_rand(),
        123_000_000_000,
    );
    genesis_config.rent = Rent::default();

    for (pubkey, account) in
        solana_program_binaries::by_id(&stake_program::id(), &genesis_config.rent)
            .unwrap()
            .into_iter()
    {
        genesis_config.add_account(pubkey, account);
    }

    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    bank.squash();
    let bank = Bank::new_from_parent(bank, &Pubkey::new_unique(), 1);

    let stake_delegations = bank.stakes_cache.stakes().stake_delegations().clone();
    assert_eq!(stake_delegations.len(), 1); // bootstrap validator has
                                            // to have a stake delegation

    let (vote_balance, stake_balance) = {
        let rent = &bank.rent_collector().rent;
        let vote_rent_exempt_reserve = rent.minimum_balance(VoteStateV4::size_of());
        let stake_rent_exempt_reserve = rent.minimum_balance(StakeStateV2::size_of());
        let minimum_delegation = stake_utils::get_minimum_delegation(
            bank.feature_set
                .is_active(&agave_feature_set::stake_raise_minimum_delegation_to_1_sol::id()),
        );
        (
            vote_rent_exempt_reserve,
            stake_rent_exempt_reserve + minimum_delegation,
        )
    };

    let vote_keypair = Keypair::new();
    let mut instructions = vote_instruction::create_account_with_config(
        &mint_keypair.pubkey(),
        &vote_keypair.pubkey(),
        &VoteInit {
            node_pubkey: mint_keypair.pubkey(),
            authorized_voter: vote_keypair.pubkey(),
            authorized_withdrawer: vote_keypair.pubkey(),
            commission: 0,
        },
        vote_balance,
        vote_instruction::CreateVoteAccountConfig {
            space: VoteStateV4::size_of() as u64,
            ..vote_instruction::CreateVoteAccountConfig::default()
        },
    );

    let stake_keypair = Keypair::new();
    instructions.extend(stake_instruction::create_account_and_delegate_stake(
        &mint_keypair.pubkey(),
        &stake_keypair.pubkey(),
        &vote_keypair.pubkey(),
        &Authorized::auto(&stake_keypair.pubkey()),
        &Lockup::default(),
        stake_balance,
    ));

    let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
    let transaction = Transaction::new(
        &[&mint_keypair, &vote_keypair, &stake_keypair],
        message,
        bank.last_blockhash(),
    );

    bank.process_transaction(&transaction).unwrap();

    let stake_delegations = bank.stakes_cache.stakes().stake_delegations().clone();
    assert_eq!(stake_delegations.len(), 2);
    assert!(stake_delegations.get(&stake_keypair.pubkey()).is_some());
}

#[test]
fn test_is_delta_with_no_committables() {
    let (genesis_config, mint_keypair) = create_genesis_config(8000);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    bank.is_delta.store(false, Relaxed);

    let keypair1 = Keypair::new();
    let keypair2 = Keypair::new();
    let fail_tx =
        system_transaction::transfer(&keypair1, &keypair2.pubkey(), 1, bank.last_blockhash());

    // Should fail with TransactionError::AccountNotFound, which means
    // the account which this tx operated on will not be committed. Thus
    // the bank is_delta should still be false
    assert_eq!(
        bank.process_transaction(&fail_tx),
        Err(TransactionError::AccountNotFound)
    );

    // Check the bank is_delta is still false
    assert!(!bank.is_delta.load(Relaxed));

    // Should fail with InstructionError, but InstructionErrors are committable,
    // so is_delta should be true
    assert_eq!(
        bank.transfer(10_001, &mint_keypair, &solana_pubkey::new_rand()),
        Err(TransactionError::InstructionError(
            0,
            SystemError::ResultWithNegativeLamports.into(),
        ))
    );

    assert!(bank.is_delta.load(Relaxed));
}

#[test]
fn test_bank_get_program_accounts() {
    let (genesis_config, mint_keypair) = create_genesis_config(500);
    let parent = Arc::new(Bank::new_for_tests(&genesis_config));

    let genesis_accounts: Vec<_> = parent.get_all_accounts(false).unwrap();
    assert!(
        genesis_accounts
            .iter()
            .any(|(pubkey, _, _)| *pubkey == mint_keypair.pubkey()),
        "mint pubkey not found"
    );
    assert!(
        genesis_accounts
            .iter()
            .any(|(_, account, _)| solana_sdk_ids::sysvar::check_id(account.owner())),
        "no sysvars found"
    );

    let bank0 = Arc::new(new_from_parent(parent));
    let pubkey0 = solana_pubkey::new_rand();
    let program_id = Pubkey::from([2; 32]);
    let account0 = AccountSharedData::new(1, 0, &program_id);
    bank0.store_account(&pubkey0, &account0);

    assert_eq!(
        bank0.get_program_accounts_modified_since_parent(&program_id),
        vec![(pubkey0, account0.clone())]
    );

    let bank1 = Arc::new(new_from_parent(bank0.clone()));
    bank1.squash();
    assert_eq!(
        bank0
            .get_program_accounts(&program_id, &ScanConfig::default(),)
            .unwrap(),
        vec![(pubkey0, account0.clone())]
    );
    assert_eq!(
        bank1
            .get_program_accounts(&program_id, &ScanConfig::default(),)
            .unwrap(),
        vec![(pubkey0, account0)]
    );
    assert_eq!(
        bank1.get_program_accounts_modified_since_parent(&program_id),
        vec![]
    );

    let bank2 = Arc::new(new_from_parent(bank1.clone()));
    let pubkey1 = solana_pubkey::new_rand();
    let account1 = AccountSharedData::new(3, 0, &program_id);
    bank2.store_account(&pubkey1, &account1);
    // Accounts with 0 lamports should be filtered out by Accounts::load_by_program()
    let pubkey2 = solana_pubkey::new_rand();
    let account2 = AccountSharedData::new(0, 0, &program_id);
    bank2.store_account(&pubkey2, &account2);

    let bank3 = Arc::new(new_from_parent(bank2));
    bank3.squash();
    assert_eq!(
        bank1
            .get_program_accounts(&program_id, &ScanConfig::default(),)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        bank3
            .get_program_accounts(&program_id, &ScanConfig::default(),)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn test_get_filtered_indexed_accounts_limit_exceeded() {
    let (genesis_config, _mint_keypair) = create_genesis_config(500);
    let mut account_indexes = AccountSecondaryIndexes::default();
    account_indexes.indexes.insert(AccountIndex::ProgramId);
    let bank_config = BankTestConfig {
        accounts_db_config: AccountsDbConfig {
            account_indexes: Some(account_indexes),
            ..ACCOUNTS_DB_CONFIG_FOR_TESTING
        },
    };
    let bank = Arc::new(Bank::new_with_config_for_tests(
        &genesis_config,
        bank_config,
    ));

    let address = Pubkey::new_unique();
    let program_id = Pubkey::new_unique();
    let limit = 100;
    let account = AccountSharedData::new(1, limit, &program_id);
    bank.store_account(&address, &account);

    assert!(bank
        .get_filtered_indexed_accounts(
            &IndexKey::ProgramId(program_id),
            |_| true,
            &ScanConfig::default(),
            Some(limit), // limit here will be exceeded, resulting in aborted scan
        )
        .is_err());
}

#[test]
fn test_get_filtered_indexed_accounts() {
    let (genesis_config, _mint_keypair) = create_genesis_config(500);
    let mut account_indexes = AccountSecondaryIndexes::default();
    account_indexes.indexes.insert(AccountIndex::ProgramId);
    let bank_config = BankTestConfig {
        accounts_db_config: AccountsDbConfig {
            account_indexes: Some(account_indexes),
            ..ACCOUNTS_DB_CONFIG_FOR_TESTING
        },
    };
    let bank = Arc::new(Bank::new_with_config_for_tests(
        &genesis_config,
        bank_config,
    ));

    let address = Pubkey::new_unique();
    let program_id = Pubkey::new_unique();
    let account = AccountSharedData::new(1, 0, &program_id);
    bank.store_account(&address, &account);

    let indexed_accounts = bank
        .get_filtered_indexed_accounts(
            &IndexKey::ProgramId(program_id),
            |_| true,
            &ScanConfig::default(),
            None,
        )
        .unwrap();
    assert_eq!(indexed_accounts.len(), 1);
    assert_eq!(indexed_accounts[0], (address, account));

    // Even though the account is re-stored in the bank (and the index) under a new program id,
    // it is still present in the index under the original program id as well. This
    // demonstrates the need for a redundant post-processing filter.
    let another_program_id = Pubkey::new_unique();
    let new_account = AccountSharedData::new(1, 0, &another_program_id);
    let bank = Arc::new(new_from_parent(bank));
    bank.store_account(&address, &new_account);
    let indexed_accounts = bank
        .get_filtered_indexed_accounts(
            &IndexKey::ProgramId(program_id),
            |_| true,
            &ScanConfig::default(),
            None,
        )
        .unwrap();
    assert_eq!(indexed_accounts.len(), 1);
    assert_eq!(indexed_accounts[0], (address, new_account.clone()));
    let indexed_accounts = bank
        .get_filtered_indexed_accounts(
            &IndexKey::ProgramId(another_program_id),
            |_| true,
            &ScanConfig::default(),
            None,
        )
        .unwrap();
    assert_eq!(indexed_accounts.len(), 1);
    assert_eq!(indexed_accounts[0], (address, new_account.clone()));

    // Post-processing filter
    let indexed_accounts = bank
        .get_filtered_indexed_accounts(
            &IndexKey::ProgramId(program_id),
            |account| account.owner() == &program_id,
            &ScanConfig::default(),
            None,
        )
        .unwrap();
    assert!(indexed_accounts.is_empty());
    let indexed_accounts = bank
        .get_filtered_indexed_accounts(
            &IndexKey::ProgramId(another_program_id),
            |account| account.owner() == &another_program_id,
            &ScanConfig::default(),
            None,
        )
        .unwrap();
    assert_eq!(indexed_accounts.len(), 1);
    assert_eq!(indexed_accounts[0], (address, new_account));
}

#[test]
fn test_status_cache_ancestors() {
    agave_logger::setup();
    let (parent, _bank_forks) = create_simple_test_arc_bank(500);
    let bank1 = Arc::new(new_from_parent(parent));
    let mut bank = bank1;
    for _ in 0..MAX_CACHE_ENTRIES * 2 {
        bank = Arc::new(new_from_parent(bank));
        bank.squash();
    }

    let bank = new_from_parent(bank);
    assert_eq!(
        bank.status_cache_ancestors(),
        (bank.slot() - MAX_CACHE_ENTRIES as u64..=bank.slot()).collect::<Vec<_>>()
    );
}

#[test]
fn test_add_builtin() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee_no_rent(500);
    let mut bank = Bank::new_for_tests(&genesis_config);

    fn mock_vote_program_id() -> Pubkey {
        Pubkey::from([42u8; 32])
    }
    declare_process_instruction!(MockBuiltin, 1, |invoke_context| {
        let transaction_context = &invoke_context.transaction_context;
        let instruction_context = transaction_context.get_current_instruction_context()?;
        let program_id = instruction_context.get_program_key()?;
        if mock_vote_program_id() != *program_id {
            return Err(InstructionError::IncorrectProgramId);
        }
        Err(InstructionError::Custom(42))
    });

    assert!(bank.get_account(&mock_vote_program_id()).is_none());
    bank.add_mockup_builtin(mock_vote_program_id(), MockBuiltin::vm);
    assert!(bank.get_account(&mock_vote_program_id()).is_some());

    let mock_account = Keypair::new();
    let mock_validator_identity = Keypair::new();
    let mut instructions = vote_instruction::create_account_with_config(
        &mint_keypair.pubkey(),
        &mock_account.pubkey(),
        &VoteInit {
            node_pubkey: mock_validator_identity.pubkey(),
            ..VoteInit::default()
        },
        1,
        vote_instruction::CreateVoteAccountConfig {
            space: VoteStateV4::size_of() as u64,
            ..vote_instruction::CreateVoteAccountConfig::default()
        },
    );
    instructions[1].program_id = mock_vote_program_id();

    let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
    let transaction = Transaction::new(
        &[&mint_keypair, &mock_account, &mock_validator_identity],
        message,
        bank.last_blockhash(),
    );

    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
    assert_eq!(
        bank.process_transaction(&transaction),
        Err(TransactionError::InstructionError(
            1,
            InstructionError::Custom(42)
        ))
    );
}

#[test]
fn test_add_duplicate_static_program() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(500, &solana_pubkey::new_rand(), 0);
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    declare_process_instruction!(MockBuiltin, 1, |_invoke_context| {
        Err(InstructionError::Custom(42))
    });

    let mock_account = Keypair::new();
    let mock_validator_identity = Keypair::new();
    let instructions = vote_instruction::create_account_with_config(
        &mint_keypair.pubkey(),
        &mock_account.pubkey(),
        &VoteInit {
            node_pubkey: mock_validator_identity.pubkey(),
            ..VoteInit::default()
        },
        1,
        vote_instruction::CreateVoteAccountConfig {
            space: VoteStateV4::size_of() as u64,
            ..vote_instruction::CreateVoteAccountConfig::default()
        },
    );

    let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
    let transaction = Transaction::new(
        &[&mint_keypair, &mock_account, &mock_validator_identity],
        message,
        bank.last_blockhash(),
    );

    let slot = bank.slot().saturating_add(1);
    let mut bank = Bank::new_from_parent(bank, &Pubkey::default(), slot);
    bank.add_mockup_builtin(solana_vote_program::id(), MockBuiltin::vm);
    let bank = bank_forks
        .write()
        .unwrap()
        .insert(bank)
        .clone_without_scheduler();

    let vote_loader_account = bank.get_account(&solana_vote_program::id()).unwrap();
    let new_vote_loader_account = bank.get_account(&solana_vote_program::id()).unwrap();
    // Vote loader account should not be updated since it was included in the genesis config.
    assert_eq!(vote_loader_account.data(), new_vote_loader_account.data());
    assert_eq!(
        bank.process_transaction(&transaction),
        Err(TransactionError::InstructionError(
            1,
            InstructionError::Custom(42)
        ))
    );
}

#[test]
fn test_add_instruction_processor_for_existing_unrelated_accounts() {
    for pass in 0..5 {
        let mut bank = create_simple_test_bank(500);

        declare_process_instruction!(MockBuiltin, 1, |_invoke_context| {
            Err(InstructionError::Custom(42))
        });

        // Non-builtin loader accounts can not be used for instruction processing
        {
            let stakes = bank.stakes_cache.stakes();
            assert!(stakes.vote_accounts().as_ref().is_empty());
        }
        assert!(bank.stakes_cache.stakes().stake_delegations().is_empty());
        if pass == 0 {
            add_root_and_flush_write_cache(&bank);
            assert_eq!(
                bank.calculate_capitalization_for_tests(),
                bank.capitalization()
            );
            continue;
        }

        let ((vote_id, vote_account), (stake_id, stake_account)) =
            crate::stakes::tests::create_staked_node_accounts(1_0000);
        bank.capitalization
            .fetch_add(vote_account.lamports() + stake_account.lamports(), Relaxed);
        bank.store_account(&vote_id, &vote_account);
        bank.store_account(&stake_id, &stake_account);
        {
            let stakes = bank.stakes_cache.stakes();
            assert!(!stakes.vote_accounts().as_ref().is_empty());
        }
        assert!(!bank.stakes_cache.stakes().stake_delegations().is_empty());
        if pass == 1 {
            add_root_and_flush_write_cache(&bank);
            assert_eq!(
                bank.calculate_capitalization_for_tests(),
                bank.capitalization()
            );
            continue;
        }

        bank.add_builtin(
            vote_id,
            "mock_program1",
            ProgramCacheEntry::new_builtin(0, 0, MockBuiltin::vm),
        );
        bank.add_builtin(
            stake_id,
            "mock_program2",
            ProgramCacheEntry::new_builtin(0, 0, MockBuiltin::vm),
        );
        {
            let stakes = bank.stakes_cache.stakes();
            assert!(stakes.vote_accounts().as_ref().is_empty());
        }
        assert!(bank.stakes_cache.stakes().stake_delegations().is_empty());
        if pass == 2 {
            add_root_and_flush_write_cache(&bank);
            assert_eq!(
                bank.calculate_capitalization_for_tests(),
                bank.capitalization()
            );
            continue;
        }
        assert_eq!(
            "mock_program1",
            String::from_utf8_lossy(bank.get_account(&vote_id).unwrap_or_default().data())
        );
        assert_eq!(
            "mock_program2",
            String::from_utf8_lossy(bank.get_account(&stake_id).unwrap_or_default().data())
        );

        // Re-adding builtin programs should be no-op
        let old_hash = bank.calculate_accounts_lt_hash_for_tests();
        bank.add_mockup_builtin(vote_id, MockBuiltin::vm);
        bank.add_mockup_builtin(stake_id, MockBuiltin::vm);
        add_root_and_flush_write_cache(&bank);
        let new_hash = bank.calculate_accounts_lt_hash_for_tests();
        assert_eq!(old_hash, new_hash);
        {
            let stakes = bank.stakes_cache.stakes();
            assert!(stakes.vote_accounts().as_ref().is_empty());
        }
        assert!(bank.stakes_cache.stakes().stake_delegations().is_empty());
        assert_eq!(
            bank.calculate_capitalization_for_tests(),
            bank.capitalization()
        );
        assert_eq!(
            "mock_program1",
            String::from_utf8_lossy(bank.get_account(&vote_id).unwrap_or_default().data())
        );
        assert_eq!(
            "mock_program2",
            String::from_utf8_lossy(bank.get_account(&stake_id).unwrap_or_default().data())
        );
    }
}

#[allow(deprecated)]
#[test]
fn test_recent_blockhashes_sysvar() {
    let (mut bank, _bank_forks) = create_simple_test_arc_bank(500);
    for i in 1..5 {
        let bhq_account = bank.get_account(&sysvar::recent_blockhashes::id()).unwrap();
        let recent_blockhashes =
            from_account::<sysvar::recent_blockhashes::RecentBlockhashes, _>(&bhq_account).unwrap();
        // Check length
        assert_eq!(recent_blockhashes.len(), i);
        let most_recent_hash = recent_blockhashes.iter().next().unwrap().blockhash;
        // Check order
        assert!(bank.is_hash_valid_for_age(&most_recent_hash, 0));
        goto_end_of_slot(bank.clone());
        bank = Arc::new(new_from_parent(bank));
    }
}

#[allow(deprecated)]
#[test]
fn test_blockhash_queue_sysvar_consistency() {
    let (bank, _bank_forks) = create_simple_test_arc_bank(100_000);
    goto_end_of_slot(bank.clone());

    let bhq_account = bank.get_account(&sysvar::recent_blockhashes::id()).unwrap();
    let recent_blockhashes =
        from_account::<sysvar::recent_blockhashes::RecentBlockhashes, _>(&bhq_account).unwrap();

    let sysvar_recent_blockhash = recent_blockhashes[0].blockhash;
    let bank_last_blockhash = bank.last_blockhash();
    assert_eq!(sysvar_recent_blockhash, bank_last_blockhash);
}

#[test]
fn test_hash_internal_state_unchanged() {
    let (genesis_config, _) = create_genesis_config(500);
    let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
    bank0.freeze();
    let bank0_hash = bank0.hash();
    let bank1 = Bank::new_from_parent(bank0, &Pubkey::default(), 1);
    bank1.freeze();
    let bank1_hash = bank1.hash();
    // Checkpointing should always result in a new state
    assert_ne!(bank0_hash, bank1_hash);
}

#[test]
fn test_hash_internal_state_unchanged_with_ticks() {
    let (genesis_config, _) = create_genesis_config(500);
    let bank = Arc::new(Bank::new_for_tests(&genesis_config));
    let bank1 = new_from_parent(bank);
    let hash1 = bank1.hash_internal_state();
    // ticks don't change its state even if a slot boundary is crossed
    // because blockhashes are only recorded at block boundaries
    for _ in 0..genesis_config.ticks_per_slot {
        assert_eq!(bank1.hash_internal_state(), hash1);
        bank1.register_default_tick_for_test();
    }
    assert_eq!(bank1.hash_internal_state(), hash1);
}

#[ignore]
#[test]
fn test_banks_leak() {
    fn add_lotsa_stake_accounts(genesis_config: &mut GenesisConfig) {
        const LOTSA: usize = 4_096;

        (0..LOTSA).for_each(|_| {
            let pubkey = solana_pubkey::new_rand();
            genesis_config.add_account(
                pubkey,
                create_lockup_stake_account(
                    &Authorized::auto(&pubkey),
                    &Lockup::default(),
                    &Rent::default(),
                    50_000_000,
                ),
            );
        });
    }
    agave_logger::setup();
    let (mut genesis_config, _) = create_genesis_config(100_000_000_000_000);
    add_lotsa_stake_accounts(&mut genesis_config);
    let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let mut num_banks = 0;
    let pid = std::process::id();
    #[cfg(not(target_os = "linux"))]
    error!(
        "\nYou can run this to watch RAM:\n   while read -p 'banks: '; do echo $(( $(ps -o vsize= \
         -p {pid})/$REPLY));done"
    );
    loop {
        num_banks += 1;
        bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
        if num_banks % 100 == 0 {
            #[cfg(target_os = "linux")]
            {
                let pages_consumed = std::fs::read_to_string(format!("/proc/{pid}/statm"))
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap();
                error!(
                    "at {} banks: {} mem or {}kB/bank",
                    num_banks,
                    pages_consumed * 4096,
                    (pages_consumed * 4) / num_banks
                );
            }
            #[cfg(not(target_os = "linux"))]
            {
                error!("{num_banks} banks, sleeping for 5 sec");
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

pub(in crate::bank) fn get_nonce_blockhash(bank: &Bank, nonce_pubkey: &Pubkey) -> Option<Hash> {
    let account = bank.get_account(nonce_pubkey)?;
    let nonce_data = get_nonce_data_from_account(&account)?;
    Some(nonce_data.blockhash())
}

pub(in crate::bank) fn get_nonce_data_from_account(
    account: &AccountSharedData,
) -> Option<nonce::state::Data> {
    let nonce_versions = StateMut::<nonce::versions::Versions>::state(account).ok()?;
    if let nonce::state::State::Initialized(nonce_data) = nonce_versions.state() {
        Some(nonce_data.clone())
    } else {
        None
    }
}

fn nonce_setup(
    bank: &Arc<Bank>,
    mint_keypair: &Keypair,
    custodian_lamports: u64,
    nonce_lamports: u64,
    nonce_authority: Option<Pubkey>,
) -> Result<(Keypair, Keypair)> {
    let custodian_keypair = Keypair::new();
    let nonce_keypair = Keypair::new();
    /* Setup accounts */
    let mut setup_ixs = vec![system_instruction::transfer(
        &mint_keypair.pubkey(),
        &custodian_keypair.pubkey(),
        custodian_lamports,
    )];
    let nonce_authority = nonce_authority.unwrap_or_else(|| nonce_keypair.pubkey());
    setup_ixs.extend_from_slice(&system_instruction::create_nonce_account(
        &custodian_keypair.pubkey(),
        &nonce_keypair.pubkey(),
        &nonce_authority,
        nonce_lamports,
    ));
    let message = Message::new(&setup_ixs, Some(&mint_keypair.pubkey()));
    let setup_tx = Transaction::new(
        &[mint_keypair, &custodian_keypair, &nonce_keypair],
        message,
        bank.last_blockhash(),
    );
    bank.process_transaction(&setup_tx)?;
    Ok((custodian_keypair, nonce_keypair))
}

type NonceSetup = (Arc<Bank>, Keypair, Keypair, Keypair, Arc<RwLock<BankForks>>);

pub(in crate::bank) fn setup_nonce_with_bank<F>(
    supply_lamports: u64,
    mut genesis_cfg_fn: F,
    custodian_lamports: u64,
    nonce_lamports: u64,
    nonce_authority: Option<Pubkey>,
    feature_set: FeatureSet,
) -> Result<NonceSetup>
where
    F: FnMut(&mut GenesisConfig),
{
    let (mut genesis_config, mint_keypair) = create_genesis_config(supply_lamports);
    genesis_config.rent.lamports_per_byte_year = 0;
    genesis_cfg_fn(&mut genesis_config);
    let mut bank = Bank::new_for_tests(&genesis_config);
    bank.feature_set = Arc::new(feature_set);
    let (mut bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();

    // Banks 0 and 1 have no fees, wait two blocks before
    // initializing our nonce accounts
    for _ in 0..2 {
        goto_end_of_slot(bank.clone());
        bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
    }

    let (custodian_keypair, nonce_keypair) = nonce_setup(
        &bank,
        &mint_keypair,
        custodian_lamports,
        nonce_lamports,
        nonce_authority,
    )?;

    // The setup nonce is not valid to be used until the next bank
    // so wait one more block
    goto_end_of_slot(bank.clone());
    bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());

    Ok((
        bank,
        mint_keypair,
        custodian_keypair,
        nonce_keypair,
        bank_forks,
    ))
}

impl Bank {
    pub(in crate::bank) fn next_durable_nonce(&self) -> DurableNonce {
        let hash_queue = self.blockhash_queue.read().unwrap();
        let last_blockhash = hash_queue.last_hash();
        DurableNonce::from_blockhash(&last_blockhash)
    }
}

#[test]
fn test_assign_from_nonce_account_fail() {
    let (bank, _bank_forks) = create_simple_test_arc_bank(100_000_000);
    let nonce = Keypair::new();
    let nonce_account = AccountSharedData::new_data(
        42_424_242,
        &nonce::versions::Versions::new(nonce::state::State::Initialized(
            nonce::state::Data::default(),
        )),
        &system_program::id(),
    )
    .unwrap();
    let blockhash = bank.last_blockhash();
    bank.store_account(&nonce.pubkey(), &nonce_account);

    let ix = system_instruction::assign(&nonce.pubkey(), &Pubkey::from([9u8; 32]));
    let message = Message::new(&[ix], Some(&nonce.pubkey()));
    let tx = Transaction::new(&[&nonce], message, blockhash);

    let expect = Err(TransactionError::InstructionError(
        0,
        InstructionError::ModifiedProgramId,
    ));
    assert_eq!(bank.process_transaction(&tx), expect);
}

#[test]
fn test_nonce_must_be_advanceable() {
    let mut bank = create_simple_test_bank(100_000_000);
    bank.feature_set = Arc::new(FeatureSet::all_enabled());
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let nonce_keypair = Keypair::new();
    let nonce_authority = nonce_keypair.pubkey();
    let durable_nonce = DurableNonce::from_blockhash(&bank.last_blockhash());
    let nonce_account =
        AccountSharedData::new_data(
            42_424_242,
            &nonce::versions::Versions::new(nonce::state::State::Initialized(
                nonce::state::Data::new(nonce_authority, durable_nonce, 5000),
            )),
            &system_program::id(),
        )
        .unwrap();
    bank.store_account(&nonce_keypair.pubkey(), &nonce_account);

    let ix = system_instruction::advance_nonce_account(&nonce_keypair.pubkey(), &nonce_authority);
    let message = Message::new(&[ix], Some(&nonce_keypair.pubkey()));
    let tx = Transaction::new(&[&nonce_keypair], message, *durable_nonce.as_hash());
    assert_eq!(
        bank.process_transaction(&tx),
        Err(TransactionError::BlockhashNotFound)
    );
}

#[test]
fn test_nonce_transaction() {
    let (mut bank, _mint_keypair, custodian_keypair, nonce_keypair, bank_forks) =
        setup_nonce_with_bank(
            10_000_000,
            |_| {},
            5_000_000,
            250_000,
            None,
            FeatureSet::all_enabled(),
        )
        .unwrap();
    let alice_keypair = Keypair::new();
    let alice_pubkey = alice_keypair.pubkey();
    let custodian_pubkey = custodian_keypair.pubkey();
    let nonce_pubkey = nonce_keypair.pubkey();

    assert_eq!(bank.get_balance(&custodian_pubkey), 4_750_000);
    assert_eq!(bank.get_balance(&nonce_pubkey), 250_000);

    /* Grab the hash stored in the nonce account */
    let nonce_hash = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();

    /* Kick nonce hash off the blockhash_queue */
    for _ in 0..MAX_RECENT_BLOCKHASHES + 1 {
        goto_end_of_slot(bank.clone());
        bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
    }

    /* Expect a non-Nonce transfer to fail */
    assert_eq!(
        bank.process_transaction(&system_transaction::transfer(
            &custodian_keypair,
            &alice_pubkey,
            100_000,
            nonce_hash
        ),),
        Err(TransactionError::BlockhashNotFound),
    );
    /* Check fee not charged */
    assert_eq!(bank.get_balance(&custodian_pubkey), 4_750_000);

    /* Nonce transfer */
    let nonce_tx = Transaction::new_signed_with_payer(
        &[
            system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
            system_instruction::transfer(&custodian_pubkey, &alice_pubkey, 100_000),
        ],
        Some(&custodian_pubkey),
        &[&custodian_keypair, &nonce_keypair],
        nonce_hash,
    );
    assert_eq!(bank.process_transaction(&nonce_tx), Ok(()));

    /* Check balances */
    let mut recent_message = nonce_tx.message;
    recent_message.recent_blockhash = bank.last_blockhash();
    let mut expected_balance = 4_650_000
        - bank
            .get_fee_for_message(&new_sanitized_message(recent_message))
            .unwrap();
    assert_eq!(bank.get_balance(&custodian_pubkey), expected_balance);
    assert_eq!(bank.get_balance(&nonce_pubkey), 250_000);
    assert_eq!(bank.get_balance(&alice_pubkey), 100_000);

    /* Confirm stored nonce has advanced */
    let new_nonce = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();
    assert_ne!(nonce_hash, new_nonce);

    /* Nonce reuse fails */
    let nonce_tx = Transaction::new_signed_with_payer(
        &[
            system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
            system_instruction::transfer(&custodian_pubkey, &alice_pubkey, 100_000),
        ],
        Some(&custodian_pubkey),
        &[&custodian_keypair, &nonce_keypair],
        nonce_hash,
    );
    assert_eq!(
        bank.process_transaction(&nonce_tx),
        Err(TransactionError::BlockhashNotFound)
    );
    /* Check fee not charged and nonce not advanced */
    assert_eq!(bank.get_balance(&custodian_pubkey), expected_balance);
    assert_eq!(
        new_nonce,
        get_nonce_blockhash(&bank, &nonce_pubkey).unwrap()
    );

    let nonce_hash = new_nonce;

    /* Kick nonce hash off the blockhash_queue */
    for _ in 0..MAX_RECENT_BLOCKHASHES + 1 {
        goto_end_of_slot(bank.clone());
        bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
    }

    let nonce_tx = Transaction::new_signed_with_payer(
        &[
            system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
            system_instruction::transfer(&custodian_pubkey, &alice_pubkey, 100_000_000),
        ],
        Some(&custodian_pubkey),
        &[&custodian_keypair, &nonce_keypair],
        nonce_hash,
    );
    assert_eq!(
        bank.process_transaction(&nonce_tx),
        Err(TransactionError::InstructionError(
            1,
            solana_system_interface::error::SystemError::ResultWithNegativeLamports.into(),
        ))
    );
    /* Check fee charged and nonce has advanced */
    let mut recent_message = nonce_tx.message.clone();
    recent_message.recent_blockhash = bank.last_blockhash();
    expected_balance -= bank
        .get_fee_for_message(&new_sanitized_message(recent_message))
        .unwrap();
    assert_eq!(bank.get_balance(&custodian_pubkey), expected_balance);
    assert_ne!(
        nonce_hash,
        get_nonce_blockhash(&bank, &nonce_pubkey).unwrap()
    );
    /* Confirm replaying a TX that failed with InstructionError::* now
     * fails with TransactionError::BlockhashNotFound
     */
    assert_eq!(
        bank.process_transaction(&nonce_tx),
        Err(TransactionError::BlockhashNotFound),
    );
}

#[test]
fn test_nonce_transaction_with_tx_wide_caps() {
    let feature_set = FeatureSet::all_enabled();
    let (mut bank, _mint_keypair, custodian_keypair, nonce_keypair, bank_forks) =
        setup_nonce_with_bank(10_000_000, |_| {}, 5_000_000, 250_000, None, feature_set).unwrap();
    let alice_keypair = Keypair::new();
    let alice_pubkey = alice_keypair.pubkey();
    let custodian_pubkey = custodian_keypair.pubkey();
    let nonce_pubkey = nonce_keypair.pubkey();

    assert_eq!(bank.get_balance(&custodian_pubkey), 4_750_000);
    assert_eq!(bank.get_balance(&nonce_pubkey), 250_000);

    /* Grab the hash stored in the nonce account */
    let nonce_hash = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();

    /* Kick nonce hash off the blockhash_queue */
    for _ in 0..MAX_RECENT_BLOCKHASHES + 1 {
        goto_end_of_slot(bank.clone());
        bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
    }

    /* Expect a non-Nonce transfer to fail */
    assert_eq!(
        bank.process_transaction(&system_transaction::transfer(
            &custodian_keypair,
            &alice_pubkey,
            100_000,
            nonce_hash
        ),),
        Err(TransactionError::BlockhashNotFound),
    );
    /* Check fee not charged */
    assert_eq!(bank.get_balance(&custodian_pubkey), 4_750_000);

    /* Nonce transfer */
    let nonce_tx = Transaction::new_signed_with_payer(
        &[
            system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
            system_instruction::transfer(&custodian_pubkey, &alice_pubkey, 100_000),
        ],
        Some(&custodian_pubkey),
        &[&custodian_keypair, &nonce_keypair],
        nonce_hash,
    );
    assert_eq!(bank.process_transaction(&nonce_tx), Ok(()));

    /* Check balances */
    let mut recent_message = nonce_tx.message;
    recent_message.recent_blockhash = bank.last_blockhash();
    let mut expected_balance = 4_650_000
        - bank
            .get_fee_for_message(&new_sanitized_message(recent_message))
            .unwrap();
    assert_eq!(bank.get_balance(&custodian_pubkey), expected_balance);
    assert_eq!(bank.get_balance(&nonce_pubkey), 250_000);
    assert_eq!(bank.get_balance(&alice_pubkey), 100_000);

    /* Confirm stored nonce has advanced */
    let new_nonce = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();
    assert_ne!(nonce_hash, new_nonce);

    /* Nonce reuse fails */
    let nonce_tx = Transaction::new_signed_with_payer(
        &[
            system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
            system_instruction::transfer(&custodian_pubkey, &alice_pubkey, 100_000),
        ],
        Some(&custodian_pubkey),
        &[&custodian_keypair, &nonce_keypair],
        nonce_hash,
    );
    assert_eq!(
        bank.process_transaction(&nonce_tx),
        Err(TransactionError::BlockhashNotFound)
    );
    /* Check fee not charged and nonce not advanced */
    assert_eq!(bank.get_balance(&custodian_pubkey), expected_balance);
    assert_eq!(
        new_nonce,
        get_nonce_blockhash(&bank, &nonce_pubkey).unwrap()
    );

    let nonce_hash = new_nonce;

    /* Kick nonce hash off the blockhash_queue */
    for _ in 0..MAX_RECENT_BLOCKHASHES + 1 {
        goto_end_of_slot(bank.clone());
        bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
    }

    let nonce_tx = Transaction::new_signed_with_payer(
        &[
            system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
            system_instruction::transfer(&custodian_pubkey, &alice_pubkey, 100_000_000),
        ],
        Some(&custodian_pubkey),
        &[&custodian_keypair, &nonce_keypair],
        nonce_hash,
    );
    assert_eq!(
        bank.process_transaction(&nonce_tx),
        Err(TransactionError::InstructionError(
            1,
            solana_system_interface::error::SystemError::ResultWithNegativeLamports.into(),
        ))
    );
    /* Check fee charged and nonce has advanced */
    let mut recent_message = nonce_tx.message.clone();
    recent_message.recent_blockhash = bank.last_blockhash();
    expected_balance -= bank
        .get_fee_for_message(&new_sanitized_message(recent_message))
        .unwrap();
    assert_eq!(bank.get_balance(&custodian_pubkey), expected_balance);
    assert_ne!(
        nonce_hash,
        get_nonce_blockhash(&bank, &nonce_pubkey).unwrap()
    );
    /* Confirm replaying a TX that failed with InstructionError::* now
     * fails with TransactionError::BlockhashNotFound
     */
    assert_eq!(
        bank.process_transaction(&nonce_tx),
        Err(TransactionError::BlockhashNotFound),
    );
}

#[test]
fn test_nonce_authority() {
    agave_logger::setup();
    let (mut bank, _mint_keypair, custodian_keypair, nonce_keypair, bank_forks) =
        setup_nonce_with_bank(
            10_000_000,
            |_| {},
            5_000_000,
            250_000,
            None,
            FeatureSet::all_enabled(),
        )
        .unwrap();
    let alice_keypair = Keypair::new();
    let alice_pubkey = alice_keypair.pubkey();
    let custodian_pubkey = custodian_keypair.pubkey();
    let nonce_pubkey = nonce_keypair.pubkey();
    let bad_nonce_authority_keypair = Keypair::new();
    let bad_nonce_authority = bad_nonce_authority_keypair.pubkey();
    let custodian_account = bank.get_account(&custodian_pubkey).unwrap();

    debug!("alice: {alice_pubkey}");
    debug!("custodian: {custodian_pubkey}");
    debug!("nonce: {nonce_pubkey}");
    debug!("nonce account: {:?}", bank.get_account(&nonce_pubkey));
    debug!("cust: {custodian_account:?}");
    let nonce_hash = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();

    for _ in 0..MAX_RECENT_BLOCKHASHES + 1 {
        goto_end_of_slot(bank.clone());
        bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
    }

    let nonce_tx = Transaction::new_signed_with_payer(
        &[
            system_instruction::advance_nonce_account(&nonce_pubkey, &bad_nonce_authority),
            system_instruction::transfer(&custodian_pubkey, &alice_pubkey, 42),
        ],
        Some(&custodian_pubkey),
        &[&custodian_keypair, &bad_nonce_authority_keypair],
        nonce_hash,
    );
    debug!("{nonce_tx:?}");
    let initial_custodian_balance = custodian_account.lamports();
    assert_eq!(
        bank.process_transaction(&nonce_tx),
        Err(TransactionError::BlockhashNotFound),
    );
    /* Check fee was *not* charged and nonce has *not* advanced */
    let mut recent_message = nonce_tx.message;
    recent_message.recent_blockhash = bank.last_blockhash();
    assert_eq!(
        bank.get_balance(&custodian_pubkey),
        initial_custodian_balance
    );
    assert_eq!(
        nonce_hash,
        get_nonce_blockhash(&bank, &nonce_pubkey).unwrap()
    );
}

#[test]
fn test_nonce_payer() {
    agave_logger::setup();
    let nonce_starting_balance = 250_000;
    let (mut bank, _mint_keypair, custodian_keypair, nonce_keypair, bank_forks) =
        setup_nonce_with_bank(
            10_000_000,
            |_| {},
            5_000_000,
            nonce_starting_balance,
            None,
            FeatureSet::all_enabled(),
        )
        .unwrap();
    let alice_keypair = Keypair::new();
    let alice_pubkey = alice_keypair.pubkey();
    let custodian_pubkey = custodian_keypair.pubkey();
    let nonce_pubkey = nonce_keypair.pubkey();

    debug!("alice: {alice_pubkey}");
    debug!("custodian: {custodian_pubkey}");
    debug!("nonce: {nonce_pubkey}");
    debug!("nonce account: {:?}", bank.get_account(&nonce_pubkey));
    debug!("cust: {:?}", bank.get_account(&custodian_pubkey));
    let nonce_hash = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();

    for _ in 0..MAX_RECENT_BLOCKHASHES + 1 {
        goto_end_of_slot(bank.clone());
        bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
    }

    let nonce_tx = Transaction::new_signed_with_payer(
        &[
            system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
            system_instruction::transfer(&custodian_pubkey, &alice_pubkey, 100_000_000),
        ],
        Some(&nonce_pubkey),
        &[&custodian_keypair, &nonce_keypair],
        nonce_hash,
    );
    debug!("{nonce_tx:?}");
    assert_eq!(
        bank.process_transaction(&nonce_tx),
        Err(TransactionError::InstructionError(
            1,
            solana_system_interface::error::SystemError::ResultWithNegativeLamports.into(),
        ))
    );
    /* Check fee charged and nonce has advanced */
    let mut recent_message = nonce_tx.message;
    recent_message.recent_blockhash = bank.last_blockhash();
    assert_eq!(
        bank.get_balance(&nonce_pubkey),
        nonce_starting_balance
            - bank
                .get_fee_for_message(&new_sanitized_message(recent_message))
                .unwrap()
    );
    assert_ne!(
        nonce_hash,
        get_nonce_blockhash(&bank, &nonce_pubkey).unwrap()
    );
}

#[test]
fn test_nonce_payer_tx_wide_cap() {
    agave_logger::setup();
    let nonce_starting_balance =
        250_000 + FeeStructure::default().compute_fee_bins.last().unwrap().fee;
    let feature_set = FeatureSet::all_enabled();
    let (mut bank, _mint_keypair, custodian_keypair, nonce_keypair, bank_forks) =
        setup_nonce_with_bank(
            10_000_000,
            |_| {},
            5_000_000,
            nonce_starting_balance,
            None,
            feature_set,
        )
        .unwrap();
    let alice_keypair = Keypair::new();
    let alice_pubkey = alice_keypair.pubkey();
    let custodian_pubkey = custodian_keypair.pubkey();
    let nonce_pubkey = nonce_keypair.pubkey();

    debug!("alice: {alice_pubkey}");
    debug!("custodian: {custodian_pubkey}");
    debug!("nonce: {nonce_pubkey}");
    debug!("nonce account: {:?}", bank.get_account(&nonce_pubkey));
    debug!("cust: {:?}", bank.get_account(&custodian_pubkey));
    let nonce_hash = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();

    for _ in 0..MAX_RECENT_BLOCKHASHES + 1 {
        goto_end_of_slot(bank.clone());
        bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
    }

    let nonce_tx = Transaction::new_signed_with_payer(
        &[
            system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
            system_instruction::transfer(&custodian_pubkey, &alice_pubkey, 100_000_000),
        ],
        Some(&nonce_pubkey),
        &[&custodian_keypair, &nonce_keypair],
        nonce_hash,
    );
    debug!("{nonce_tx:?}");

    assert_eq!(
        bank.process_transaction(&nonce_tx),
        Err(TransactionError::InstructionError(
            1,
            solana_system_interface::error::SystemError::ResultWithNegativeLamports.into(),
        ))
    );
    /* Check fee charged and nonce has advanced */
    let mut recent_message = nonce_tx.message;
    recent_message.recent_blockhash = bank.last_blockhash();
    assert_eq!(
        bank.get_balance(&nonce_pubkey),
        nonce_starting_balance
            - bank
                .get_fee_for_message(&new_sanitized_message(recent_message))
                .unwrap()
    );
    assert_ne!(
        nonce_hash,
        get_nonce_blockhash(&bank, &nonce_pubkey).unwrap()
    );
}

#[test]
fn test_nonce_fee_calculator_updates() {
    let (mut genesis_config, mint_keypair) = create_genesis_config(1_000_000);
    genesis_config.rent.lamports_per_byte_year = 0;
    let mut bank = Bank::new_for_tests(&genesis_config);
    bank.feature_set = Arc::new(FeatureSet::all_enabled());
    let (mut bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();

    // Deliberately use bank 0 to initialize nonce account, so that nonce account fee_calculator indicates 0 fees
    let (custodian_keypair, nonce_keypair) =
        nonce_setup(&bank, &mint_keypair, 500_000, 100_000, None).unwrap();
    let custodian_pubkey = custodian_keypair.pubkey();
    let nonce_pubkey = nonce_keypair.pubkey();

    // Grab the hash and fee_calculator stored in the nonce account
    let (stored_nonce_hash, stored_fee_calculator) = bank
        .get_account(&nonce_pubkey)
        .and_then(|acc| {
            let nonce_versions = StateMut::<nonce::versions::Versions>::state(&acc);
            match nonce_versions.ok()?.state() {
                nonce::state::State::Initialized(data) => {
                    Some((data.blockhash(), data.fee_calculator))
                }
                _ => None,
            }
        })
        .unwrap();

    // Kick nonce hash off the blockhash_queue
    for _ in 0..MAX_RECENT_BLOCKHASHES + 1 {
        goto_end_of_slot(bank.clone());
        bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
    }

    // Nonce transfer
    let nonce_tx = Transaction::new_signed_with_payer(
        &[
            system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
            system_instruction::transfer(&custodian_pubkey, &solana_pubkey::new_rand(), 100_000),
        ],
        Some(&custodian_pubkey),
        &[&custodian_keypair, &nonce_keypair],
        stored_nonce_hash,
    );
    bank.process_transaction(&nonce_tx).unwrap();

    // Grab the new hash and fee_calculator; both should be updated
    let (nonce_hash, fee_calculator) = bank
        .get_account(&nonce_pubkey)
        .and_then(|acc| {
            let nonce_versions = StateMut::<nonce::versions::Versions>::state(&acc);
            match nonce_versions.ok()?.state() {
                nonce::state::State::Initialized(data) => {
                    Some((data.blockhash(), data.fee_calculator))
                }
                _ => None,
            }
        })
        .unwrap();

    assert_ne!(stored_nonce_hash, nonce_hash);
    assert_ne!(stored_fee_calculator, fee_calculator);
}

#[test]
fn test_nonce_fee_calculator_updates_tx_wide_cap() {
    let (mut genesis_config, mint_keypair) = create_genesis_config(1_000_000);
    genesis_config.rent.lamports_per_byte_year = 0;
    let mut bank = Bank::new_for_tests(&genesis_config);
    bank.feature_set = Arc::new(FeatureSet::all_enabled());
    let (mut bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();

    // Deliberately use bank 0 to initialize nonce account, so that nonce account fee_calculator indicates 0 fees
    let (custodian_keypair, nonce_keypair) =
        nonce_setup(&bank, &mint_keypair, 500_000, 100_000, None).unwrap();
    let custodian_pubkey = custodian_keypair.pubkey();
    let nonce_pubkey = nonce_keypair.pubkey();

    // Grab the hash and fee_calculator stored in the nonce account
    let (stored_nonce_hash, stored_fee_calculator) = bank
        .get_account(&nonce_pubkey)
        .and_then(|acc| {
            let nonce_versions = StateMut::<nonce::versions::Versions>::state(&acc);
            match nonce_versions.ok()?.state() {
                nonce::state::State::Initialized(data) => {
                    Some((data.blockhash(), data.fee_calculator))
                }
                _ => None,
            }
        })
        .unwrap();

    // Kick nonce hash off the blockhash_queue
    for _ in 0..MAX_RECENT_BLOCKHASHES + 1 {
        goto_end_of_slot(bank.clone());
        bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
    }

    // Nonce transfer
    let nonce_tx = Transaction::new_signed_with_payer(
        &[
            system_instruction::advance_nonce_account(&nonce_pubkey, &nonce_pubkey),
            system_instruction::transfer(&custodian_pubkey, &solana_pubkey::new_rand(), 100_000),
        ],
        Some(&custodian_pubkey),
        &[&custodian_keypair, &nonce_keypair],
        stored_nonce_hash,
    );
    bank.process_transaction(&nonce_tx).unwrap();

    // Grab the new hash and fee_calculator; both should be updated
    let (nonce_hash, fee_calculator) = bank
        .get_account(&nonce_pubkey)
        .and_then(|acc| {
            let nonce_versions = StateMut::<nonce::versions::Versions>::state(&acc);
            match nonce_versions.ok()?.state() {
                nonce::state::State::Initialized(data) => {
                    Some((data.blockhash(), data.fee_calculator))
                }
                _ => None,
            }
        })
        .unwrap();

    assert_ne!(stored_nonce_hash, nonce_hash);
    assert_ne!(stored_fee_calculator, fee_calculator);
}

#[test]
fn test_check_ro_durable_nonce_fails() {
    let (mut bank, _mint_keypair, custodian_keypair, nonce_keypair, bank_forks) =
        setup_nonce_with_bank(
            10_000_000,
            |_| {},
            5_000_000,
            250_000,
            None,
            FeatureSet::all_enabled(),
        )
        .unwrap();
    let custodian_pubkey = custodian_keypair.pubkey();
    let nonce_pubkey = nonce_keypair.pubkey();

    let nonce_hash = get_nonce_blockhash(&bank, &nonce_pubkey).unwrap();
    let account_metas = vec![
        AccountMeta::new_readonly(nonce_pubkey, false),
        #[allow(deprecated)]
        AccountMeta::new_readonly(sysvar::recent_blockhashes::id(), false),
        AccountMeta::new_readonly(nonce_pubkey, true),
    ];
    let nonce_instruction = Instruction::new_with_bincode(
        system_program::id(),
        &system_instruction::SystemInstruction::AdvanceNonceAccount,
        account_metas,
    );
    let tx = Transaction::new_signed_with_payer(
        &[nonce_instruction],
        Some(&custodian_pubkey),
        &[&custodian_keypair, &nonce_keypair],
        nonce_hash,
    );
    // SanitizedMessage::get_durable_nonce returns None because nonce
    // account is not writable. Durable nonce and blockhash domains are
    // separate, so the recent_blockhash (== durable nonce) in the
    // transaction is not found in the hash queue.
    assert_eq!(
        bank.process_transaction(&tx),
        Err(TransactionError::BlockhashNotFound),
    );
    // Kick nonce hash off the blockhash_queue
    for _ in 0..MAX_RECENT_BLOCKHASHES + 1 {
        goto_end_of_slot(bank.clone());
        bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref())
    }
    // Caught by the runtime because it is a nonce transaction
    assert_eq!(
        bank.process_transaction(&tx),
        Err(TransactionError::BlockhashNotFound)
    );
    let (_, lamports_per_signature) = bank.last_blockhash_and_lamports_per_signature();
    assert_eq!(
        bank.check_load_and_advance_message_nonce_account(
            &new_sanitized_message(tx.message().clone()),
            &bank.next_durable_nonce(),
            lamports_per_signature,
        ),
        None
    );
}

#[test]
fn test_collect_balances() {
    let (parent, _bank_forks) = create_simple_test_arc_bank(500);
    let bank0 = Arc::new(new_from_parent(parent));

    let keypair = Keypair::new();
    let pubkey0 = solana_pubkey::new_rand();
    let pubkey1 = solana_pubkey::new_rand();
    let program_id = Pubkey::from([2; 32]);
    let keypair_account = AccountSharedData::new(8, 0, &program_id);
    let account0 = AccountSharedData::new(11, 0, &program_id);
    let program_account = AccountSharedData::new(1, 10, &Pubkey::default());
    bank0.store_account(&keypair.pubkey(), &keypair_account);
    bank0.store_account(&pubkey0, &account0);
    bank0.store_account(&program_id, &program_account);

    let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
    let tx0 = Transaction::new_with_compiled_instructions(
        &[&keypair],
        &[pubkey0],
        Hash::default(),
        vec![program_id],
        instructions,
    );
    let instructions = vec![CompiledInstruction::new(1, &(), vec![0])];
    let tx1 = Transaction::new_with_compiled_instructions(
        &[&keypair],
        &[pubkey1],
        Hash::default(),
        vec![program_id],
        instructions,
    );
    let txs = vec![tx0, tx1];
    let batch = bank0.prepare_batch_for_tests(txs.clone());
    let balances = bank0.collect_balances(&batch);
    assert_eq!(balances.len(), 2);
    assert_eq!(balances[0], vec![8, 11, 1]);
    assert_eq!(balances[1], vec![8, 0, 1]);

    let txs: Vec<_> = txs.into_iter().rev().collect();
    let batch = bank0.prepare_batch_for_tests(txs);
    let balances = bank0.collect_balances(&batch);
    assert_eq!(balances.len(), 2);
    assert_eq!(balances[0], vec![8, 0, 1]);
    assert_eq!(balances[1], vec![8, 11, 1]);
}

#[test]
fn test_pre_post_transaction_balances() {
    let (mut genesis_config, _mint_keypair) = create_genesis_config(500_000);
    let fee_rate_governor = FeeRateGovernor::new(5000, 0);
    genesis_config.fee_rate_governor = fee_rate_governor;
    let (parent, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let bank0 = new_from_parent_with_fork_next_slot(parent, bank_forks.as_ref());

    let keypair0 = Keypair::new();
    let keypair1 = Keypair::new();
    let pubkey0 = solana_pubkey::new_rand();
    let pubkey1 = solana_pubkey::new_rand();
    let pubkey2 = solana_pubkey::new_rand();
    let keypair0_account = AccountSharedData::new(908_000, 0, &Pubkey::default());
    let keypair1_account = AccountSharedData::new(909_000, 0, &Pubkey::default());
    let account0 = AccountSharedData::new(911_000, 0, &Pubkey::default());
    bank0.store_account(&keypair0.pubkey(), &keypair0_account);
    bank0.store_account(&keypair1.pubkey(), &keypair1_account);
    bank0.store_account(&pubkey0, &account0);

    let blockhash = bank0.last_blockhash();

    let tx0 = system_transaction::transfer(&keypair0, &pubkey0, 2_000, blockhash);
    let tx1 = system_transaction::transfer(&Keypair::new(), &pubkey1, 2_000, blockhash);
    let tx2 = system_transaction::transfer(&keypair1, &pubkey2, 912_000, blockhash);
    let txs = vec![tx0, tx1, tx2];

    let lock_result = bank0.prepare_batch_for_tests(txs);
    let (commit_results, balance_collector) = bank0.load_execute_and_commit_transactions(
        &lock_result,
        MAX_PROCESSING_AGE,
        ExecutionRecordingConfig {
            enable_cpi_recording: false,
            enable_log_recording: false,
            enable_return_data_recording: false,
            enable_transaction_balance_recording: true,
        },
        &mut ExecuteTimings::default(),
        None,
    );

    let (native_pre, native_post, _, _) = balance_collector.unwrap().into_vecs();
    let transaction_balances_set = TransactionBalancesSet::new(native_pre, native_post);

    assert_eq!(transaction_balances_set.pre_balances.len(), 3);
    assert_eq!(transaction_balances_set.post_balances.len(), 3);

    assert!(commit_results[0].was_executed_successfully());
    assert_eq!(
        transaction_balances_set.pre_balances[0],
        vec![908_000, 911_000, 1]
    );
    assert_eq!(
        transaction_balances_set.post_balances[0],
        vec![901_000, 913_000, 1]
    );

    // Failed transactions still produce balance sets
    // This is a TransactionError - not possible to charge fees
    assert_matches!(commit_results[1], Err(TransactionError::AccountNotFound));
    assert_eq!(transaction_balances_set.pre_balances[1], vec![0, 0, 1]);
    assert_eq!(transaction_balances_set.post_balances[1], vec![0, 0, 1]);

    // Failed transactions still produce balance sets
    // This is an InstructionError - fees charged
    assert_eq!(
        commit_results[2].as_ref().unwrap().status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::Custom(1),
        )),
    );
    assert_eq!(
        transaction_balances_set.pre_balances[2],
        vec![909_000, 0, 1]
    );
    assert_eq!(
        transaction_balances_set.post_balances[2],
        vec![904_000, 0, 1]
    );
}

#[test]
fn test_transaction_with_duplicate_accounts_in_instruction() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee_no_rent(500);

    let mock_program_id = Pubkey::from([2u8; 32]);
    let (bank, _bank_forks) =
        Bank::new_with_mockup_builtin_for_tests(&genesis_config, mock_program_id, MockBuiltin::vm);

    declare_process_instruction!(MockBuiltin, 1, |invoke_context| {
        let transaction_context = &invoke_context.transaction_context;
        let instruction_context = transaction_context.get_current_instruction_context()?;
        let instruction_data = instruction_context.get_instruction_data();
        let lamports = u64::from_le_bytes(instruction_data.try_into().unwrap());
        instruction_context
            .try_borrow_instruction_account(2)?
            .checked_sub_lamports(lamports)?;
        instruction_context
            .try_borrow_instruction_account(1)?
            .checked_add_lamports(lamports)?;
        instruction_context
            .try_borrow_instruction_account(0)?
            .checked_sub_lamports(lamports)?;
        instruction_context
            .try_borrow_instruction_account(1)?
            .checked_add_lamports(lamports)?;
        Ok(())
    });

    let from_pubkey = solana_pubkey::new_rand();
    let to_pubkey = solana_pubkey::new_rand();
    let dup_pubkey = from_pubkey;
    let from_account = AccountSharedData::new(100 * LAMPORTS_PER_SOL, 1, &mock_program_id);
    let to_account = AccountSharedData::new(0, 1, &mock_program_id);
    bank.store_account(&from_pubkey, &from_account);
    bank.store_account(&to_pubkey, &to_account);

    let account_metas = vec![
        AccountMeta::new(from_pubkey, false),
        AccountMeta::new(to_pubkey, false),
        AccountMeta::new(dup_pubkey, false),
    ];
    let instruction =
        Instruction::new_with_bincode(mock_program_id, &(10 * LAMPORTS_PER_SOL), account_metas);
    let tx = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair],
        bank.last_blockhash(),
    );

    let result = bank.process_transaction(&tx);
    assert_eq!(result, Ok(()));
    assert_eq!(bank.get_balance(&from_pubkey), 80 * LAMPORTS_PER_SOL);
    assert_eq!(bank.get_balance(&to_pubkey), 20 * LAMPORTS_PER_SOL);
}

#[test]
fn test_transaction_with_program_ids_passed_to_programs() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee_no_rent(500);

    let mock_program_id = Pubkey::from([2u8; 32]);
    let (bank, _bank_forks) =
        Bank::new_with_mockup_builtin_for_tests(&genesis_config, mock_program_id, MockBuiltin::vm);

    let from_pubkey = solana_pubkey::new_rand();
    let to_pubkey = solana_pubkey::new_rand();
    let dup_pubkey = from_pubkey;
    let from_account = AccountSharedData::new(100, 1, &mock_program_id);
    let to_account = AccountSharedData::new(0, 1, &mock_program_id);
    bank.store_account(&from_pubkey, &from_account);
    bank.store_account(&to_pubkey, &to_account);

    let account_metas = vec![
        AccountMeta::new(from_pubkey, false),
        AccountMeta::new(to_pubkey, false),
        AccountMeta::new(dup_pubkey, false),
        AccountMeta::new(mock_program_id, false),
    ];
    let instruction = Instruction::new_with_bincode(mock_program_id, &10, account_metas);
    let tx = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair],
        bank.last_blockhash(),
    );

    let result = bank.process_transaction(&tx);
    assert_eq!(result, Ok(()));
}

#[test]
fn test_account_ids_after_program_ids() {
    agave_logger::setup();
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee_no_rent(500);
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    let from_pubkey = solana_pubkey::new_rand();
    let to_pubkey = solana_pubkey::new_rand();

    let account_metas = vec![
        AccountMeta::new(from_pubkey, false),
        AccountMeta::new(to_pubkey, false),
    ];

    let instruction = Instruction::new_with_bincode(solana_vote_program::id(), &10, account_metas);
    let mut tx = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair],
        bank.last_blockhash(),
    );

    tx.message.account_keys.push(solana_pubkey::new_rand());

    let slot = bank.slot().saturating_add(1);
    let mut bank = Bank::new_from_parent(bank, &Pubkey::default(), slot);
    bank.add_mockup_builtin(solana_vote_program::id(), MockBuiltin::vm);
    let bank = bank_forks
        .write()
        .unwrap()
        .insert(bank)
        .clone_without_scheduler();

    let result = bank.process_transaction(&tx);
    assert_eq!(result, Ok(()));
    let account = bank.get_account(&solana_vote_program::id()).unwrap();
    info!("account: {account:?}");
    assert!(account.executable());
}

#[test]
fn test_incinerator() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee_no_rent(1_000_000_000_000);
    let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    // Move to the first normal slot so normal rent behaviour applies
    let bank = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank0,
        &Pubkey::default(),
        genesis_config.epoch_schedule.first_normal_slot,
    );
    let pre_capitalization = bank.capitalization();

    // Burn a non-rent exempt amount
    let burn_amount = bank.get_minimum_balance_for_rent_exemption(0) - 1;

    assert_eq!(bank.get_balance(&incinerator::id()), 0);
    bank.transfer(burn_amount, &mint_keypair, &incinerator::id())
        .unwrap();
    assert_eq!(bank.get_balance(&incinerator::id()), burn_amount);
    bank.freeze();
    assert_eq!(bank.get_balance(&incinerator::id()), 0);

    // Ensure that no rent was collected, and the entire burn amount was removed from bank
    // capitalization
    assert_eq!(bank.capitalization(), pre_capitalization - burn_amount);
}

#[test]
fn test_duplicate_account_key() {
    agave_logger::setup();
    let (genesis_config, mint_keypair) = create_genesis_config(500);
    let (bank, _bank_forks) = Bank::new_with_mockup_builtin_for_tests(
        &genesis_config,
        solana_vote_program::id(),
        MockBuiltin::vm,
    );

    let from_pubkey = solana_pubkey::new_rand();
    let to_pubkey = solana_pubkey::new_rand();

    let account_metas = vec![
        AccountMeta::new(from_pubkey, false),
        AccountMeta::new(to_pubkey, false),
    ];

    let instruction = Instruction::new_with_bincode(solana_vote_program::id(), &10, account_metas);
    let mut tx = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair],
        bank.last_blockhash(),
    );
    tx.message.account_keys.push(from_pubkey);

    let result = bank.process_transaction(&tx);
    assert_eq!(result, Err(TransactionError::AccountLoadedTwice));
}

#[test]
fn test_process_transaction_with_too_many_account_locks() {
    agave_logger::setup();
    let (genesis_config, mint_keypair) = create_genesis_config(500);
    let (bank, _bank_forks) = Bank::new_with_mockup_builtin_for_tests(
        &genesis_config,
        solana_vote_program::id(),
        MockBuiltin::vm,
    );

    let from_pubkey = solana_pubkey::new_rand();
    let to_pubkey = solana_pubkey::new_rand();

    let account_metas = vec![
        AccountMeta::new(from_pubkey, false),
        AccountMeta::new(to_pubkey, false),
    ];

    let instruction = Instruction::new_with_bincode(solana_vote_program::id(), &10, account_metas);
    let mut tx = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair],
        bank.last_blockhash(),
    );

    let transaction_account_lock_limit = bank.get_transaction_account_lock_limit();
    while tx.message.account_keys.len() <= transaction_account_lock_limit {
        tx.message.account_keys.push(solana_pubkey::new_rand());
    }

    let result = bank.process_transaction(&tx);
    assert_eq!(result, Err(TransactionError::TooManyAccountLocks));
}

#[test]
fn test_program_id_as_payer() {
    agave_logger::setup();
    let (genesis_config, mint_keypair) = create_genesis_config(500);
    let mut bank = Bank::new_for_tests(&genesis_config);

    let from_pubkey = solana_pubkey::new_rand();
    let to_pubkey = solana_pubkey::new_rand();

    let account_metas = vec![
        AccountMeta::new(from_pubkey, false),
        AccountMeta::new(to_pubkey, false),
    ];

    bank.add_mockup_builtin(solana_vote_program::id(), MockBuiltin::vm);

    let instruction = Instruction::new_with_bincode(solana_vote_program::id(), &10, account_metas);
    let mut tx = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair],
        bank.last_blockhash(),
    );

    info!(
        "mint: {} account keys: {:?}",
        mint_keypair.pubkey(),
        tx.message.account_keys
    );
    assert_eq!(tx.message.account_keys.len(), 4);
    tx.message.account_keys.clear();
    tx.message.account_keys.push(solana_vote_program::id());
    tx.message.account_keys.push(mint_keypair.pubkey());
    tx.message.account_keys.push(from_pubkey);
    tx.message.account_keys.push(to_pubkey);
    tx.message.instructions[0].program_id_index = 0;
    tx.message.instructions[0].accounts.clear();
    tx.message.instructions[0].accounts.push(2);
    tx.message.instructions[0].accounts.push(3);

    let result = bank.process_transaction(&tx);
    assert_eq!(result, Err(TransactionError::SanitizeFailure));
}

#[test]
fn test_ref_account_key_after_program_id() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee_no_rent(500);
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    let from_pubkey = solana_pubkey::new_rand();
    let to_pubkey = solana_pubkey::new_rand();

    let account_metas = vec![
        AccountMeta::new(from_pubkey, false),
        AccountMeta::new(to_pubkey, false),
    ];

    let slot = bank.slot().saturating_add(1);
    let mut bank = Bank::new_from_parent(bank, &Pubkey::default(), slot);
    bank.add_mockup_builtin(solana_vote_program::id(), MockBuiltin::vm);
    let bank = bank_forks
        .write()
        .unwrap()
        .insert(bank)
        .clone_without_scheduler();

    let instruction = Instruction::new_with_bincode(solana_vote_program::id(), &10, account_metas);
    let mut tx = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair],
        bank.last_blockhash(),
    );

    tx.message.account_keys.push(solana_pubkey::new_rand());
    assert_eq!(tx.message.account_keys.len(), 5);
    tx.message.instructions[0].accounts.remove(0);
    tx.message.instructions[0].accounts.push(4);

    let result = bank.process_transaction(&tx);
    assert_eq!(result, Ok(()));
}

#[test]
fn test_fuzz_instructions() {
    agave_logger::setup();
    use rand::{rng, Rng};
    let bank = create_simple_test_bank(1_000_000_000);

    let max_programs = 5;
    let program_keys: Vec<_> = (0..max_programs)
        .enumerate()
        .map(|i| {
            let key = solana_pubkey::new_rand();
            let name = format!("program{i:?}");
            bank.add_builtin(
                key,
                name.as_str(),
                ProgramCacheEntry::new_builtin(0, 0, MockBuiltin::vm),
            );
            (key, name.as_bytes().to_vec())
        })
        .collect();
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let max_keys = MAX_STATIC_ACCOUNTS_PER_PACKET;
    let keys: Vec<_> = (0..max_keys)
        .enumerate()
        .map(|_| {
            let key = solana_pubkey::new_rand();
            let balance = if rng().random_ratio(9, 10) {
                let lamports = if rng().random_ratio(1, 5) {
                    rng().random_range(0..10)
                } else {
                    rng().random_range(20..100)
                };
                let space = rng().random_range(0..10);
                let owner = Pubkey::default();
                let account = AccountSharedData::new(lamports, space, &owner);
                bank.store_account(&key, &account);
                lamports
            } else {
                0
            };
            (key, balance)
        })
        .collect();
    let mut results = HashMap::new();
    for _ in 0..2_000 {
        let num_keys = if rng().random_ratio(1, 5) {
            rng().random_range(0..max_keys)
        } else {
            rng().random_range(1..4)
        };
        let num_instructions = rng().random_range(0..max_keys - num_keys);

        let mut account_keys: Vec<_> = if rng().random_ratio(1, 5) {
            (0..num_keys)
                .map(|_| {
                    let idx = rng().random_range(0..keys.len());
                    keys[idx].0
                })
                .collect()
        } else {
            let mut inserted = HashSet::new();
            (0..num_keys)
                .map(|_| {
                    let mut idx;
                    loop {
                        idx = rng().random_range(0..keys.len());
                        if !inserted.contains(&idx) {
                            break;
                        }
                    }
                    inserted.insert(idx);
                    keys[idx].0
                })
                .collect()
        };

        let instructions: Vec<_> = if num_keys > 0 {
            (0..num_instructions)
                .map(|_| {
                    let num_accounts_to_pass = rng().random_range(0..num_keys);
                    let account_indexes = (0..num_accounts_to_pass)
                        .map(|_| rng().random_range(0..num_keys))
                        .collect();
                    let program_index: u8 = rng().random_range(0..num_keys);
                    if rng().random_ratio(4, 5) {
                        let programs_index = rng().random_range(0..program_keys.len());
                        account_keys[program_index as usize] = program_keys[programs_index].0;
                    }
                    CompiledInstruction::new(program_index, &10, account_indexes)
                })
                .collect()
        } else {
            vec![]
        };

        let account_keys_len = std::cmp::max(account_keys.len(), 2);
        let num_signatures = if rng().random_ratio(1, 5) {
            rng().random_range(0..account_keys_len + 10)
        } else {
            rng().random_range(1..account_keys_len)
        };

        let num_required_signatures = if rng().random_ratio(1, 5) {
            rng().random_range(0..account_keys_len + 10) as u8
        } else {
            rng().random_range(1..std::cmp::max(2, num_signatures)) as u8
        };
        let num_readonly_signed_accounts = if rng().random_ratio(1, 5) {
            rng().random_range(0..account_keys_len) as u8
        } else {
            let max = if num_required_signatures > 1 {
                num_required_signatures - 1
            } else {
                1
            };
            rng().random_range(0..max)
        };

        let num_readonly_unsigned_accounts =
            if rng().random_ratio(1, 5) || (num_required_signatures as usize) >= account_keys_len {
                rng().random_range(0..account_keys_len) as u8
            } else {
                rng().random_range(0..account_keys_len - num_required_signatures as usize) as u8
            };

        let header = MessageHeader {
            num_required_signatures,
            num_readonly_signed_accounts,
            num_readonly_unsigned_accounts,
        };
        let message = Message {
            header,
            account_keys,
            recent_blockhash: bank.last_blockhash(),
            instructions,
        };

        let tx = Transaction {
            signatures: vec![Signature::default(); num_signatures],
            message,
        };

        let result = bank.process_transaction(&tx);
        for (key, balance) in &keys {
            assert_eq!(bank.get_balance(key), *balance);
        }
        for (key, name) in &program_keys {
            let account = bank.get_account(key).unwrap();
            assert!(account.executable());
            assert_eq!(account.data(), name);
        }
        info!("result: {result:?}");
        let result_key = format!("{result:?}");
        *results.entry(result_key).or_insert(0) += 1;
    }
    info!("results: {results:?}");
}

// DEVELOPERS: This test is intended to ensure that the bank hash remains
// consistent across all changes, including feature set changes. If you add a
// new feature that affects the bank hash, you should update this test to use a
// test matrix that tests the bank hash calculation with and without your
// added feature.
#[test]
fn test_bank_hash_consistency() {
    let genesis_config = GenesisConfig {
        // Override the creation time to ensure bank hash consistency
        creation_time: 0,
        accounts: BTreeMap::from([(
            Pubkey::from([42; 32]),
            Account::new(1_000_000_000_000, 0, &system_program::id()),
        )]),
        cluster_type: ClusterType::MainnetBeta,
        ..GenesisConfig::default()
    };

    // Set the feature set to all enabled so that we detect any inconsistencies
    // in the hash computation that may arise from feature set changes
    let feature_set = FeatureSet::all_enabled();

    let mut bank = Arc::new(Bank::new_from_genesis(
        &genesis_config,
        Arc::new(RuntimeConfig::default()),
        vec![],
        None,
        BankTestConfig::default().accounts_db_config,
        None,
        Some(Pubkey::from([42; 32])),
        Arc::default(),
        None,
        Some(feature_set),
    ));
    loop {
        goto_end_of_slot(Arc::clone(&bank));
        if bank.slot == 0 {
            assert_eq!(bank.epoch(), 0);
            assert_eq!(
                bank.hash().to_string(),
                "EzyLJJki4ALhQAq5wbmiNctDhytQckGJRXnk9APKXv7r",
            );
        }

        if bank.slot == 32 {
            assert_eq!(bank.epoch(), 1);
            assert_eq!(
                bank.hash().to_string(),
                "6h1KzSuTW6MwkgjtEbrv6AyUZ2NHtSxCQi8epjHDFYh8"
            );
        }
        if bank.slot == 128 {
            assert_eq!(bank.epoch(), 2);
            assert_eq!(
                bank.hash().to_string(),
                "4GX3883TVK7SQfbPUHem4HXcqdHU2DZVAB6yEXspn2qe"
            );
            break;
        }
        bank = Arc::new(new_from_parent(bank));
    }
}

#[ignore]
#[test]
fn test_same_program_id_uses_unique_executable_accounts() {
    declare_process_instruction!(MockBuiltin, 1, |invoke_context| {
        let transaction_context = &invoke_context.transaction_context;
        let instruction_context = transaction_context.get_current_instruction_context()?;
        let program_idx = instruction_context.get_index_of_program_account_in_transaction()?;
        let mut acc = transaction_context.accounts().try_borrow_mut(program_idx)?;
        acc.set_data_from_slice(&[1, 2]);
        Ok(())
    });

    let (genesis_config, mint_keypair) = create_genesis_config(50000);
    let program1_pubkey = solana_pubkey::new_rand();
    let (bank, _bank_forks) =
        Bank::new_with_mockup_builtin_for_tests(&genesis_config, program1_pubkey, MockBuiltin::vm);

    // Add a new program owned by the first
    let program2_pubkey = solana_pubkey::new_rand();
    let mut program2_account = AccountSharedData::new(1, 1, &program1_pubkey);
    program2_account.set_executable(true);
    bank.store_account(&program2_pubkey, &program2_account);

    let instruction = Instruction::new_with_bincode(program2_pubkey, &10, vec![]);
    let tx = Transaction::new_signed_with_payer(
        &[instruction.clone(), instruction],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair],
        bank.last_blockhash(),
    );
    assert!(bank.process_transaction(&tx).is_ok());
    assert_eq!(6, bank.get_account(&program1_pubkey).unwrap().data().len());
    assert_eq!(1, bank.get_account(&program2_pubkey).unwrap().data().len());
}

#[test]
fn test_clean_nonrooted() {
    agave_logger::setup();

    let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000_000);
    let pubkey0 = Pubkey::from([0; 32]);
    let pubkey1 = Pubkey::from([1; 32]);

    info!("pubkey0: {pubkey0}");
    info!("pubkey1: {pubkey1}");

    // Set root for bank 0, with caching enabled
    let bank0 = Arc::new(Bank::new_with_config_for_tests(
        &genesis_config,
        BankTestConfig::default(),
    ));

    let account_zero = AccountSharedData::new(0, 0, &Pubkey::new_unique());

    goto_end_of_slot(bank0.clone());
    bank0.freeze();
    bank0.squash();
    // Flush now so that accounts cache cleaning doesn't clean up bank 0 when later
    // slots add updates to the cache
    bank0.force_flush_accounts_cache();

    // Store some lamports in bank 1
    let some_lamports = 123;
    let bank1 = Arc::new(Bank::new_from_parent(bank0.clone(), &Pubkey::default(), 1));
    test_utils::deposit(&bank1, &pubkey0, some_lamports).unwrap();
    goto_end_of_slot(bank1.clone());
    bank1.freeze();
    bank1.flush_accounts_cache_slot_for_tests();

    bank1.print_accounts_stats();

    // Store some lamports for pubkey1 in bank 2, root bank 2
    // bank2's parent is bank0
    let bank2 = Arc::new(Bank::new_from_parent(bank0, &Pubkey::default(), 2));
    test_utils::deposit(&bank2, &pubkey1, some_lamports).unwrap();
    bank2.store_account(&pubkey0, &account_zero);
    goto_end_of_slot(bank2.clone());
    bank2.freeze();
    bank2.squash();
    bank2.force_flush_accounts_cache();

    bank2.print_accounts_stats();
    drop(bank1);

    // Clean accounts, which should add earlier slots to the shrink
    // candidate set
    bank2.clean_accounts_for_tests();

    let bank3 = Arc::new(Bank::new_from_parent(bank2, &Pubkey::default(), 3));
    test_utils::deposit(&bank3, &pubkey1, some_lamports + 1).unwrap();
    goto_end_of_slot(bank3.clone());
    bank3.freeze();
    bank3.squash();
    bank3.force_flush_accounts_cache();

    bank3.clean_accounts_for_tests();
    bank3.rc.accounts.accounts_db.assert_ref_count(&pubkey0, 2);
    assert!(bank3
        .rc
        .accounts
        .accounts_db
        .storage
        .get_slot_storage_entry(1)
        .is_none());

    bank3.print_accounts_stats();
}

#[test]
fn test_shrink_candidate_slots_cached() {
    agave_logger::setup();

    let (genesis_config, _mint_keypair) = create_genesis_config(1_000_000_000);
    let pubkey0 = solana_pubkey::new_rand();
    let pubkey1 = solana_pubkey::new_rand();
    let pubkey2 = solana_pubkey::new_rand();

    // Set root for bank 0, with caching enabled
    let bank0 = Arc::new(Bank::new_with_config_for_tests(
        &genesis_config,
        BankTestConfig::default(),
    ));

    // Make pubkey0 large so any slot containing it is a candidate for shrinking
    let pubkey0_size = 100_000;

    let account0 = AccountSharedData::new(1000, pubkey0_size, &Pubkey::new_unique());
    bank0.store_account(&pubkey0, &account0);

    goto_end_of_slot(bank0.clone());
    bank0.freeze();
    bank0.squash();
    // Flush now so that accounts cache cleaning doesn't clean up bank 0 when later
    // slots add updates to the cache
    bank0.force_flush_accounts_cache();

    // Store some lamports in bank 1
    let some_lamports = 123;
    let bank1 = Arc::new(new_from_parent(bank0));
    test_utils::deposit(&bank1, &pubkey1, some_lamports).unwrap();
    test_utils::deposit(&bank1, &pubkey2, some_lamports).unwrap();
    goto_end_of_slot(bank1.clone());
    bank1.freeze();
    bank1.squash();
    // Flush now so that accounts cache cleaning doesn't clean up bank 0 when later
    // slots add updates to the cache
    bank1.force_flush_accounts_cache();

    // Store some lamports for pubkey1 in bank 2, root bank 2
    let bank2 = Arc::new(new_from_parent(bank1));
    test_utils::deposit(&bank2, &pubkey1, some_lamports).unwrap();
    bank2.store_account(&pubkey0, &account0);
    goto_end_of_slot(bank2.clone());
    bank2.freeze();
    bank2.squash();
    bank2.force_flush_accounts_cache();

    // Clean accounts, which should add earlier slots to the shrink
    // candidate set
    bank2.clean_accounts_for_tests();

    // Slots 0 and 1 should be candidates for shrinking, but slot 2
    // shouldn't because none of its accounts are outdated by a later
    // root
    assert_eq!(bank2.shrink_candidate_slots(), 2);
    let alive_counts: Vec<usize> = (0..3)
        .map(|slot| {
            bank2
                .rc
                .accounts
                .accounts_db
                .alive_account_count_in_slot(slot)
        })
        .collect();

    // No more slots should be shrunk
    assert_eq!(bank2.shrink_candidate_slots(), 0);
    // alive_counts represents the count of alive accounts in the three slots 0,1,2
    assert_eq!(alive_counts, vec![12, 1, 6]);
}

#[test]
fn test_add_builtin_no_overwrite() {
    let slot = 123;
    let program_id = solana_pubkey::new_rand();

    let (parent_bank, _bank_forks) = create_simple_test_arc_bank(100_000);
    let mut bank = Arc::new(Bank::new_from_parent(parent_bank, &Pubkey::default(), slot));
    assert_eq!(bank.get_account_modified_slot(&program_id), None);

    Arc::get_mut(&mut bank)
        .unwrap()
        .add_mockup_builtin(program_id, MockBuiltin::vm);
    assert_eq!(bank.get_account_modified_slot(&program_id).unwrap().1, slot);

    let mut bank = Arc::new(new_from_parent(bank));
    Arc::get_mut(&mut bank)
        .unwrap()
        .add_mockup_builtin(program_id, MockBuiltin::vm);
    assert_eq!(bank.get_account_modified_slot(&program_id).unwrap().1, slot);
}

#[test]
fn test_add_builtin_loader_no_overwrite() {
    let slot = 123;
    let loader_id = solana_pubkey::new_rand();

    let (parent_bank, _bank_forks) = create_simple_test_arc_bank(100_000);
    let mut bank = Arc::new(Bank::new_from_parent(parent_bank, &Pubkey::default(), slot));
    assert_eq!(bank.get_account_modified_slot(&loader_id), None);

    Arc::get_mut(&mut bank)
        .unwrap()
        .add_mockup_builtin(loader_id, MockBuiltin::vm);
    assert_eq!(bank.get_account_modified_slot(&loader_id).unwrap().1, slot);

    let mut bank = Arc::new(new_from_parent(bank));
    Arc::get_mut(&mut bank)
        .unwrap()
        .add_mockup_builtin(loader_id, MockBuiltin::vm);
    assert_eq!(bank.get_account_modified_slot(&loader_id).unwrap().1, slot);
}

#[test]
fn test_add_builtin_account() {
    for pass in 0..5 {
        let (mut genesis_config, _mint_keypair) = create_genesis_config(100_000);
        activate_all_features(&mut genesis_config);

        let slot = 123;
        // The account at program_id will be created initially with just 1 lamport.
        let program_id = Pubkey::new_from_array([0xFF; 32]);

        let bank = Arc::new(Bank::new_from_parent(
            Arc::new(Bank::new_for_tests(&genesis_config)),
            &Pubkey::default(),
            slot,
        ));
        add_root_and_flush_write_cache(&bank.parent().unwrap());
        assert_eq!(bank.get_account_modified_slot(&program_id), None);

        assert_capitalization_diff(
            &bank,
            || bank.add_builtin_account("mock_program", &program_id),
            |old, new| {
                assert_eq!(old + 1, new);
                pass == 0
            },
        );
        if pass == 0 {
            continue;
        }

        assert_eq!(bank.get_account_modified_slot(&program_id).unwrap().1, slot);

        let bank = Arc::new(new_from_parent(bank));
        add_root_and_flush_write_cache(&bank.parent().unwrap());
        assert_capitalization_diff(
            &bank,
            || bank.add_builtin_account("mock_program", &program_id),
            |old, new| {
                assert_eq!(old, new);
                pass == 1
            },
        );
        if pass == 1 {
            continue;
        }

        assert_eq!(bank.get_account_modified_slot(&program_id).unwrap().1, slot);

        let bank = Arc::new(new_from_parent(bank));
        add_root_and_flush_write_cache(&bank.parent().unwrap());
        // No builtin replacement should happen if the program id is already assigned to a
        // builtin.
        assert_capitalization_diff(
            &bank,
            || bank.add_builtin_account("mock_program v2", &program_id),
            |old, new| {
                assert_eq!(old, new);
                pass == 2
            },
        );
        if pass == 2 {
            continue;
        }

        // No replacement should have happened
        assert_eq!(bank.get_account_modified_slot(&program_id).unwrap().1, slot);
    }
}

/// useful to adapt tests written prior to introduction of the write cache
/// to use the write cache
fn add_root_and_flush_write_cache(bank: &Bank) {
    bank.rc.accounts.add_root(bank.slot());
    bank.flush_accounts_cache_slot_for_tests()
}

#[test]
fn test_add_builtin_account_inherited_cap_while_replacing() {
    for pass in 0..4 {
        let (genesis_config, mint_keypair) = create_genesis_config(100_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let program_id = solana_pubkey::new_rand();

        bank.add_builtin_account("mock_program", &program_id);
        if pass == 0 {
            add_root_and_flush_write_cache(&bank);
            assert_eq!(
                bank.capitalization(),
                bank.calculate_capitalization_for_tests()
            );
            continue;
        }

        // someone mess with program_id's balance
        bank.withdraw(&mint_keypair.pubkey(), 10).unwrap();
        if pass == 1 {
            add_root_and_flush_write_cache(&bank);
            assert_ne!(
                bank.capitalization(),
                bank.calculate_capitalization_for_tests()
            );
            continue;
        }
        test_utils::deposit(&bank, &program_id, 10).unwrap();
        if pass == 2 {
            add_root_and_flush_write_cache(&bank);
            assert_eq!(
                bank.capitalization(),
                bank.calculate_capitalization_for_tests()
            );
            continue;
        }

        bank.add_builtin_account("mock_program v2", &program_id);
        add_root_and_flush_write_cache(&bank);
        assert_eq!(
            bank.capitalization(),
            bank.calculate_capitalization_for_tests()
        );
    }
}

#[test]
fn test_add_builtin_account_squatted_while_not_replacing() {
    for pass in 0..3 {
        let (genesis_config, mint_keypair) = create_genesis_config(100_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let program_id = solana_pubkey::new_rand();

        // someone managed to squat at program_id!
        bank.withdraw(&mint_keypair.pubkey(), 10).unwrap();
        if pass == 0 {
            add_root_and_flush_write_cache(&bank);
            assert_ne!(
                bank.capitalization(),
                bank.calculate_capitalization_for_tests()
            );
            continue;
        }
        test_utils::deposit(&bank, &program_id, 10).unwrap();
        if pass == 1 {
            add_root_and_flush_write_cache(&bank);
            assert_eq!(
                bank.capitalization(),
                bank.calculate_capitalization_for_tests()
            );
            continue;
        }

        bank.add_builtin_account("mock_program", &program_id);
        add_root_and_flush_write_cache(&bank);
        assert_eq!(
            bank.capitalization(),
            bank.calculate_capitalization_for_tests()
        );
    }
}

#[test]
#[should_panic(
    expected = "Can't change frozen bank by adding not-existing new builtin program \
                (mock_program, CiXgo2KHKSDmDnV1F6B69eWFgNAPiSBjjYvfB4cvRNre). Maybe, inconsistent \
                program activation is detected on snapshot restore?"
)]
fn test_add_builtin_account_after_frozen() {
    let slot = 123;
    let program_id = Pubkey::from_str("CiXgo2KHKSDmDnV1F6B69eWFgNAPiSBjjYvfB4cvRNre").unwrap();

    let (parent_bank, _bank_forks) = create_simple_test_arc_bank(100_000);
    let bank = Bank::new_from_parent(parent_bank, &Pubkey::default(), slot);
    bank.freeze();

    bank.add_builtin_account("mock_program", &program_id);
}

#[test]
fn test_add_precompiled_account() {
    for pass in 0..2 {
        let (mut genesis_config, _mint_keypair) = create_genesis_config(100_000);
        activate_all_features(&mut genesis_config);

        let slot = 123;
        let program_id = solana_pubkey::new_rand();

        let bank = Arc::new(Bank::new_from_parent(
            Arc::new(Bank::new_for_tests(&genesis_config)),
            &Pubkey::default(),
            slot,
        ));
        add_root_and_flush_write_cache(&bank.parent().unwrap());
        assert_eq!(bank.get_account_modified_slot(&program_id), None);

        assert_capitalization_diff(
            &bank,
            || bank.add_precompiled_account(&program_id),
            |old, new| {
                assert_eq!(old + 1, new);
                pass == 0
            },
        );
        if pass == 0 {
            continue;
        }

        assert_eq!(bank.get_account_modified_slot(&program_id).unwrap().1, slot);

        let bank = Arc::new(new_from_parent(bank));
        add_root_and_flush_write_cache(&bank.parent().unwrap());
        assert_capitalization_diff(
            &bank,
            || bank.add_precompiled_account(&program_id),
            |old, new| {
                assert_eq!(old, new);
                true
            },
        );

        assert_eq!(bank.get_account_modified_slot(&program_id).unwrap().1, slot);
    }
}

#[test]
fn test_add_precompiled_account_inherited_cap_while_replacing() {
    // when we flush the cache, it has side effects, so we have to restart the test each time we flush the cache
    // and then want to continue modifying the bank
    for pass in 0..4 {
        let (genesis_config, mint_keypair) = create_genesis_config(100_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let program_id = solana_pubkey::new_rand();

        bank.add_precompiled_account(&program_id);
        if pass == 0 {
            add_root_and_flush_write_cache(&bank);
            assert_eq!(
                bank.capitalization(),
                bank.calculate_capitalization_for_tests()
            );
            continue;
        }

        // someone mess with program_id's balance
        bank.withdraw(&mint_keypair.pubkey(), 10).unwrap();
        if pass == 1 {
            add_root_and_flush_write_cache(&bank);
            assert_ne!(
                bank.capitalization(),
                bank.calculate_capitalization_for_tests()
            );
            continue;
        }
        test_utils::deposit(&bank, &program_id, 10).unwrap();
        if pass == 2 {
            add_root_and_flush_write_cache(&bank);
            assert_eq!(
                bank.capitalization(),
                bank.calculate_capitalization_for_tests()
            );
            continue;
        }

        bank.add_precompiled_account(&program_id);
        add_root_and_flush_write_cache(&bank);
        assert_eq!(
            bank.capitalization(),
            bank.calculate_capitalization_for_tests()
        );
    }
}

#[test]
fn test_add_precompiled_account_squatted_while_not_replacing() {
    for pass in 0..3 {
        let (genesis_config, mint_keypair) = create_genesis_config(100_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let program_id = solana_pubkey::new_rand();

        // someone managed to squat at program_id!
        bank.withdraw(&mint_keypair.pubkey(), 10).unwrap();
        if pass == 0 {
            add_root_and_flush_write_cache(&bank);

            assert_ne!(
                bank.capitalization(),
                bank.calculate_capitalization_for_tests()
            );
            continue;
        }
        test_utils::deposit(&bank, &program_id, 10).unwrap();
        if pass == 1 {
            add_root_and_flush_write_cache(&bank);
            assert_eq!(
                bank.capitalization(),
                bank.calculate_capitalization_for_tests()
            );
            continue;
        }

        bank.add_precompiled_account(&program_id);
        add_root_and_flush_write_cache(&bank);

        assert_eq!(
            bank.capitalization(),
            bank.calculate_capitalization_for_tests()
        );
    }
}

#[test]
#[should_panic(
    expected = "Can't change frozen bank by adding not-existing new precompiled program \
                (CiXgo2KHKSDmDnV1F6B69eWFgNAPiSBjjYvfB4cvRNre). Maybe, inconsistent program \
                activation is detected on snapshot restore?"
)]
fn test_add_precompiled_account_after_frozen() {
    let slot = 123;
    let program_id = Pubkey::from_str("CiXgo2KHKSDmDnV1F6B69eWFgNAPiSBjjYvfB4cvRNre").unwrap();

    let (parent_bank, _bank_forks) = create_simple_test_arc_bank(100_000);
    let bank = Bank::new_from_parent(parent_bank, &Pubkey::default(), slot);
    bank.freeze();

    bank.add_precompiled_account(&program_id);
}

#[test]
fn test_reconfigure_token2_native_mint() {
    agave_logger::setup();

    let genesis_config =
        create_genesis_config_with_leader(5, &solana_pubkey::new_rand(), 0).genesis_config;
    let bank = Arc::new(Bank::new_for_tests(&genesis_config));
    assert_eq!(bank.get_balance(&token::native_mint::id()), 1000000000);
    let native_mint_account = bank.get_account(&token::native_mint::id()).unwrap();
    assert_eq!(native_mint_account.data().len(), 82);
    assert_eq!(native_mint_account.owner(), &token::id());
}

#[test]
fn test_bank_load_program() {
    agave_logger::setup();

    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee(1_000_000_000);
    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), 42);
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), 50);

    let program_key = solana_pubkey::new_rand();
    let programdata_key = solana_pubkey::new_rand();

    let mut file = File::open("../programs/bpf_loader/test_elfs/out/noop_aligned.so").unwrap();
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    let mut program_account = AccountSharedData::new_data(
        40,
        &UpgradeableLoaderState::Program {
            programdata_address: programdata_key,
        },
        &bpf_loader_upgradeable::id(),
    )
    .unwrap();
    program_account.set_executable(true);
    program_account.set_rent_epoch(1);
    let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
    let mut programdata_account = AccountSharedData::new(
        40,
        programdata_data_offset + elf.len(),
        &bpf_loader_upgradeable::id(),
    );
    programdata_account
        .set_state(&UpgradeableLoaderState::ProgramData {
            slot: 42,
            upgrade_authority_address: None,
        })
        .unwrap();
    programdata_account.data_as_mut_slice()[programdata_data_offset..].copy_from_slice(&elf);
    programdata_account.set_rent_epoch(1);
    bank.store_account_and_update_capitalization(&program_key, &program_account);
    bank.store_account_and_update_capitalization(&programdata_key, &programdata_account);

    let instruction = Instruction::new_with_bytes(program_key, &[], Vec::new());
    let invocation_message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let binding = mint_keypair.insecure_clone();
    let transaction = Transaction::new(
        &[&binding],
        invocation_message.clone(),
        bank.last_blockhash(),
    );
    assert!(bank.process_transaction(&transaction).is_ok());

    {
        let program_cache = bank
            .transaction_processor
            .global_program_cache
            .read()
            .unwrap();
        let [program] = program_cache.get_slot_versions_for_tests(&program_key) else {
            panic!();
        };
        assert_matches!(program.program, ProgramCacheEntryType::Loaded(_));
        assert_eq!(
            program.account_size,
            program_account.data().len() + programdata_account.data().len()
        );
    }
}

#[allow(deprecated)]
#[test]
fn test_bpf_loader_upgradeable_deploy_with_max_len() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee(1_000_000_000);
    let mut bank = Bank::new_for_tests(&genesis_config);
    bank.feature_set = Arc::new(FeatureSet::all_enabled());
    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let mut bank_client = BankClient::new_shared(bank.clone());

    // Setup keypairs and addresses
    let payer_keypair = Keypair::new();
    let program_keypair = Keypair::new();
    let buffer_address = Pubkey::new_unique();
    let (programdata_address, _) = Pubkey::find_program_address(
        &[program_keypair.pubkey().as_ref()],
        &bpf_loader_upgradeable::id(),
    );
    let upgrade_authority_keypair = Keypair::new();

    // Test nonexistent program invocation
    let instruction = Instruction::new_with_bytes(program_keypair.pubkey(), &[], Vec::new());
    let invocation_message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let binding = mint_keypair.insecure_clone();
    let transaction = Transaction::new(
        &[&binding],
        invocation_message.clone(),
        bank.last_blockhash(),
    );
    assert_eq!(
        bank.process_transaction(&transaction),
        Err(TransactionError::ProgramAccountNotFound),
    );
    {
        // Make sure it is not in the cache because the account owner is not a loader
        let program_cache = bank
            .transaction_processor
            .global_program_cache
            .read()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert!(slot_versions.is_empty());
    }

    // Advance bank to get a new last blockhash so that when we retry invocation
    // after creating the program, the new transaction created below with the
    // same `invocation_message` as above doesn't return `AlreadyProcessed` when
    // processed.
    goto_end_of_slot(bank.clone());
    let bank = bank_client
        .advance_slot(1, bank_forks.as_ref(), &mint_keypair.pubkey())
        .unwrap();

    // Load program file
    let mut file = File::open("../programs/bpf_loader/test_elfs/out/noop_aligned.so")
        .expect("file open failed");
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();

    // Compute rent exempt balances
    let program_len = elf.len();
    let min_program_balance =
        bank.get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program());
    let min_buffer_balance = bank.get_minimum_balance_for_rent_exemption(
        UpgradeableLoaderState::size_of_buffer(program_len),
    );
    let min_programdata_balance = bank.get_minimum_balance_for_rent_exemption(
        UpgradeableLoaderState::size_of_programdata(program_len),
    );

    // Setup accounts
    let buffer_account = {
        let mut account = AccountSharedData::new(
            min_buffer_balance,
            UpgradeableLoaderState::size_of_buffer(elf.len()),
            &bpf_loader_upgradeable::id(),
        );
        account
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(upgrade_authority_keypair.pubkey()),
            })
            .unwrap();
        account
            .data_as_mut_slice()
            .get_mut(UpgradeableLoaderState::size_of_buffer_metadata()..)
            .unwrap()
            .copy_from_slice(&elf);
        account
    };
    let program_account = AccountSharedData::new(
        min_programdata_balance,
        UpgradeableLoaderState::size_of_program(),
        &bpf_loader_upgradeable::id(),
    );
    let programdata_account = AccountSharedData::new(
        1,
        UpgradeableLoaderState::size_of_programdata(elf.len()),
        &bpf_loader_upgradeable::id(),
    );

    // Test uninitialized program invocation
    bank.store_account(&program_keypair.pubkey(), &program_account);
    let transaction = Transaction::new(
        &[&binding],
        invocation_message.clone(),
        bank.last_blockhash(),
    );
    assert_eq!(
        bank.process_transaction(&transaction),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::UnsupportedProgramId
        )),
    );
    {
        let program_cache = bank
            .transaction_processor
            .global_program_cache
            .read()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert_eq!(slot_versions.len(), 1);
        assert_eq!(slot_versions[0].deployment_slot, bank.slot());
        assert_eq!(slot_versions[0].effective_slot, bank.slot());
        assert!(matches!(
            slot_versions[0].program,
            ProgramCacheEntryType::Closed,
        ));
    }

    // Test buffer invocation
    bank.store_account(&buffer_address, &buffer_account);
    let instruction = Instruction::new_with_bytes(buffer_address, &[], Vec::new());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let transaction = Transaction::new(&[&binding], message, bank.last_blockhash());
    assert_eq!(
        bank.process_transaction(&transaction),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::UnsupportedProgramId,
        )),
    );
    {
        let program_cache = bank
            .transaction_processor
            .global_program_cache
            .read()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&buffer_address);
        assert_eq!(slot_versions.len(), 1);
        assert_eq!(slot_versions[0].deployment_slot, bank.slot());
        assert_eq!(slot_versions[0].effective_slot, bank.slot());
        assert!(matches!(
            slot_versions[0].program,
            ProgramCacheEntryType::Closed,
        ));
    }

    // Test successful deploy
    let payer_base_balance = LAMPORTS_PER_SOL;
    let deploy_fees = {
        let fee_calculator = genesis_config.fee_rate_governor.create_fee_calculator();
        3 * fee_calculator.lamports_per_signature
    };
    let min_payer_balance = min_program_balance
        .saturating_add(min_programdata_balance)
        .saturating_sub(min_buffer_balance.saturating_add(deploy_fees));
    bank.store_account(
        &payer_keypair.pubkey(),
        &AccountSharedData::new(
            payer_base_balance.saturating_add(min_payer_balance),
            0,
            &system_program::id(),
        ),
    );
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    let message = Message::new(
        &solana_loader_v3_interface::instruction::deploy_with_max_program_len(
            &payer_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len(),
        )
        .unwrap(),
        Some(&payer_keypair.pubkey()),
    );
    assert!(bank_client
        .send_and_confirm_message(
            &[&payer_keypair, &program_keypair, &upgrade_authority_keypair],
            message
        )
        .is_ok());
    assert_eq!(
        bank.get_balance(&payer_keypair.pubkey()),
        payer_base_balance
    );
    assert_eq!(bank.get_balance(&buffer_address), 0);
    assert_eq!(None, bank.get_account(&buffer_address));
    let post_program_account = bank.get_account(&program_keypair.pubkey()).unwrap();
    assert_eq!(post_program_account.lamports(), min_program_balance);
    assert_eq!(post_program_account.owner(), &bpf_loader_upgradeable::id());
    assert_eq!(
        post_program_account.data().len(),
        UpgradeableLoaderState::size_of_program()
    );
    let state: UpgradeableLoaderState = post_program_account.state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::Program {
            programdata_address
        }
    );
    let post_programdata_account = bank.get_account(&programdata_address).unwrap();
    assert_eq!(post_programdata_account.lamports(), min_programdata_balance);
    assert_eq!(
        post_programdata_account.owner(),
        &bpf_loader_upgradeable::id()
    );
    let state: UpgradeableLoaderState = post_programdata_account.state().unwrap();
    assert_eq!(
        state,
        UpgradeableLoaderState::ProgramData {
            slot: bank_client.get_slot().unwrap(),
            upgrade_authority_address: Some(upgrade_authority_keypair.pubkey())
        }
    );
    for (i, byte) in post_programdata_account
        .data()
        .get(UpgradeableLoaderState::size_of_programdata_metadata()..)
        .unwrap()
        .iter()
        .enumerate()
    {
        assert_eq!(*elf.get(i).unwrap(), *byte);
    }

    // Advance the bank so that the program becomes effective
    goto_end_of_slot(bank.clone());
    let bank = bank_client
        .advance_slot(1, bank_forks.as_ref(), &mint_keypair.pubkey())
        .unwrap();

    // Invoke the deployed program
    let transaction = Transaction::new(&[&binding], invocation_message, bank.last_blockhash());
    assert!(bank.process_transaction(&transaction).is_ok());
    {
        let program_cache = bank
            .transaction_processor
            .global_program_cache
            .read()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert_eq!(slot_versions.len(), 1);
        assert_eq!(slot_versions[0].deployment_slot, bank.slot() - 1);
        assert_eq!(slot_versions[0].effective_slot, bank.slot());
        assert!(matches!(
            slot_versions[0].program,
            ProgramCacheEntryType::Loaded(_),
        ));
    }

    // Test initialized program account
    bank.clear_signatures();
    bank.store_account(&buffer_address, &buffer_account);
    let bank = bank_client
        .advance_slot(1, bank_forks.as_ref(), &mint_keypair.pubkey())
        .unwrap();
    let message = Message::new(
        &[Instruction::new_with_bincode(
            bpf_loader_upgradeable::id(),
            &UpgradeableLoaderInstruction::DeployWithMaxDataLen {
                max_data_len: elf.len(),
            },
            vec![
                AccountMeta::new(mint_keypair.pubkey(), true),
                AccountMeta::new(programdata_address, false),
                AccountMeta::new(program_keypair.pubkey(), false),
                AccountMeta::new(buffer_address, false),
                AccountMeta::new_readonly(sysvar::rent::id(), false),
                AccountMeta::new_readonly(sysvar::clock::id(), false),
                AccountMeta::new_readonly(system_program::id(), false),
                AccountMeta::new_readonly(upgrade_authority_keypair.pubkey(), true),
            ],
        )],
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        TransactionError::InstructionError(0, InstructionError::AccountAlreadyInitialized),
        bank_client
            .send_and_confirm_message(&[&mint_keypair, &upgrade_authority_keypair], message)
            .unwrap_err()
            .unwrap()
    );

    // Test initialized ProgramData account
    bank.clear_signatures();
    bank.store_account(&buffer_address, &buffer_account);
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    let message = Message::new(
        &solana_loader_v3_interface::instruction::deploy_with_max_program_len(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len(),
        )
        .unwrap(),
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        TransactionError::InstructionError(1, InstructionError::Custom(0)),
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .unwrap_err()
            .unwrap()
    );

    // Test deploy no authority
    bank.clear_signatures();
    bank.store_account(&buffer_address, &buffer_account);
    bank.store_account(&program_keypair.pubkey(), &program_account);
    bank.store_account(&programdata_address, &programdata_account);
    let message = Message::new(
        &[Instruction::new_with_bincode(
            bpf_loader_upgradeable::id(),
            &UpgradeableLoaderInstruction::DeployWithMaxDataLen {
                max_data_len: elf.len(),
            },
            vec![
                AccountMeta::new(mint_keypair.pubkey(), true),
                AccountMeta::new(programdata_address, false),
                AccountMeta::new(program_keypair.pubkey(), false),
                AccountMeta::new(buffer_address, false),
                AccountMeta::new_readonly(sysvar::rent::id(), false),
                AccountMeta::new_readonly(sysvar::clock::id(), false),
                AccountMeta::new_readonly(system_program::id(), false),
            ],
        )],
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        TransactionError::InstructionError(0, InstructionError::MissingAccount),
        bank_client
            .send_and_confirm_message(&[&mint_keypair], message)
            .unwrap_err()
            .unwrap()
    );

    // Test deploy authority not a signer
    bank.clear_signatures();
    bank.store_account(&buffer_address, &buffer_account);
    bank.store_account(&program_keypair.pubkey(), &program_account);
    bank.store_account(&programdata_address, &programdata_account);
    let message = Message::new(
        &[Instruction::new_with_bincode(
            bpf_loader_upgradeable::id(),
            &UpgradeableLoaderInstruction::DeployWithMaxDataLen {
                max_data_len: elf.len(),
            },
            vec![
                AccountMeta::new(mint_keypair.pubkey(), true),
                AccountMeta::new(programdata_address, false),
                AccountMeta::new(program_keypair.pubkey(), false),
                AccountMeta::new(buffer_address, false),
                AccountMeta::new_readonly(sysvar::rent::id(), false),
                AccountMeta::new_readonly(sysvar::clock::id(), false),
                AccountMeta::new_readonly(system_program::id(), false),
                AccountMeta::new_readonly(upgrade_authority_keypair.pubkey(), false),
            ],
        )],
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature),
        bank_client
            .send_and_confirm_message(&[&mint_keypair], message)
            .unwrap_err()
            .unwrap()
    );

    // Test invalid Buffer account state
    bank.clear_signatures();
    bank.store_account(&buffer_address, &AccountSharedData::default());
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    let message = Message::new(
        &solana_loader_v3_interface::instruction::deploy_with_max_program_len(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len(),
        )
        .unwrap(),
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        TransactionError::InstructionError(1, InstructionError::InvalidAccountData),
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .unwrap_err()
            .unwrap()
    );

    // Test program account not rent exempt
    bank.clear_signatures();
    bank.store_account(&buffer_address, &buffer_account);
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    let message = Message::new(
        &solana_loader_v3_interface::instruction::deploy_with_max_program_len(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance.saturating_sub(1),
            elf.len(),
        )
        .unwrap(),
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        TransactionError::InstructionError(1, InstructionError::ExecutableAccountNotRentExempt),
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .unwrap_err()
            .unwrap()
    );

    // Test program account not rent exempt because data is larger than needed
    bank.clear_signatures();
    bank.store_account(&buffer_address, &buffer_account);
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    let mut instructions = solana_loader_v3_interface::instruction::deploy_with_max_program_len(
        &mint_keypair.pubkey(),
        &program_keypair.pubkey(),
        &buffer_address,
        &upgrade_authority_keypair.pubkey(),
        min_program_balance,
        elf.len(),
    )
    .unwrap();
    *instructions.get_mut(0).unwrap() = system_instruction::create_account(
        &mint_keypair.pubkey(),
        &program_keypair.pubkey(),
        min_program_balance,
        (UpgradeableLoaderState::size_of_program() as u64).saturating_add(1),
        &bpf_loader_upgradeable::id(),
    );
    let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
    assert_eq!(
        TransactionError::InstructionError(1, InstructionError::ExecutableAccountNotRentExempt),
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .unwrap_err()
            .unwrap()
    );

    // Test program account too small
    bank.clear_signatures();
    bank.store_account(&buffer_address, &buffer_account);
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    let mut instructions = solana_loader_v3_interface::instruction::deploy_with_max_program_len(
        &mint_keypair.pubkey(),
        &program_keypair.pubkey(),
        &buffer_address,
        &upgrade_authority_keypair.pubkey(),
        min_program_balance,
        elf.len(),
    )
    .unwrap();
    *instructions.get_mut(0).unwrap() = system_instruction::create_account(
        &mint_keypair.pubkey(),
        &program_keypair.pubkey(),
        min_program_balance,
        (UpgradeableLoaderState::size_of_program() as u64).saturating_sub(1),
        &bpf_loader_upgradeable::id(),
    );
    let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
    assert_eq!(
        TransactionError::InstructionError(1, InstructionError::AccountDataTooSmall),
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .unwrap_err()
            .unwrap()
    );

    // Test Insufficient payer funds (need more funds to cover the
    // difference between buffer lamports and programdata lamports)
    bank.clear_signatures();
    bank.store_account(
        &mint_keypair.pubkey(),
        &AccountSharedData::new(
            deploy_fees.saturating_add(min_program_balance),
            0,
            &system_program::id(),
        ),
    );
    bank.store_account(&buffer_address, &buffer_account);
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    let message = Message::new(
        &solana_loader_v3_interface::instruction::deploy_with_max_program_len(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len(),
        )
        .unwrap(),
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        TransactionError::InstructionError(1, InstructionError::Custom(1)),
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .unwrap_err()
            .unwrap()
    );
    bank.store_account(
        &mint_keypair.pubkey(),
        &AccountSharedData::new(1_000_000_000, 0, &system_program::id()),
    );

    // Test max_data_len
    bank.clear_signatures();
    bank.store_account(&buffer_address, &buffer_account);
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    let message = Message::new(
        &solana_loader_v3_interface::instruction::deploy_with_max_program_len(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len().saturating_sub(1),
        )
        .unwrap(),
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        TransactionError::InstructionError(1, InstructionError::AccountDataTooSmall),
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .unwrap_err()
            .unwrap()
    );

    // Test max_data_len too large
    bank.clear_signatures();
    bank.store_account(
        &mint_keypair.pubkey(),
        &AccountSharedData::new(u64::MAX / 2, 0, &system_program::id()),
    );
    let mut modified_buffer_account = buffer_account.clone();
    modified_buffer_account.set_lamports(u64::MAX / 2);
    bank.store_account(&buffer_address, &modified_buffer_account);
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    let message = Message::new(
        &solana_loader_v3_interface::instruction::deploy_with_max_program_len(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            usize::MAX,
        )
        .unwrap(),
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        TransactionError::InstructionError(1, InstructionError::InvalidArgument),
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .unwrap_err()
            .unwrap()
    );

    fn truncate_data(account: &mut AccountSharedData, len: usize) {
        let mut data = account.data().to_vec();
        data.truncate(len);
        account.set_data(data);
    }

    // Test Bad ELF data
    bank.clear_signatures();
    let mut modified_buffer_account = buffer_account;
    truncate_data(
        &mut modified_buffer_account,
        UpgradeableLoaderState::size_of_buffer(1),
    );
    bank.store_account(&buffer_address, &modified_buffer_account);
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    let message = Message::new(
        &solana_loader_v3_interface::instruction::deploy_with_max_program_len(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len(),
        )
        .unwrap(),
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        TransactionError::InstructionError(1, InstructionError::InvalidAccountData),
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .unwrap_err()
            .unwrap()
    );

    // Test small buffer account
    bank.clear_signatures();
    let mut modified_buffer_account = AccountSharedData::new(
        min_programdata_balance,
        UpgradeableLoaderState::size_of_buffer(elf.len()),
        &bpf_loader_upgradeable::id(),
    );
    modified_buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(upgrade_authority_keypair.pubkey()),
        })
        .unwrap();
    modified_buffer_account
        .data_as_mut_slice()
        .get_mut(UpgradeableLoaderState::size_of_buffer_metadata()..)
        .unwrap()
        .copy_from_slice(&elf);
    truncate_data(&mut modified_buffer_account, 5);
    bank.store_account(&buffer_address, &modified_buffer_account);
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    let message = Message::new(
        &solana_loader_v3_interface::instruction::deploy_with_max_program_len(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len(),
        )
        .unwrap(),
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        TransactionError::InstructionError(1, InstructionError::InvalidAccountData),
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .unwrap_err()
            .unwrap()
    );

    // Mismatched buffer and program authority
    bank.clear_signatures();
    let mut modified_buffer_account = AccountSharedData::new(
        min_programdata_balance,
        UpgradeableLoaderState::size_of_buffer(elf.len()),
        &bpf_loader_upgradeable::id(),
    );
    modified_buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(buffer_address),
        })
        .unwrap();
    modified_buffer_account
        .data_as_mut_slice()
        .get_mut(UpgradeableLoaderState::size_of_buffer_metadata()..)
        .unwrap()
        .copy_from_slice(&elf);
    bank.store_account(&buffer_address, &modified_buffer_account);
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    let message = Message::new(
        &solana_loader_v3_interface::instruction::deploy_with_max_program_len(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len(),
        )
        .unwrap(),
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        TransactionError::InstructionError(1, InstructionError::IncorrectAuthority),
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .unwrap_err()
            .unwrap()
    );

    // Deploy buffer with mismatched None authority
    bank.clear_signatures();
    let mut modified_buffer_account = AccountSharedData::new(
        min_programdata_balance,
        UpgradeableLoaderState::size_of_buffer(elf.len()),
        &bpf_loader_upgradeable::id(),
    );
    modified_buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: None,
        })
        .unwrap();
    modified_buffer_account
        .data_as_mut_slice()
        .get_mut(UpgradeableLoaderState::size_of_buffer_metadata()..)
        .unwrap()
        .copy_from_slice(&elf);
    bank.store_account(&buffer_address, &modified_buffer_account);
    bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
    bank.store_account(&programdata_address, &AccountSharedData::default());
    let message = Message::new(
        &solana_loader_v3_interface::instruction::deploy_with_max_program_len(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len(),
        )
        .unwrap(),
        Some(&mint_keypair.pubkey()),
    );
    assert_eq!(
        TransactionError::InstructionError(1, InstructionError::IncorrectAuthority),
        bank_client
            .send_and_confirm_message(
                &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .unwrap_err()
            .unwrap()
    );
}

#[test]
fn test_compute_active_feature_set() {
    let (bank0, _bank_forks) = create_simple_test_arc_bank(100_000);
    let mut bank = Bank::new_from_parent(bank0, &Pubkey::default(), 1);

    let test_feature = "TestFeature11111111111111111111111111111111"
        .parse::<Pubkey>()
        .unwrap();
    let mut feature_set = FeatureSet::default();
    feature_set.inactive_mut().insert(test_feature);
    bank.feature_set = Arc::new(feature_set.clone());

    let (feature_set, new_activations) = bank.compute_active_feature_set(true);
    assert!(new_activations.is_empty());
    assert!(!feature_set.is_active(&test_feature));

    // Depositing into the `test_feature` account should do nothing
    test_utils::deposit(&bank, &test_feature, 42).unwrap();
    let (feature_set, new_activations) = bank.compute_active_feature_set(true);
    assert!(new_activations.is_empty());
    assert!(!feature_set.is_active(&test_feature));

    // Request `test_feature` activation
    let feature = Feature::default();
    assert_eq!(feature.activated_at, None);
    bank.store_account(&test_feature, &feature::create_account(&feature, 42));
    let feature = feature::from_account(&bank.get_account(&test_feature).expect("get_account"))
        .expect("from_account");
    assert_eq!(feature.activated_at, None);

    // Run `compute_active_feature_set` excluding pending activation
    let (feature_set, new_activations) = bank.compute_active_feature_set(false);
    assert!(new_activations.is_empty());
    assert!(!feature_set.is_active(&test_feature));

    // Run `compute_active_feature_set` including pending activation
    let (_feature_set, new_activations) = bank.compute_active_feature_set(true);
    assert_eq!(new_activations.len(), 1);
    assert!(new_activations.contains(&test_feature));

    // Actually activate the pending activation
    bank.compute_and_apply_new_feature_activations();
    let feature = feature::from_account(&bank.get_account(&test_feature).expect("get_account"))
        .expect("from_account");
    assert_eq!(feature.activated_at, Some(1));

    let (feature_set, new_activations) = bank.compute_active_feature_set(true);
    assert!(new_activations.is_empty());
    assert!(feature_set.is_active(&test_feature));
}

#[test]
fn test_reserved_account_keys() {
    let (bank0, _bank_forks) = create_simple_test_arc_bank(100_000);
    let mut bank = Bank::new_from_parent(bank0, &Pubkey::default(), 1);
    let test_feature_id = Pubkey::new_unique();
    bank.feature_set = Arc::new(FeatureSet::new(
        AHashMap::new(),
        AHashSet::from([test_feature_id]),
    ));
    bank.reserved_account_keys = Arc::new(ReservedAccountKeys::new(&[
        ReservedAccount::new_active(system_program::id()),
        ReservedAccount::new_pending(Pubkey::new_unique(), test_feature_id),
    ]));

    assert_eq!(
        bank.get_reserved_account_keys().len(),
        1,
        "before activating the test feature, bank should already have an active reserved key"
    );

    bank.store_account(
        &test_feature_id,
        &feature::create_account(&Feature::default(), 42),
    );
    bank.compute_and_apply_new_feature_activations();

    assert_eq!(
        bank.get_reserved_account_keys().len(),
        2,
        "after activating the test feature, bank should have another active reserved key"
    );
}

#[test]
fn test_block_limits() {
    const MAX_WRITABLE_ACCOUNT_UNITS_SIMD_0306_FIRST: u64 = 24_000_000;
    const MAX_WRITABLE_ACCOUNT_UNITS_SIMD_0306_SECOND: u64 = 40_000_000;
    let (bank0, _bank_forks) = create_simple_test_arc_bank(100_000);
    let mut bank = Bank::new_from_parent(bank0, &Pubkey::default(), 1);

    // Ensure increased block limits features are inactive.
    assert!(!bank
        .feature_set
        .is_active(&feature_set::raise_block_limits_to_100m::id()));
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_block_limit(),
        MAX_BLOCK_UNITS,
        "before activating the feature, bank should have old/default limit"
    );
    assert!(!bank
        .feature_set
        .is_active(&feature_set::raise_account_cu_limit::id()));
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_account_limit(),
        MAX_WRITABLE_ACCOUNT_UNITS,
        "before activating the feature, bank should have old/default limit"
    );

    // Activate `raise_block_limits_to_100m` feature
    bank.store_account(
        &feature_set::raise_block_limits_to_100m::id(),
        &feature::create_account(&Feature::default(), 42),
    );
    // compute_and_apply_features_after_snapshot_restore will not cause the block limit to be updated
    bank.compute_and_apply_features_after_snapshot_restore();
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_block_limit(),
        MAX_BLOCK_UNITS,
        "before activating the feature, bank should have old/default limit"
    );

    // compute_and_apply_new_feature_activations will cause feature to be activated
    bank.compute_and_apply_new_feature_activations();
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_block_limit(),
        MAX_BLOCK_UNITS_SIMD_0286,
        "after activating the feature, bank should have new limit"
    );
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_account_limit(),
        MAX_WRITABLE_ACCOUNT_UNITS,
        "after activating the feature, bank should have new limit"
    );

    // Make sure the limits propagate to the child-bank.
    let mut bank = Bank::new_from_parent(Arc::new(bank), &Pubkey::default(), 2);
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_block_limit(),
        MAX_BLOCK_UNITS_SIMD_0286,
        "child bank should have new limit"
    );

    // Activate `raise_account_cu_limit` feature
    bank.store_account(
        &feature_set::raise_account_cu_limit::id(),
        &feature::create_account(&Feature::default(), 42),
    );

    // compute_and_apply_features_after_snapshot_restore will not cause the block limit to be updated
    bank.compute_and_apply_features_after_snapshot_restore();
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_account_limit(),
        MAX_WRITABLE_ACCOUNT_UNITS,
        "before activating the feature, bank should have old/default limit"
    );

    // compute_and_apply_new_feature_activations will cause feature to be activated
    bank.compute_and_apply_new_feature_activations();
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_account_limit(),
        MAX_WRITABLE_ACCOUNT_UNITS_SIMD_0306_SECOND,
        "after activating the feature, bank should have new limit"
    );

    // Test SIMD-0306 getting activated first
    let (bank0, _bank_forks) = create_simple_test_arc_bank(100_000);
    let mut bank = Bank::new_from_parent(bank0, &Pubkey::default(), 1);

    // Activate `raise_account_cu_limit` feature
    bank.store_account(
        &feature_set::raise_account_cu_limit::id(),
        &feature::create_account(&Feature::default(), 42),
    );
    // compute_and_apply_features_after_snapshot_restore will not cause the block limit to be updated
    bank.compute_and_apply_features_after_snapshot_restore();
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_account_limit(),
        MAX_WRITABLE_ACCOUNT_UNITS,
        "before activating the feature, bank should have old/default limit"
    );

    // compute_and_apply_new_feature_activations will cause feature to be activated
    bank.compute_and_apply_new_feature_activations();
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_block_limit(),
        MAX_BLOCK_UNITS,
        "after activating the feature, bank should have new limit"
    );
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_account_limit(),
        MAX_WRITABLE_ACCOUNT_UNITS_SIMD_0306_FIRST,
        "after activating the feature, bank should have new limit"
    );

    // Activate `raise_block_limits_to_100m` feature
    bank.store_account(
        &feature_set::raise_block_limits_to_100m::id(),
        &feature::create_account(&Feature::default(), 42),
    );
    // compute_and_apply_features_after_snapshot_restore will not cause the block limit to be updated
    bank.compute_and_apply_features_after_snapshot_restore();
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_block_limit(),
        MAX_BLOCK_UNITS,
        "before activating the feature, bank should have old/default limit"
    );

    // compute_and_apply_new_feature_activations will cause feature to be activated
    bank.compute_and_apply_new_feature_activations();
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_block_limit(),
        MAX_BLOCK_UNITS_SIMD_0286,
        "after activating the feature, bank should have new limit"
    );
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_account_limit(),
        MAX_WRITABLE_ACCOUNT_UNITS_SIMD_0306_SECOND,
        "after activating the feature, bank should have new limit"
    );

    // Test starting from a genesis config with and without feature account
    let (mut genesis_config, _keypair) = create_genesis_config(100_000);
    // Without feature account in genesis, old limits are used.
    let bank = Bank::new_for_tests(&genesis_config);
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_block_limit(),
        MAX_BLOCK_UNITS,
        "before activating the feature, bank should have old/default limit"
    );
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_account_limit(),
        MAX_WRITABLE_ACCOUNT_UNITS,
        "before activating the feature, bank should have old/default limit"
    );

    activate_feature(
        &mut genesis_config,
        feature_set::raise_block_limits_to_100m::id(),
    );
    let bank = Bank::new_for_tests(&genesis_config);
    assert!(bank
        .feature_set
        .is_active(&feature_set::raise_block_limits_to_100m::id()));
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_block_limit(),
        MAX_BLOCK_UNITS_SIMD_0286,
        "bank created from genesis config should have new limit"
    );

    activate_feature(
        &mut genesis_config,
        feature_set::raise_account_cu_limit::id(),
    );
    let bank = Bank::new_for_tests(&genesis_config);
    assert!(bank
        .feature_set
        .is_active(&feature_set::raise_account_cu_limit::id()));
    assert_eq!(
        bank.read_cost_tracker().unwrap().get_account_limit(),
        MAX_WRITABLE_ACCOUNT_UNITS_SIMD_0306_SECOND,
        "bank created from genesis config should have new limit"
    );
}

fn min_rent_exempt_balance_for_sysvars(bank: &Bank, sysvar_ids: &[Pubkey]) -> u64 {
    sysvar_ids
        .iter()
        .map(|sysvar_id| {
            trace!("min_rent_excempt_balance_for_sysvars: {sysvar_id}");
            bank.get_minimum_balance_for_rent_exemption(
                bank.get_account(sysvar_id).unwrap().data().len(),
            )
        })
        .sum()
}

#[test]
fn test_adjust_sysvar_balance_for_rent() {
    let bank = create_simple_test_bank(0);
    let mut smaller_sample_sysvar = AccountSharedData::new(1, 0, &Pubkey::default());
    assert_eq!(smaller_sample_sysvar.lamports(), 1);
    bank.adjust_sysvar_balance_for_rent(&mut smaller_sample_sysvar);
    assert_eq!(
        smaller_sample_sysvar.lamports(),
        bank.get_minimum_balance_for_rent_exemption(smaller_sample_sysvar.data().len()),
    );

    let mut bigger_sample_sysvar = AccountSharedData::new(
        1,
        smaller_sample_sysvar.data().len() + 1,
        &Pubkey::default(),
    );
    bank.adjust_sysvar_balance_for_rent(&mut bigger_sample_sysvar);
    assert!(smaller_sample_sysvar.lamports() < bigger_sample_sysvar.lamports());

    // excess lamports shouldn't be reduced by adjust_sysvar_balance_for_rent()
    let excess_lamports = smaller_sample_sysvar.lamports() + 999;
    smaller_sample_sysvar.set_lamports(excess_lamports);
    bank.adjust_sysvar_balance_for_rent(&mut smaller_sample_sysvar);
    assert_eq!(smaller_sample_sysvar.lamports(), excess_lamports);
}

#[test]
fn test_update_clock_timestamp() {
    let leader_pubkey = solana_pubkey::new_rand();
    let GenesisConfigInfo {
        genesis_config,
        voting_keypair,
        ..
    } = create_genesis_config_with_leader(5, &leader_pubkey, 3);
    let mut bank = Bank::new_for_tests(&genesis_config);
    // Advance past slot 0, which has special handling.
    bank = new_from_parent(Arc::new(bank));
    bank = new_from_parent(Arc::new(bank));
    assert_eq!(
        bank.clock().unix_timestamp,
        bank.unix_timestamp_from_genesis()
    );

    bank.update_clock(None);
    assert_eq!(
        bank.clock().unix_timestamp,
        bank.unix_timestamp_from_genesis()
    );

    update_vote_account_timestamp(
        BlockTimestamp {
            slot: bank.slot(),
            timestamp: bank.unix_timestamp_from_genesis() - 1,
        },
        &bank,
        &voting_keypair.pubkey(),
    );
    bank.update_clock(None);
    assert_eq!(
        bank.clock().unix_timestamp,
        bank.unix_timestamp_from_genesis()
    );

    update_vote_account_timestamp(
        BlockTimestamp {
            slot: bank.slot(),
            timestamp: bank.unix_timestamp_from_genesis(),
        },
        &bank,
        &voting_keypair.pubkey(),
    );
    bank.update_clock(None);
    assert_eq!(
        bank.clock().unix_timestamp,
        bank.unix_timestamp_from_genesis()
    );

    update_vote_account_timestamp(
        BlockTimestamp {
            slot: bank.slot(),
            timestamp: bank.unix_timestamp_from_genesis() + 1,
        },
        &bank,
        &voting_keypair.pubkey(),
    );
    bank.update_clock(None);
    assert_eq!(
        bank.clock().unix_timestamp,
        bank.unix_timestamp_from_genesis() + 1
    );

    // Timestamp cannot go backward from ancestor Bank to child
    bank = new_from_parent(Arc::new(bank));
    update_vote_account_timestamp(
        BlockTimestamp {
            slot: bank.slot(),
            timestamp: bank.unix_timestamp_from_genesis() - 1,
        },
        &bank,
        &voting_keypair.pubkey(),
    );
    bank.update_clock(None);
    assert_eq!(
        bank.clock().unix_timestamp,
        bank.unix_timestamp_from_genesis()
    );
}

fn poh_estimate_offset(bank: &Bank) -> Duration {
    let mut epoch_start_slot = bank.epoch_schedule.get_first_slot_in_epoch(bank.epoch());
    if epoch_start_slot == bank.slot() {
        epoch_start_slot = bank
            .epoch_schedule
            .get_first_slot_in_epoch(bank.epoch() - 1);
    }
    bank.slot().saturating_sub(epoch_start_slot) as u32
        * Duration::from_nanos(bank.ns_per_slot as u64)
}

#[test]
fn test_timestamp_slow() {
    fn max_allowable_delta_since_epoch(bank: &Bank, max_allowable_drift: u32) -> i64 {
        let poh_estimate_offset = poh_estimate_offset(bank);
        (poh_estimate_offset.as_secs()
            + (poh_estimate_offset * max_allowable_drift / 100).as_secs()) as i64
    }

    let leader_pubkey = solana_pubkey::new_rand();
    let GenesisConfigInfo {
        mut genesis_config,
        voting_keypair,
        ..
    } = create_genesis_config_with_leader(5, &leader_pubkey, 3);
    let slots_in_epoch = 32;
    genesis_config.epoch_schedule = EpochSchedule::new(slots_in_epoch);
    let mut bank = Bank::new_for_tests(&genesis_config);
    let slot_duration = Duration::from_nanos(bank.ns_per_slot as u64);

    let recent_timestamp: UnixTimestamp = bank.unix_timestamp_from_genesis();
    let additional_secs =
        ((slot_duration * MAX_ALLOWABLE_DRIFT_PERCENTAGE_SLOW_V2 * 32) / 100).as_secs() as i64 + 1; // Greater than max_allowable_drift_slow_v2 for full epoch
    update_vote_account_timestamp(
        BlockTimestamp {
            slot: bank.slot(),
            timestamp: recent_timestamp + additional_secs,
        },
        &bank,
        &voting_keypair.pubkey(),
    );

    // additional_secs greater than MAX_ALLOWABLE_DRIFT_PERCENTAGE_SLOW_V2 for an epoch
    // timestamp bounded to 150% deviation
    for _ in 0..31 {
        bank = new_from_parent(Arc::new(bank));
        assert_eq!(
            bank.clock().unix_timestamp,
            bank.clock().epoch_start_timestamp
                + max_allowable_delta_since_epoch(&bank, MAX_ALLOWABLE_DRIFT_PERCENTAGE_SLOW_V2),
        );
        assert_eq!(bank.clock().epoch_start_timestamp, recent_timestamp);
    }
}

#[test]
fn test_timestamp_fast() {
    fn max_allowable_delta_since_epoch(bank: &Bank, max_allowable_drift: u32) -> i64 {
        let poh_estimate_offset = poh_estimate_offset(bank);
        (poh_estimate_offset.as_secs()
            - (poh_estimate_offset * max_allowable_drift / 100).as_secs()) as i64
    }

    let leader_pubkey = solana_pubkey::new_rand();
    let GenesisConfigInfo {
        mut genesis_config,
        voting_keypair,
        ..
    } = create_genesis_config_with_leader(5, &leader_pubkey, 3);
    let slots_in_epoch = 32;
    genesis_config.epoch_schedule = EpochSchedule::new(slots_in_epoch);
    let mut bank = Bank::new_for_tests(&genesis_config);

    let recent_timestamp: UnixTimestamp = bank.unix_timestamp_from_genesis();
    let additional_secs = 5; // Greater than MAX_ALLOWABLE_DRIFT_PERCENTAGE_FAST for full epoch
    update_vote_account_timestamp(
        BlockTimestamp {
            slot: bank.slot(),
            timestamp: recent_timestamp - additional_secs,
        },
        &bank,
        &voting_keypair.pubkey(),
    );

    // additional_secs greater than MAX_ALLOWABLE_DRIFT_PERCENTAGE_FAST for an epoch
    // timestamp bounded to 25% deviation
    for _ in 0..31 {
        bank = new_from_parent(Arc::new(bank));
        assert_eq!(
            bank.clock().unix_timestamp,
            bank.clock().epoch_start_timestamp
                + max_allowable_delta_since_epoch(&bank, MAX_ALLOWABLE_DRIFT_PERCENTAGE_FAST),
        );
        assert_eq!(bank.clock().epoch_start_timestamp, recent_timestamp);
    }
}

#[test]
fn test_program_is_native_loader() {
    let (genesis_config, mint_keypair) = create_genesis_config(50000);
    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();

    let tx = Transaction::new_signed_with_payer(
        &[Instruction::new_with_bincode(
            native_loader::id(),
            &(),
            vec![],
        )],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair],
        bank.last_blockhash(),
    );

    let err = bank.process_transaction(&tx).unwrap_err();
    assert_eq!(err, TransactionError::ProgramAccountNotFound);
}

#[test]
fn test_invoke_non_program_account_owned_by_a_builtin() {
    let (genesis_config, mint_keypair) = create_genesis_config(10000000);
    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();

    let bogus_program = Pubkey::new_unique();
    bank.transfer(
        genesis_config.rent.minimum_balance(0),
        &mint_keypair,
        &bogus_program,
    )
    .unwrap();

    let created_account_keypair = Keypair::new();
    let mut ix = system_instruction::create_account(
        &mint_keypair.pubkey(),
        &created_account_keypair.pubkey(),
        genesis_config.rent.minimum_balance(0),
        0,
        &system_program::id(),
    );
    // Calling an account owned by the system program, instead of calling the system program itself
    ix.program_id = bogus_program;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair, &created_account_keypair],
        bank.last_blockhash(),
    );
    assert_eq!(
        bank.process_transaction(&tx),
        Err(TransactionError::InvalidProgramForExecution)
    );
}

#[test]
fn test_debug_bank() {
    let (genesis_config, _mint_keypair) = create_genesis_config(50000);
    let bank = Bank::new_for_tests(&genesis_config);
    let debug = format!("{bank:#?}");
    assert!(!debug.is_empty());
}

#[derive(Debug)]
enum AcceptableScanResults {
    DroppedSlotError,
    NoFailure,
    Both,
}

fn test_store_scan_consistency<F>(
    update_f: F,
    drop_callback: Option<Box<dyn DropCallback + Send + Sync>>,
    acceptable_scan_results: AcceptableScanResults,
) where
    F: Fn(
            Arc<Bank>,
            crossbeam_channel::Sender<Arc<Bank>>,
            crossbeam_channel::Receiver<BankId>,
            Arc<HashSet<Pubkey>>,
            Pubkey,
            u64,
        ) + std::marker::Send
        + 'static,
{
    agave_logger::setup();
    // Set up initial bank
    let mut genesis_config =
        create_genesis_config_with_leader(10, &solana_pubkey::new_rand(), 374_999_998_287_840)
            .genesis_config;
    genesis_config.rent = Rent::free();
    let bank0 = Arc::new(Bank::new_with_config_for_tests(
        &genesis_config,
        BankTestConfig::default(),
    ));
    bank0.set_callback(drop_callback);

    // Set up pubkeys to write to
    let total_pubkeys = ITER_BATCH_SIZE * 10;
    let total_pubkeys_to_modify = 10;
    let all_pubkeys: Vec<Pubkey> = std::iter::repeat_with(solana_pubkey::new_rand)
        .take(total_pubkeys)
        .collect();
    let program_id = system_program::id();
    let starting_lamports = 1;
    let starting_account = AccountSharedData::new(starting_lamports, 0, &program_id);

    // Write accounts to the store
    for key in &all_pubkeys {
        bank0.store_account(key, &starting_account);
    }

    // Set aside a subset of accounts to modify
    let pubkeys_to_modify: Arc<HashSet<Pubkey>> = Arc::new(
        all_pubkeys
            .into_iter()
            .take(total_pubkeys_to_modify)
            .collect(),
    );
    let exit = Arc::new(AtomicBool::new(false));

    // Thread that runs scan and constantly checks for
    // consistency
    let pubkeys_to_modify_ = pubkeys_to_modify.clone();

    // Channel over which the bank to scan is sent
    let (bank_to_scan_sender, bank_to_scan_receiver): (
        crossbeam_channel::Sender<Arc<Bank>>,
        crossbeam_channel::Receiver<Arc<Bank>>,
    ) = bounded(1);

    let (scan_finished_sender, scan_finished_receiver): (
        crossbeam_channel::Sender<BankId>,
        crossbeam_channel::Receiver<BankId>,
    ) = unbounded();
    let num_banks_scanned = Arc::new(AtomicU64::new(0));
    let scan_thread = {
        let exit = exit.clone();
        let num_banks_scanned = num_banks_scanned.clone();
        Builder::new()
            .name("scan".to_string())
            .spawn(move || {
                loop {
                    info!("starting scan iteration");
                    if exit.load(Relaxed) {
                        info!("scan exiting");
                        return;
                    }
                    if let Ok(bank_to_scan) =
                        bank_to_scan_receiver.recv_timeout(Duration::from_millis(10))
                    {
                        info!("scanning program accounts for slot {}", bank_to_scan.slot());
                        let accounts_result =
                            bank_to_scan.get_program_accounts(&program_id, &ScanConfig::default());
                        let _ = scan_finished_sender.send(bank_to_scan.bank_id());
                        num_banks_scanned.fetch_add(1, Relaxed);
                        match (&acceptable_scan_results, accounts_result.is_err()) {
                            (AcceptableScanResults::DroppedSlotError, _)
                            | (AcceptableScanResults::Both, true) => {
                                assert_eq!(
                                    accounts_result,
                                    Err(ScanError::SlotRemoved {
                                        slot: bank_to_scan.slot(),
                                        bank_id: bank_to_scan.bank_id()
                                    })
                                );
                            }
                            (AcceptableScanResults::NoFailure, _)
                            | (AcceptableScanResults::Both, false) => {
                                assert!(accounts_result.is_ok())
                            }
                        }

                        // Should never see empty accounts because no slot ever deleted
                        // any of the original accounts, and the scan should reflect the
                        // account state at some frozen slot `X` (no partial updates).
                        if let Ok(accounts) = accounts_result {
                            assert!(!accounts.is_empty());
                            let mut expected_lamports = None;
                            let mut target_accounts_found = HashSet::new();
                            for (pubkey, account) in accounts {
                                let account_balance = account.lamports();
                                if pubkeys_to_modify_.contains(&pubkey) {
                                    target_accounts_found.insert(pubkey);
                                    if let Some(expected_lamports) = expected_lamports {
                                        assert_eq!(account_balance, expected_lamports);
                                    } else {
                                        // All pubkeys in the specified set should have the same balance
                                        expected_lamports = Some(account_balance);
                                    }
                                }
                            }

                            // Should've found all the accounts, i.e. no partial cleans should
                            // be detected
                            assert_eq!(target_accounts_found.len(), total_pubkeys_to_modify);
                        }
                    }
                }
            })
            .unwrap()
    };

    // Thread that constantly updates the accounts, sets
    // roots, and cleans
    let update_thread = Builder::new()
        .name("update".to_string())
        .spawn(move || {
            update_f(
                bank0,
                bank_to_scan_sender,
                scan_finished_receiver,
                pubkeys_to_modify,
                program_id,
                starting_lamports,
            );
        })
        .unwrap();

    // Let threads run for a while, check the scans didn't see any mixed slots
    let min_expected_number_of_scans = 5;
    std::thread::sleep(Duration::new(5, 0));
    // This can be reduced when you are running this test locally to deal with hangs
    // But, if it is too low, the ci fails intermittently.
    let mut remaining_loops = 2000;
    loop {
        if num_banks_scanned.load(Relaxed) > min_expected_number_of_scans {
            break;
        } else {
            std::thread::sleep(Duration::from_millis(100));
        }
        remaining_loops -= 1;
        if remaining_loops == 0 {
            break; // just quit and try to get the thread result (panic, etc.)
        }
    }
    exit.store(true, Relaxed);
    scan_thread.join().unwrap();
    update_thread.join().unwrap();
    assert!(remaining_loops > 0, "test timed out");
}

#[test]
fn test_store_scan_consistency_unrooted() {
    let (pruned_banks_sender, pruned_banks_receiver) = unbounded();
    let pruned_banks_request_handler = PrunedBanksRequestHandler {
        pruned_banks_receiver,
    };
    test_store_scan_consistency(
        move |bank0,
              bank_to_scan_sender,
              _scan_finished_receiver,
              pubkeys_to_modify,
              program_id,
              starting_lamports| {
            let mut current_major_fork_bank = bank0;
            loop {
                let mut current_minor_fork_bank = current_major_fork_bank.clone();
                let num_new_banks = 2;
                let lamports = current_minor_fork_bank.slot() + starting_lamports + 1;
                // Modify banks on the two banks on the minor fork
                for pubkeys_to_modify in &pubkeys_to_modify
                    .iter()
                    .chunks(pubkeys_to_modify.len() / num_new_banks)
                {
                    let slot = current_minor_fork_bank.slot() + 2;
                    current_minor_fork_bank = Arc::new(Bank::new_from_parent(
                        current_minor_fork_bank,
                        &solana_pubkey::new_rand(),
                        slot,
                    ));
                    let account = AccountSharedData::new(lamports, 0, &program_id);
                    // Write partial updates to each of the banks in the minor fork so if any of them
                    // get cleaned up, there will be keys with the wrong account value/missing.
                    for key in pubkeys_to_modify {
                        current_minor_fork_bank.store_account(key, &account);
                    }
                    current_minor_fork_bank.freeze();
                }

                // All the parent banks made in this iteration of the loop
                // are currently discoverable, previous parents should have
                // been squashed
                assert_eq!(
                    current_minor_fork_bank.clone().parents_inclusive().len(),
                    num_new_banks + 1,
                );

                // `next_major_bank` needs to be sandwiched between the minor fork banks
                // That way, after the squash(), the minor fork has the potential to see a
                // *partial* clean of the banks < `next_major_bank`.
                current_major_fork_bank = Arc::new(Bank::new_from_parent(
                    current_major_fork_bank,
                    &solana_pubkey::new_rand(),
                    current_minor_fork_bank.slot() - 1,
                ));
                let lamports = current_major_fork_bank.slot() + starting_lamports + 1;
                let account = AccountSharedData::new(lamports, 0, &program_id);
                for key in pubkeys_to_modify.iter() {
                    // Store rooted updates to these pubkeys such that the minor
                    // fork updates to the same keys will be deleted by clean
                    current_major_fork_bank.store_account(key, &account);
                }

                // Send the last new bank to the scan thread to perform the scan.
                // Meanwhile this thread will continually set roots on a separate fork
                // and squash/clean, purging the account entries from the minor forks
                /*
                            bank 0
                        /         \
                minor bank 1       \
                    /         current_major_fork_bank
                minor bank 2

                */
                // The capacity of the channel is 1 so that this thread will wait for the scan to finish before starting
                // the next iteration, allowing the scan to stay in sync with these updates
                // such that every scan will see this interruption.
                if bank_to_scan_sender.send(current_minor_fork_bank).is_err() {
                    // Channel was disconnected, exit
                    return;
                }
                current_major_fork_bank.freeze();
                current_major_fork_bank.squash();
                // Try to get cache flush/clean to overlap with the scan
                current_major_fork_bank.force_flush_accounts_cache();
                current_major_fork_bank.clean_accounts_for_tests();
                // Move purge here so that Bank::drop()->purge_slots() doesn't race
                // with clean. Simulates the call from AccountsBackgroundService
                pruned_banks_request_handler.handle_request(&current_major_fork_bank);
            }
        },
        Some(Box::new(SendDroppedBankCallback::new(pruned_banks_sender))),
        AcceptableScanResults::NoFailure,
    )
}

#[test]
fn test_store_scan_consistency_root() {
    let (pruned_banks_sender, pruned_banks_receiver) = unbounded();
    let pruned_banks_request_handler = PrunedBanksRequestHandler {
        pruned_banks_receiver,
    };
    test_store_scan_consistency(
        move |bank0,
              bank_to_scan_sender,
              _scan_finished_receiver,
              pubkeys_to_modify,
              program_id,
              starting_lamports| {
            let mut current_bank = bank0.clone();
            let mut prev_bank = bank0;
            loop {
                let lamports_this_round = current_bank.slot() + starting_lamports + 1;
                let account = AccountSharedData::new(lamports_this_round, 0, &program_id);
                for key in pubkeys_to_modify.iter() {
                    current_bank.store_account(key, &account);
                }
                current_bank.freeze();
                // Send the previous bank to the scan thread to perform the scan.
                // Meanwhile this thread will squash and update roots immediately after
                // so the roots will update while scanning.
                //
                // The capacity of the channel is 1 so that this thread will wait for the scan to finish before starting
                // the next iteration, allowing the scan to stay in sync with these updates
                // such that every scan will see this interruption.
                if bank_to_scan_sender.send(prev_bank).is_err() {
                    // Channel was disconnected, exit
                    return;
                }
                current_bank.squash();
                if current_bank.slot() % 2 == 0 {
                    current_bank.force_flush_accounts_cache();
                    current_bank.clean_accounts();
                }
                prev_bank = current_bank.clone();
                let slot = current_bank.slot() + 1;
                current_bank = Arc::new(Bank::new_from_parent(
                    current_bank,
                    &solana_pubkey::new_rand(),
                    slot,
                ));

                // Move purge here so that Bank::drop()->purge_slots() doesn't race
                // with clean. Simulates the call from AccountsBackgroundService
                pruned_banks_request_handler.handle_request(&current_bank);
            }
        },
        Some(Box::new(SendDroppedBankCallback::new(pruned_banks_sender))),
        AcceptableScanResults::NoFailure,
    );
}

fn setup_banks_on_fork_to_remove(
    bank0: Arc<Bank>,
    pubkeys_to_modify: Arc<HashSet<Pubkey>>,
    program_id: &Pubkey,
    starting_lamports: u64,
    num_banks_on_fork: usize,
    step_size: usize,
) -> (Arc<Bank>, Vec<(Slot, BankId)>, Ancestors) {
    // Need at least 2 keys to create inconsistency in account balances when deleting
    // slots
    assert!(pubkeys_to_modify.len() > 1);

    // Tracks the bank at the tip of the to be created fork
    let mut bank_at_fork_tip = bank0;

    // All the slots on the fork except slot 0
    let mut slots_on_fork = Vec::with_capacity(num_banks_on_fork);

    // All accounts in each set of `step_size` slots will have the same account balances.
    // The account balances of the accounts changes every `step_size` banks. Thus if you
    // delete any one of the latest `step_size` slots, then you will see varying account
    // balances when loading the accounts.
    assert!(num_banks_on_fork >= 2);
    assert!(step_size >= 2);
    let pubkeys_to_modify: Vec<Pubkey> = pubkeys_to_modify.iter().cloned().collect();
    let pubkeys_to_modify_per_slot = (pubkeys_to_modify.len() / step_size).max(1);
    for _ in (0..num_banks_on_fork).step_by(step_size) {
        let mut lamports_this_round = 0;
        for i in 0..step_size {
            let slot = bank_at_fork_tip.slot() + 1;
            bank_at_fork_tip = Arc::new(Bank::new_from_parent(
                bank_at_fork_tip,
                &solana_pubkey::new_rand(),
                slot,
            ));
            if lamports_this_round == 0 {
                lamports_this_round = bank_at_fork_tip.bank_id() + starting_lamports + 1;
            }
            let pubkey_to_modify_starting_index = i * pubkeys_to_modify_per_slot;
            let account = AccountSharedData::new(lamports_this_round, 0, program_id);
            for pubkey_index_to_modify in pubkey_to_modify_starting_index
                ..pubkey_to_modify_starting_index + pubkeys_to_modify_per_slot
            {
                let key = pubkeys_to_modify[pubkey_index_to_modify % pubkeys_to_modify.len()];
                bank_at_fork_tip.store_account(&key, &account);
            }
            bank_at_fork_tip.freeze();
            slots_on_fork.push((bank_at_fork_tip.slot(), bank_at_fork_tip.bank_id()));
        }
    }

    let ancestors: Vec<(Slot, usize)> = slots_on_fork.iter().map(|(s, _)| (*s, 0)).collect();
    let ancestors = Ancestors::from(ancestors);

    (bank_at_fork_tip, slots_on_fork, ancestors)
}

#[test]
fn test_remove_unrooted_before_scan() {
    test_store_scan_consistency(
        |bank0,
         bank_to_scan_sender,
         scan_finished_receiver,
         pubkeys_to_modify,
         program_id,
         starting_lamports| {
            loop {
                let (bank_at_fork_tip, slots_on_fork, ancestors) = setup_banks_on_fork_to_remove(
                    bank0.clone(),
                    pubkeys_to_modify.clone(),
                    &program_id,
                    starting_lamports,
                    10,
                    2,
                );
                // Test removing the slot before the scan starts, should cause
                // SlotRemoved error every time
                for k in pubkeys_to_modify.iter() {
                    assert!(bank_at_fork_tip.load_slow(&ancestors, k).is_some());
                }
                bank_at_fork_tip.remove_unrooted_slots(&slots_on_fork);

                // Accounts on this fork should not be found after removal
                for k in pubkeys_to_modify.iter() {
                    assert!(bank_at_fork_tip.load_slow(&ancestors, k).is_none());
                }
                if bank_to_scan_sender.send(bank_at_fork_tip.clone()).is_err() {
                    return;
                }

                // Wait for scan to finish before starting next iteration
                let finished_scan_bank_id = scan_finished_receiver.recv();
                if finished_scan_bank_id.is_err() {
                    return;
                }
                assert_eq!(finished_scan_bank_id.unwrap(), bank_at_fork_tip.bank_id());
            }
        },
        None,
        // Test removing the slot before the scan starts, should error every time
        AcceptableScanResults::DroppedSlotError,
    );
}

#[test]
fn test_remove_unrooted_scan_then_recreate_same_slot_before_scan() {
    test_store_scan_consistency(
        |bank0,
         bank_to_scan_sender,
         scan_finished_receiver,
         pubkeys_to_modify,
         program_id,
         starting_lamports| {
            let mut prev_bank = bank0.clone();
            loop {
                let start = Instant::now();
                let (bank_at_fork_tip, slots_on_fork, ancestors) = setup_banks_on_fork_to_remove(
                    bank0.clone(),
                    pubkeys_to_modify.clone(),
                    &program_id,
                    starting_lamports,
                    10,
                    2,
                );
                info!("setting up banks elapsed: {}", start.elapsed().as_millis());
                // Remove the fork. Then we'll recreate the slots and only after we've
                // recreated the slots, do we send this old bank for scanning.
                // Skip scanning bank 0 on first iteration of loop, since those accounts
                // aren't being removed
                if prev_bank.slot() != 0 {
                    info!(
                        "sending bank with slot: {:?}, elapsed: {}",
                        prev_bank.slot(),
                        start.elapsed().as_millis()
                    );
                    // Although we dumped the slots last iteration via `remove_unrooted_slots()`,
                    // we've recreated those slots this iteration, so they should be findable
                    // again
                    for k in pubkeys_to_modify.iter() {
                        assert!(bank_at_fork_tip.load_slow(&ancestors, k).is_some());
                    }

                    // Now after we've recreated the slots removed in the previous loop
                    // iteration, send the previous bank, should fail even though the
                    // same slots were recreated
                    if bank_to_scan_sender.send(prev_bank.clone()).is_err() {
                        return;
                    }

                    let finished_scan_bank_id = scan_finished_receiver.recv();
                    if finished_scan_bank_id.is_err() {
                        return;
                    }
                    // Wait for scan to finish before starting next iteration
                    assert_eq!(finished_scan_bank_id.unwrap(), prev_bank.bank_id());
                }
                bank_at_fork_tip.remove_unrooted_slots(&slots_on_fork);
                prev_bank = bank_at_fork_tip;
            }
        },
        None,
        // Test removing the slot before the scan starts, should error every time
        AcceptableScanResults::DroppedSlotError,
    );
}

#[test]
fn test_remove_unrooted_scan_interleaved_with_remove_unrooted_slots() {
    test_store_scan_consistency(
        |bank0,
         bank_to_scan_sender,
         scan_finished_receiver,
         pubkeys_to_modify,
         program_id,
         starting_lamports| {
            loop {
                let step_size = 2;
                let (bank_at_fork_tip, slots_on_fork, ancestors) = setup_banks_on_fork_to_remove(
                    bank0.clone(),
                    pubkeys_to_modify.clone(),
                    &program_id,
                    starting_lamports,
                    10,
                    step_size,
                );
                // Although we dumped the slots last iteration via `remove_unrooted_slots()`,
                // we've recreated those slots this iteration, so they should be findable
                // again
                for k in pubkeys_to_modify.iter() {
                    assert!(bank_at_fork_tip.load_slow(&ancestors, k).is_some());
                }

                // Now after we've recreated the slots removed in the previous loop
                // iteration, send the previous bank, should fail even though the
                // same slots were recreated
                if bank_to_scan_sender.send(bank_at_fork_tip.clone()).is_err() {
                    return;
                }

                // Remove 1 < `step_size` of the *latest* slots while the scan is happening.
                // This should create inconsistency between the account balances of accounts
                // stored in that slot, and the accounts stored in earlier slots
                let slot_to_remove = *slots_on_fork.last().unwrap();
                bank_at_fork_tip.remove_unrooted_slots(&[slot_to_remove]);

                // Wait for scan to finish before starting next iteration
                let finished_scan_bank_id = scan_finished_receiver.recv();
                if finished_scan_bank_id.is_err() {
                    return;
                }
                assert_eq!(finished_scan_bank_id.unwrap(), bank_at_fork_tip.bank_id());

                // Remove the rest of the slots before the next iteration
                for (slot, bank_id) in slots_on_fork {
                    bank_at_fork_tip.remove_unrooted_slots(&[(slot, bank_id)]);
                }
            }
        },
        None,
        // Test removing the slot before the scan starts, should error every time
        AcceptableScanResults::Both,
    );
}

#[test]
fn test_get_inflation_start_slot_devnet_testnet() {
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config_with_leader(42, &solana_pubkey::new_rand(), 42);
    genesis_config
        .accounts
        .remove(&feature_set::pico_inflation::id())
        .unwrap();
    genesis_config
        .accounts
        .remove(&feature_set::full_inflation::devnet_and_testnet::id())
        .unwrap();
    for pair in feature_set::FULL_INFLATION_FEATURE_PAIRS.iter() {
        genesis_config.accounts.remove(&pair.vote_id).unwrap();
        genesis_config.accounts.remove(&pair.enable_id).unwrap();
    }

    let bank = Bank::new_for_tests(&genesis_config);

    // Advance slot
    let mut bank = new_from_parent(Arc::new(bank));
    bank = new_from_parent(Arc::new(bank));
    assert_eq!(bank.get_inflation_start_slot(), 0);
    assert_eq!(bank.slot(), 2);

    // Request `pico_inflation` activation
    bank.store_account(
        &feature_set::pico_inflation::id(),
        &feature::create_account(
            &Feature {
                activated_at: Some(1),
            },
            42,
        ),
    );
    bank.feature_set = Arc::new(bank.compute_active_feature_set(true).0);
    assert_eq!(bank.get_inflation_start_slot(), 1);

    // Advance slot
    bank = new_from_parent(Arc::new(bank));
    assert_eq!(bank.slot(), 3);

    // Request `full_inflation::devnet_and_testnet` activation,
    // which takes priority over pico_inflation
    bank.store_account(
        &feature_set::full_inflation::devnet_and_testnet::id(),
        &feature::create_account(
            &Feature {
                activated_at: Some(2),
            },
            42,
        ),
    );
    bank.feature_set = Arc::new(bank.compute_active_feature_set(true).0);
    assert_eq!(bank.get_inflation_start_slot(), 2);

    // Request `full_inflation::mainnet::certusone` activation,
    // which should have no effect on `get_inflation_start_slot`
    bank.store_account(
        &feature_set::full_inflation::mainnet::certusone::vote::id(),
        &feature::create_account(
            &Feature {
                activated_at: Some(3),
            },
            42,
        ),
    );
    bank.store_account(
        &feature_set::full_inflation::mainnet::certusone::enable::id(),
        &feature::create_account(
            &Feature {
                activated_at: Some(3),
            },
            42,
        ),
    );
    bank.feature_set = Arc::new(bank.compute_active_feature_set(true).0);
    assert_eq!(bank.get_inflation_start_slot(), 2);
}

#[test]
fn test_get_inflation_start_slot_mainnet() {
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config_with_leader(42, &solana_pubkey::new_rand(), 42);
    genesis_config
        .accounts
        .remove(&feature_set::pico_inflation::id())
        .unwrap();
    genesis_config
        .accounts
        .remove(&feature_set::full_inflation::devnet_and_testnet::id())
        .unwrap();
    for pair in feature_set::FULL_INFLATION_FEATURE_PAIRS.iter() {
        genesis_config.accounts.remove(&pair.vote_id).unwrap();
        genesis_config.accounts.remove(&pair.enable_id).unwrap();
    }

    let bank = Bank::new_for_tests(&genesis_config);

    // Advance slot
    let mut bank = new_from_parent(Arc::new(bank));
    bank = new_from_parent(Arc::new(bank));
    assert_eq!(bank.get_inflation_start_slot(), 0);
    assert_eq!(bank.slot(), 2);

    // Request `pico_inflation` activation
    bank.store_account(
        &feature_set::pico_inflation::id(),
        &feature::create_account(
            &Feature {
                activated_at: Some(1),
            },
            42,
        ),
    );
    bank.feature_set = Arc::new(bank.compute_active_feature_set(true).0);
    assert_eq!(bank.get_inflation_start_slot(), 1);

    // Advance slot
    bank = new_from_parent(Arc::new(bank));
    assert_eq!(bank.slot(), 3);

    // Request `full_inflation::mainnet::certusone` activation,
    // which takes priority over pico_inflation
    bank.store_account(
        &feature_set::full_inflation::mainnet::certusone::vote::id(),
        &feature::create_account(
            &Feature {
                activated_at: Some(2),
            },
            42,
        ),
    );
    bank.store_account(
        &feature_set::full_inflation::mainnet::certusone::enable::id(),
        &feature::create_account(
            &Feature {
                activated_at: Some(2),
            },
            42,
        ),
    );
    bank.feature_set = Arc::new(bank.compute_active_feature_set(true).0);
    assert_eq!(bank.get_inflation_start_slot(), 2);

    // Advance slot
    bank = new_from_parent(Arc::new(bank));
    assert_eq!(bank.slot(), 4);

    // Request `full_inflation::devnet_and_testnet` activation,
    // which should have no effect on `get_inflation_start_slot`
    bank.store_account(
        &feature_set::full_inflation::devnet_and_testnet::id(),
        &feature::create_account(
            &Feature {
                activated_at: Some(bank.slot()),
            },
            42,
        ),
    );
    bank.feature_set = Arc::new(bank.compute_active_feature_set(true).0);
    assert_eq!(bank.get_inflation_start_slot(), 2);
}

#[test]
fn test_get_inflation_num_slots_with_activations() {
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config_with_leader(42, &solana_pubkey::new_rand(), 42);
    let slots_per_epoch = 32;
    genesis_config.epoch_schedule = EpochSchedule::new(slots_per_epoch);
    genesis_config
        .accounts
        .remove(&feature_set::pico_inflation::id())
        .unwrap();
    genesis_config
        .accounts
        .remove(&feature_set::full_inflation::devnet_and_testnet::id())
        .unwrap();
    for pair in feature_set::FULL_INFLATION_FEATURE_PAIRS.iter() {
        genesis_config.accounts.remove(&pair.vote_id).unwrap();
        genesis_config.accounts.remove(&pair.enable_id).unwrap();
    }

    let mut bank = Bank::new_for_tests(&genesis_config);
    assert_eq!(bank.get_inflation_num_slots(), 0);
    for _ in 0..2 * slots_per_epoch {
        bank = new_from_parent(Arc::new(bank));
    }
    assert_eq!(bank.get_inflation_num_slots(), 2 * slots_per_epoch);

    // Activate pico_inflation
    let pico_inflation_activation_slot = bank.slot();
    bank.store_account(
        &feature_set::pico_inflation::id(),
        &feature::create_account(
            &Feature {
                activated_at: Some(pico_inflation_activation_slot),
            },
            42,
        ),
    );
    bank.feature_set = Arc::new(bank.compute_active_feature_set(true).0);
    assert_eq!(bank.get_inflation_num_slots(), slots_per_epoch);
    for _ in 0..slots_per_epoch {
        bank = new_from_parent(Arc::new(bank));
    }
    assert_eq!(bank.get_inflation_num_slots(), 2 * slots_per_epoch);

    // Activate full_inflation::devnet_and_testnet
    let full_inflation_activation_slot = bank.slot();
    bank.store_account(
        &feature_set::full_inflation::devnet_and_testnet::id(),
        &feature::create_account(
            &Feature {
                activated_at: Some(full_inflation_activation_slot),
            },
            42,
        ),
    );
    bank.feature_set = Arc::new(bank.compute_active_feature_set(true).0);
    assert_eq!(bank.get_inflation_num_slots(), slots_per_epoch);
    for _ in 0..slots_per_epoch {
        bank = new_from_parent(Arc::new(bank));
    }
    assert_eq!(bank.get_inflation_num_slots(), 2 * slots_per_epoch);
}

#[test]
fn test_get_inflation_num_slots_already_activated() {
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config_with_leader(42, &solana_pubkey::new_rand(), 42);
    let slots_per_epoch = 32;
    genesis_config.epoch_schedule = EpochSchedule::new(slots_per_epoch);
    let mut bank = Bank::new_for_tests(&genesis_config);
    assert_eq!(bank.get_inflation_num_slots(), 0);
    for _ in 0..slots_per_epoch {
        bank = new_from_parent(Arc::new(bank));
    }
    assert_eq!(bank.get_inflation_num_slots(), slots_per_epoch);
    for _ in 0..slots_per_epoch {
        bank = new_from_parent(Arc::new(bank));
    }
    assert_eq!(bank.get_inflation_num_slots(), 2 * slots_per_epoch);
}

#[test]
fn test_stake_vote_account_validity() {
    let thread_pool = ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    // TODO: stakes cache should be hardened for the case when the account
    // owner is changed from vote/stake program to something else. see:
    // https://github.com/solana-labs/solana/pull/24200#discussion_r849935444
    check_stake_vote_account_validity(
        false, // check owner change
        |bank: &Bank| bank._load_vote_and_stake_accounts(&thread_pool, null_tracer()),
    );
}

#[test]
fn test_epoch_schedule_from_genesis_config() {
    let validator_vote_keypairs0 = ValidatorVoteKeypairs::new_rand();
    let validator_vote_keypairs1 = ValidatorVoteKeypairs::new_rand();
    let validator_keypairs = vec![&validator_vote_keypairs0, &validator_vote_keypairs1];
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config_with_vote_accounts(
        1_000_000_000,
        &validator_keypairs,
        vec![LAMPORTS_PER_SOL; 2],
    );

    genesis_config.epoch_schedule = EpochSchedule::custom(8192, 100, true);

    let bank = Arc::new(Bank::new_from_genesis(
        &genesis_config,
        Arc::<RuntimeConfig>::default(),
        Vec::new(),
        None,
        ACCOUNTS_DB_CONFIG_FOR_TESTING,
        None,
        None,
        Arc::default(),
        None,
        None,
    ));

    assert_eq!(bank.epoch_schedule(), &genesis_config.epoch_schedule);
}

fn check_stake_vote_account_validity<F>(check_owner_change: bool, load_vote_and_stake_accounts: F)
where
    F: Fn(&Bank) -> StakeDelegationsMap,
{
    let validator_vote_keypairs0 = ValidatorVoteKeypairs::new_rand();
    let validator_vote_keypairs1 = ValidatorVoteKeypairs::new_rand();
    let validator_keypairs = vec![&validator_vote_keypairs0, &validator_vote_keypairs1];
    let GenesisConfigInfo { genesis_config, .. } = create_genesis_config_with_vote_accounts(
        1_000_000_000,
        &validator_keypairs,
        vec![LAMPORTS_PER_SOL; 2],
    );
    let bank = Arc::new(Bank::new_from_genesis(
        &genesis_config,
        Arc::<RuntimeConfig>::default(),
        Vec::new(),
        None,
        ACCOUNTS_DB_CONFIG_FOR_TESTING,
        None,
        None,
        Arc::default(),
        None,
        None,
    ));
    let vote_and_stake_accounts = load_vote_and_stake_accounts(&bank);
    assert_eq!(vote_and_stake_accounts.len(), 2);

    let mut vote_account = bank
        .get_account(&validator_vote_keypairs0.vote_keypair.pubkey())
        .unwrap_or_default();
    let original_lamports = vote_account.lamports();
    vote_account.set_lamports(0);
    // Simulate vote account removal via full withdrawal
    bank.store_account(
        &validator_vote_keypairs0.vote_keypair.pubkey(),
        &vote_account,
    );

    // Modify staked vote account owner; a vote account owned by another program could be
    // freely modified with malicious data
    let bogus_vote_program = Pubkey::new_unique();
    vote_account.set_lamports(original_lamports);
    vote_account.set_owner(bogus_vote_program);
    bank.store_account(
        &validator_vote_keypairs0.vote_keypair.pubkey(),
        &vote_account,
    );

    assert_eq!(bank.vote_accounts().len(), 1);

    // Modify stake account owner; a stake account owned by another program could be freely
    // modified with malicious data
    let bogus_stake_program = Pubkey::new_unique();
    let mut stake_account = bank
        .get_account(&validator_vote_keypairs1.stake_keypair.pubkey())
        .unwrap_or_default();
    stake_account.set_owner(bogus_stake_program);
    bank.store_account(
        &validator_vote_keypairs1.stake_keypair.pubkey(),
        &stake_account,
    );

    // Accounts must be valid stake and vote accounts
    let vote_and_stake_accounts = load_vote_and_stake_accounts(&bank);
    assert_eq!(
        vote_and_stake_accounts.len(),
        usize::from(!check_owner_change)
    );
}

#[test]
fn test_vote_epoch_panic() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(
        1_000_000_000_000_000,
        &Pubkey::new_unique(),
        bootstrap_validator_stake_lamports(),
    );
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    let vote_keypair = keypair_from_seed(&[1u8; 32]).unwrap();

    let mut setup_ixs = Vec::new();
    setup_ixs.extend(vote_instruction::create_account_with_config(
        &mint_keypair.pubkey(),
        &vote_keypair.pubkey(),
        &VoteInit {
            node_pubkey: mint_keypair.pubkey(),
            authorized_voter: vote_keypair.pubkey(),
            authorized_withdrawer: mint_keypair.pubkey(),
            commission: 0,
        },
        1_000_000_000,
        vote_instruction::CreateVoteAccountConfig {
            space: VoteStateV4::size_of() as u64,
            ..vote_instruction::CreateVoteAccountConfig::default()
        },
    ));
    setup_ixs.push(vote_instruction::withdraw(
        &vote_keypair.pubkey(),
        &mint_keypair.pubkey(),
        1_000_000_000,
        &mint_keypair.pubkey(),
    ));
    setup_ixs.push(system_instruction::transfer(
        &mint_keypair.pubkey(),
        &vote_keypair.pubkey(),
        1_000_000_000,
    ));

    let result = bank.process_transaction(&Transaction::new(
        &[&mint_keypair, &vote_keypair],
        Message::new(&setup_ixs, Some(&mint_keypair.pubkey())),
        bank.last_blockhash(),
    ));
    assert!(result.is_ok());

    let _bank = Bank::new_from_parent_with_bank_forks(
        bank_forks.as_ref(),
        bank,
        &mint_keypair.pubkey(),
        genesis_config.epoch_schedule.get_first_slot_in_epoch(1),
    );
}

#[test_case(false; "old")]
#[test_case(true; "simd83")]
fn test_tx_log_order(relax_intrabatch_account_locks: bool) {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(
        1_000_000_000_000_000,
        &Pubkey::new_unique(),
        bootstrap_validator_stake_lamports(),
    );
    let mut bank = Bank::new_for_tests(&genesis_config);
    if !relax_intrabatch_account_locks {
        bank.deactivate_feature(&feature_set::relax_intrabatch_account_locks::id());
    }
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
    *bank.transaction_log_collector_config.write().unwrap() = TransactionLogCollectorConfig {
        mentioned_addresses: HashSet::new(),
        filter: TransactionLogCollectorFilter::All,
    };
    let blockhash = bank.last_blockhash();

    let sender0 = Keypair::new();
    let sender1 = Keypair::new();
    bank.transfer(100, &mint_keypair, &sender0.pubkey())
        .unwrap();
    bank.transfer(100, &mint_keypair, &sender1.pubkey())
        .unwrap();

    let recipient0 = Pubkey::new_unique();
    let recipient1 = Pubkey::new_unique();
    let tx0 = system_transaction::transfer(&sender0, &recipient0, 10, blockhash);
    let success_sig = tx0.signatures[0];
    let tx1 = system_transaction::transfer(&sender1, &recipient1, 110, blockhash); // Should produce insufficient funds log
    let failure_sig = tx1.signatures[0];
    let tx2 = system_transaction::transfer(&sender0, &recipient0, 1, blockhash);
    let txs = vec![tx0, tx1, tx2];
    let batch = bank.prepare_batch_for_tests(txs);

    let commit_results = bank
        .load_execute_and_commit_transactions(
            &batch,
            MAX_PROCESSING_AGE,
            ExecutionRecordingConfig {
                enable_cpi_recording: false,
                enable_log_recording: true,
                enable_return_data_recording: false,
                enable_transaction_balance_recording: false,
            },
            &mut ExecuteTimings::default(),
            None,
        )
        .0;

    assert_eq!(commit_results.len(), 3);

    assert!(commit_results[0].is_ok());
    assert!(commit_results[0]
        .as_ref()
        .unwrap()
        .log_messages
        .as_ref()
        .unwrap()[1]
        .contains(&"success".to_string()));
    assert!(commit_results[1].is_ok());
    assert!(commit_results[1]
        .as_ref()
        .unwrap()
        .log_messages
        .as_ref()
        .unwrap()[2]
        .contains(&"failed".to_string()));
    if relax_intrabatch_account_locks {
        assert!(commit_results[2].is_ok());
    } else {
        assert!(commit_results[2].is_err());
    }

    let stored_logs = &bank.transaction_log_collector.read().unwrap().logs;
    let success_log_info = stored_logs
        .iter()
        .find(|transaction_log_info| transaction_log_info.signature == success_sig)
        .unwrap();
    assert!(success_log_info.result.is_ok());
    let success_log = success_log_info.log_messages.clone().pop().unwrap();
    assert!(success_log.contains(&"success".to_string()));
    let failure_log_info = stored_logs
        .iter()
        .find(|transaction_log_info| transaction_log_info.signature == failure_sig)
        .unwrap();
    assert!(failure_log_info.result.is_err());
    let failure_log = failure_log_info.log_messages.clone().pop().unwrap();
    assert!(failure_log.contains(&"failed".to_string()));
}

#[test]
fn test_tx_return_data() {
    agave_logger::setup();
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(
        1_000_000_000_000_000,
        &Pubkey::new_unique(),
        bootstrap_validator_stake_lamports(),
    );
    let mock_program_id = Pubkey::from([2u8; 32]);
    let (bank, _bank_forks) =
        Bank::new_with_mockup_builtin_for_tests(&genesis_config, mock_program_id, MockBuiltin::vm);

    declare_process_instruction!(MockBuiltin, 1, |invoke_context| {
        let mock_program_id = Pubkey::from([2u8; 32]);
        let transaction_context = &mut invoke_context.transaction_context;
        let instruction_context = transaction_context.get_current_instruction_context()?;
        let instruction_data = instruction_context.get_instruction_data();
        let mut return_data = [0u8; MAX_RETURN_DATA];
        if !instruction_data.is_empty() {
            let index = usize::from_le_bytes(instruction_data.try_into().unwrap());
            return_data[index / 2] = 1;
            transaction_context
                .set_return_data(mock_program_id, return_data[..index + 1].to_vec())
                .unwrap();
        }
        Ok(())
    });

    let blockhash = bank.last_blockhash();

    for index in [
        None,
        Some(0),
        Some(MAX_RETURN_DATA / 2),
        Some(MAX_RETURN_DATA - 1),
    ] {
        let data = if let Some(index) = index {
            usize::to_le_bytes(index).to_vec()
        } else {
            Vec::new()
        };
        let txs = vec![Transaction::new_signed_with_payer(
            &[Instruction {
                program_id: mock_program_id,
                data,
                accounts: vec![AccountMeta::new(Pubkey::new_unique(), false)],
            }],
            Some(&mint_keypair.pubkey()),
            &[&mint_keypair],
            blockhash,
        )];
        let batch = bank.prepare_batch_for_tests(txs);
        let commit_results = bank
            .load_execute_and_commit_transactions(
                &batch,
                MAX_PROCESSING_AGE,
                ExecutionRecordingConfig {
                    enable_cpi_recording: false,
                    enable_log_recording: false,
                    enable_return_data_recording: true,
                    enable_transaction_balance_recording: false,
                },
                &mut ExecuteTimings::default(),
                None,
            )
            .0;
        let return_data = commit_results[0].as_ref().unwrap().return_data.clone();
        if let Some(index) = index {
            let return_data = return_data.unwrap();
            assert_eq!(return_data.program_id, mock_program_id);
            let mut expected_data = vec![0u8; index + 1];
            // include some trailing zeros
            expected_data[index / 2] = 1;
            assert_eq!(return_data.data, expected_data);
        } else {
            assert!(return_data.is_none());
        }
    }
}

#[test]
fn test_get_largest_accounts() {
    let GenesisConfigInfo { genesis_config, .. } =
        create_genesis_config_with_leader(42, &solana_pubkey::new_rand(), 42);
    let bank = Bank::new_for_tests(&genesis_config);

    let pubkeys: Vec<_> = (0..5).map(|_| Pubkey::new_unique()).collect();
    let pubkeys_hashset: HashSet<_> = pubkeys.iter().cloned().collect();

    let pubkeys_balances: Vec<_> = pubkeys
        .iter()
        .cloned()
        .zip(vec![
            2 * LAMPORTS_PER_SOL,
            3 * LAMPORTS_PER_SOL,
            3 * LAMPORTS_PER_SOL,
            4 * LAMPORTS_PER_SOL,
            5 * LAMPORTS_PER_SOL,
        ])
        .collect();

    // Initialize accounts; all have larger SOL balances than current Bank built-ins
    let account0 = AccountSharedData::new(pubkeys_balances[0].1, 0, &Pubkey::default());
    bank.store_account(&pubkeys_balances[0].0, &account0);
    let account1 = AccountSharedData::new(pubkeys_balances[1].1, 0, &Pubkey::default());
    bank.store_account(&pubkeys_balances[1].0, &account1);
    let account2 = AccountSharedData::new(pubkeys_balances[2].1, 0, &Pubkey::default());
    bank.store_account(&pubkeys_balances[2].0, &account2);
    let account3 = AccountSharedData::new(pubkeys_balances[3].1, 0, &Pubkey::default());
    bank.store_account(&pubkeys_balances[3].0, &account3);
    let account4 = AccountSharedData::new(pubkeys_balances[4].1, 0, &Pubkey::default());
    bank.store_account(&pubkeys_balances[4].0, &account4);

    // Create HashSet to exclude an account
    let exclude4: HashSet<_> = pubkeys[4..].iter().cloned().collect();

    let mut sorted_accounts = pubkeys_balances.clone();
    sorted_accounts.sort_by(|a, b| a.1.cmp(&b.1).reverse());

    // Return only one largest account
    assert_eq!(
        bank.get_largest_accounts(1, &pubkeys_hashset, AccountAddressFilter::Include, false)
            .unwrap(),
        vec![(pubkeys[4], 5 * LAMPORTS_PER_SOL)]
    );
    assert_eq!(
        bank.get_largest_accounts(1, &HashSet::new(), AccountAddressFilter::Exclude, false)
            .unwrap(),
        vec![(pubkeys[4], 5 * LAMPORTS_PER_SOL)]
    );
    assert_eq!(
        bank.get_largest_accounts(1, &exclude4, AccountAddressFilter::Exclude, false)
            .unwrap(),
        vec![(pubkeys[3], 4 * LAMPORTS_PER_SOL)]
    );

    // Return all added accounts
    let results = bank
        .get_largest_accounts(10, &pubkeys_hashset, AccountAddressFilter::Include, false)
        .unwrap();
    assert_eq!(results.len(), sorted_accounts.len());
    for pubkey_balance in sorted_accounts.iter() {
        assert!(results.contains(pubkey_balance));
    }
    let mut sorted_results = results.clone();
    sorted_results.sort_by(|a, b| a.1.cmp(&b.1).reverse());
    assert_eq!(sorted_results, results);

    let expected_accounts = sorted_accounts[1..].to_vec();
    let results = bank
        .get_largest_accounts(10, &exclude4, AccountAddressFilter::Exclude, false)
        .unwrap();
    // results include 5 Bank builtins
    assert_eq!(results.len(), 10);
    for pubkey_balance in expected_accounts.iter() {
        assert!(results.contains(pubkey_balance));
    }
    let mut sorted_results = results.clone();
    sorted_results.sort_by(|a, b| a.1.cmp(&b.1).reverse());
    assert_eq!(sorted_results, results);

    // Return 3 added accounts
    let expected_accounts = sorted_accounts[0..4].to_vec();
    let results = bank
        .get_largest_accounts(4, &pubkeys_hashset, AccountAddressFilter::Include, false)
        .unwrap();
    assert_eq!(results.len(), expected_accounts.len());
    for pubkey_balance in expected_accounts.iter() {
        assert!(results.contains(pubkey_balance));
    }

    let expected_accounts = expected_accounts[1..4].to_vec();
    let results = bank
        .get_largest_accounts(3, &exclude4, AccountAddressFilter::Exclude, false)
        .unwrap();
    assert_eq!(results.len(), expected_accounts.len());
    for pubkey_balance in expected_accounts.iter() {
        assert!(results.contains(pubkey_balance));
    }

    // Exclude more, and non-sequential, accounts
    let exclude: HashSet<_> = [pubkeys[0], pubkeys[2], pubkeys[4]]
        .iter()
        .cloned()
        .collect();
    assert_eq!(
        bank.get_largest_accounts(2, &exclude, AccountAddressFilter::Exclude, false)
            .unwrap(),
        vec![pubkeys_balances[3], pubkeys_balances[1]]
    );
}

#[test]
fn test_transfer_sysvar() {
    agave_logger::setup();
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(
        1_000_000_000_000_000,
        &Pubkey::new_unique(),
        bootstrap_validator_stake_lamports(),
    );
    let program_id = solana_pubkey::new_rand();

    let (bank, _bank_forks) =
        Bank::new_with_mockup_builtin_for_tests(&genesis_config, program_id, MockBuiltin::vm);

    declare_process_instruction!(MockBuiltin, 1, |invoke_context| {
        let transaction_context = &invoke_context.transaction_context;
        let instruction_context = transaction_context.get_current_instruction_context()?;
        instruction_context
            .try_borrow_instruction_account(1)?
            .set_data_from_slice(&[0; 40])?;
        Ok(())
    });

    let blockhash = bank.last_blockhash();
    #[allow(deprecated)]
    let blockhash_sysvar = sysvar::clock::id();
    #[allow(deprecated)]
    let orig_lamports = bank.get_account(&sysvar::clock::id()).unwrap().lamports();
    let tx = system_transaction::transfer(&mint_keypair, &blockhash_sysvar, 10, blockhash);
    assert_eq!(
        bank.process_transaction(&tx),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::ReadonlyLamportChange
        ))
    );
    assert_eq!(
        bank.get_account(&sysvar::clock::id()).unwrap().lamports(),
        orig_lamports
    );

    let accounts = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new(blockhash_sysvar, false),
    ];
    let ix = Instruction::new_with_bincode(program_id, &0, accounts);
    let message = Message::new(&[ix], Some(&mint_keypair.pubkey()));
    let tx = Transaction::new(&[&mint_keypair], message, blockhash);
    assert_eq!(
        bank.process_transaction(&tx),
        Err(TransactionError::InstructionError(
            0,
            InstructionError::ReadonlyDataModified
        ))
    );
}

#[test]
fn test_clean_dropped_unrooted_frozen_banks() {
    agave_logger::setup();
    do_test_clean_dropped_unrooted_banks(FreezeBank1::Yes);
}

#[test]
fn test_clean_dropped_unrooted_unfrozen_banks() {
    agave_logger::setup();
    do_test_clean_dropped_unrooted_banks(FreezeBank1::No);
}

/// A simple enum to toggle freezing Bank1 or not.  Used in the clean_dropped_unrooted tests.
enum FreezeBank1 {
    No,
    Yes,
}

fn do_test_clean_dropped_unrooted_banks(freeze_bank1: FreezeBank1) {
    //! Test that dropped unrooted banks are cleaned up properly
    //!
    //! slot 0:       bank0 (rooted)
    //!               /   \
    //! slot 1:      /   bank1 (unrooted and dropped)
    //!             /
    //! slot 2:  bank2 (rooted)
    //!
    //! In the scenario above, when `clean_accounts()` is called on bank2, the keys that exist
    //! _only_ in bank1 should be cleaned up, since those keys are unreachable.
    //!
    //! The following scenarios are tested:
    //!
    //! 1. A key is written _only_ in an unrooted bank (key1)
    //!     - In this case, key1 should be cleaned up
    //! 2. A key is written in both an unrooted _and_ rooted bank (key3)
    //!     - In this case, key3's ref-count should be decremented correctly
    //! 3. A key with zero lamports is _only_ in an unrooted bank (key4)
    //!     - In this case, key4 should be cleaned up
    //! 4. A key with zero lamports is in both an unrooted _and_ rooted bank (key5)
    //!     - In this case, key5's ref-count should be decremented correctly

    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let (bank0, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let amount = genesis_config.rent.minimum_balance(0);

    let collector = Pubkey::new_unique();
    let owner = Pubkey::new_unique();

    let key1 = Keypair::new(); // only touched in bank1
    let key2 = Keypair::new(); // only touched in bank2
    let key3 = Keypair::new(); // touched in both bank1 and bank2
    let key4 = Keypair::new(); // in only bank1, and has zero lamports
    let key5 = Keypair::new(); // in both bank1 and bank2, and has zero lamports
    bank0
        .transfer(amount, &mint_keypair, &key2.pubkey())
        .unwrap();
    bank0.freeze();

    let slot = 1;
    let bank1 =
        Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank0.clone(), &collector, slot);
    add_root_and_flush_write_cache(&bank0);
    bank1
        .transfer(amount, &mint_keypair, &key1.pubkey())
        .unwrap();
    bank1.store_account(&key4.pubkey(), &AccountSharedData::new(0, 0, &owner));
    bank1.store_account(&key5.pubkey(), &AccountSharedData::new(0, 0, &owner));

    if let FreezeBank1::Yes = freeze_bank1 {
        bank1.freeze();
    }

    let slot = slot + 1;
    let bank2 = Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank0, &collector, slot);
    bank2
        .transfer(amount * 2, &mint_keypair, &key2.pubkey())
        .unwrap();
    bank2
        .transfer(amount, &mint_keypair, &key3.pubkey())
        .unwrap();
    bank2.store_account(&key5.pubkey(), &AccountSharedData::new(0, 0, &owner));

    bank2.freeze(); // the freeze here is not strictly necessary, but more for illustration
    bank2.squash();
    add_root_and_flush_write_cache(&bank2);

    bank_forks.write().unwrap().remove(1);
    drop(bank1);
    bank2.clean_accounts_for_tests();

    let expected_ref_count_for_cleaned_up_keys = 0;
    let expected_ref_count_for_keys_in_both_slot1_and_slot2 = 1;

    bank2
        .rc
        .accounts
        .accounts_db
        .assert_ref_count(&key1.pubkey(), expected_ref_count_for_cleaned_up_keys);
    bank2.rc.accounts.accounts_db.assert_ref_count(
        &key3.pubkey(),
        expected_ref_count_for_keys_in_both_slot1_and_slot2,
    );
    bank2
        .rc
        .accounts
        .accounts_db
        .assert_ref_count(&key4.pubkey(), expected_ref_count_for_cleaned_up_keys);
    bank2.rc.accounts.accounts_db.assert_ref_count(
        &key5.pubkey(),
        expected_ref_count_for_keys_in_both_slot1_and_slot2,
    );
    assert_eq!(
        bank2.rc.accounts.accounts_db.alive_account_count_in_slot(1),
        0
    );
}

#[test]
fn test_compute_budget_program_noop() {
    agave_logger::setup();
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(
        1_000_000_000_000_000,
        &Pubkey::new_unique(),
        bootstrap_validator_stake_lamports(),
    );
    let program_id = solana_pubkey::new_rand();
    let (bank, _bank_forks) =
        Bank::new_with_mockup_builtin_for_tests(&genesis_config, program_id, MockBuiltin::vm);

    declare_process_instruction!(MockBuiltin, 1, |invoke_context| {
        let compute_budget = ComputeBudget::from_budget_and_cost(
            invoke_context.get_compute_budget(),
            invoke_context.get_execution_cost(),
        );
        assert_eq!(
            compute_budget,
            ComputeBudget {
                compute_unit_limit: u64::from(
                    execution_budget::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT
                ),
                heap_size: 48 * 1024,
                ..ComputeBudget::new_with_defaults(
                    invoke_context
                        .get_feature_set()
                        .raise_cpi_nesting_limit_to_8,
                    invoke_context
                        .get_feature_set()
                        .increase_cpi_account_info_limit
                )
            }
        );
        Ok(())
    });

    let message = Message::new(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(
                execution_budget::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
            ),
            ComputeBudgetInstruction::request_heap_frame(48 * 1024),
            Instruction::new_with_bincode(program_id, &0, vec![]),
        ],
        Some(&mint_keypair.pubkey()),
    );
    let tx = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());
    bank.process_transaction(&tx).unwrap();
}

#[test]
fn test_compute_request_instruction() {
    agave_logger::setup();
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(
        1_000_000_000_000_000,
        &Pubkey::new_unique(),
        bootstrap_validator_stake_lamports(),
    );
    let program_id = solana_pubkey::new_rand();
    let (bank, _bank_forks) =
        Bank::new_with_mockup_builtin_for_tests(&genesis_config, program_id, MockBuiltin::vm);

    declare_process_instruction!(MockBuiltin, 1, |invoke_context| {
        let compute_budget = ComputeBudget::from_budget_and_cost(
            invoke_context.get_compute_budget(),
            invoke_context.get_execution_cost(),
        );
        assert_eq!(
            compute_budget,
            ComputeBudget {
                compute_unit_limit: u64::from(
                    execution_budget::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT
                ),
                heap_size: 48 * 1024,
                ..ComputeBudget::new_with_defaults(
                    invoke_context
                        .get_feature_set()
                        .raise_cpi_nesting_limit_to_8,
                    invoke_context
                        .get_feature_set()
                        .increase_cpi_account_info_limit
                )
            }
        );
        Ok(())
    });

    let message = Message::new(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(
                execution_budget::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
            ),
            ComputeBudgetInstruction::request_heap_frame(48 * 1024),
            Instruction::new_with_bincode(program_id, &0, vec![]),
        ],
        Some(&mint_keypair.pubkey()),
    );
    let tx = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());
    bank.process_transaction(&tx).unwrap();
}

#[test]
fn test_failed_compute_request_instruction() {
    agave_logger::setup();
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(
        1_000_000_000_000_000,
        &Pubkey::new_unique(),
        bootstrap_validator_stake_lamports(),
    );

    let program_id = solana_pubkey::new_rand();
    let (bank, _bank_forks) =
        Bank::new_with_mockup_builtin_for_tests(&genesis_config, program_id, MockBuiltin::vm);

    let payer0_keypair = Keypair::new();
    let payer1_keypair = Keypair::new();
    bank.transfer(10, &mint_keypair, &payer0_keypair.pubkey())
        .unwrap();
    bank.transfer(10, &mint_keypair, &payer1_keypair.pubkey())
        .unwrap();

    const TEST_COMPUTE_UNIT_LIMIT: u32 = 500u32;
    declare_process_instruction!(MockBuiltin, 1, |invoke_context| {
        let compute_budget = ComputeBudget::from_budget_and_cost(
            invoke_context.get_compute_budget(),
            invoke_context.get_execution_cost(),
        );
        assert_eq!(
            compute_budget,
            ComputeBudget {
                compute_unit_limit: u64::from(TEST_COMPUTE_UNIT_LIMIT),
                heap_size: 48 * 1024,
                ..ComputeBudget::new_with_defaults(
                    invoke_context
                        .get_feature_set()
                        .raise_cpi_nesting_limit_to_8,
                    invoke_context
                        .get_feature_set()
                        .increase_cpi_account_info_limit
                )
            }
        );
        Ok(())
    });

    // This message will not be executed because the compute budget request is invalid
    let message0 = Message::new(
        &[
            ComputeBudgetInstruction::request_heap_frame(1),
            Instruction::new_with_bincode(program_id, &0, vec![]),
        ],
        Some(&payer0_keypair.pubkey()),
    );
    // This message will be processed successfully
    let message1 = Message::new(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(TEST_COMPUTE_UNIT_LIMIT),
            ComputeBudgetInstruction::request_heap_frame(48 * 1024),
            Instruction::new_with_bincode(program_id, &0, vec![]),
        ],
        Some(&payer1_keypair.pubkey()),
    );
    let txs = [
        Transaction::new(&[&payer0_keypair], message0, bank.last_blockhash()),
        Transaction::new(&[&payer1_keypair], message1, bank.last_blockhash()),
    ];
    let results = bank.process_transactions(txs.iter());

    assert_eq!(
        results[0],
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidInstructionData
        ))
    );
    assert_eq!(results[1], Ok(()));
    // two transfers and the mock program
    assert_eq!(bank.signature_count(), 3);
}

#[test]
fn test_verify_and_hash_transaction_sig_len() {
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config_with_leader(42, &solana_pubkey::new_rand(), 42);

    // activate all features
    activate_all_features(&mut genesis_config);
    let bank = Bank::new_for_tests(&genesis_config);

    let recent_blockhash = Hash::new_unique();
    let from_keypair = Keypair::new();
    let to_keypair = Keypair::new();
    let from_pubkey = from_keypair.pubkey();
    let to_pubkey = to_keypair.pubkey();

    enum TestCase {
        AddSignature,
        RemoveSignature,
    }

    let make_transaction = |case: TestCase| {
        let message = Message::new(
            &[system_instruction::transfer(&from_pubkey, &to_pubkey, 1)],
            Some(&from_pubkey),
        );
        let mut tx = Transaction::new(&[&from_keypair], message, recent_blockhash);
        assert_eq!(tx.message.header.num_required_signatures, 1);
        match case {
            TestCase::AddSignature => {
                let signature = to_keypair.sign_message(&tx.message.serialize());
                tx.signatures.push(signature);
            }
            TestCase::RemoveSignature => {
                tx.signatures.remove(0);
            }
        }
        tx
    };

    // Too few signatures: Sanitization failure
    {
        let tx = make_transaction(TestCase::RemoveSignature);
        assert_matches!(
            bank.verify_transaction(tx.into(), TransactionVerificationMode::FullVerification),
            Err(TransactionError::SanitizeFailure)
        );
    }
    // Too many signatures: Sanitization failure
    {
        let tx = make_transaction(TestCase::AddSignature);
        assert_matches!(
            bank.verify_transaction(tx.into(), TransactionVerificationMode::FullVerification),
            Err(TransactionError::SanitizeFailure)
        );
    }
}

#[test]
fn test_verify_transactions_packet_data_size() {
    let GenesisConfigInfo { genesis_config, .. } =
        create_genesis_config_with_leader(42, &solana_pubkey::new_rand(), 42);
    let bank = Bank::new_for_tests(&genesis_config);

    let recent_blockhash = Hash::new_unique();
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let make_transaction = |size| {
        let ixs: Vec<_> = std::iter::repeat_with(|| {
            system_instruction::transfer(&pubkey, &Pubkey::new_unique(), 1)
        })
        .take(size)
        .collect();
        let message = Message::new(&ixs[..], Some(&pubkey));
        Transaction::new(&[&keypair], message, recent_blockhash)
    };
    // Small transaction.
    {
        let tx = make_transaction(5);
        assert!(bincode::serialized_size(&tx).unwrap() <= PACKET_DATA_SIZE as u64);
        assert!(bank
            .verify_transaction(tx.into(), TransactionVerificationMode::FullVerification)
            .is_ok(),);
    }
    // Big transaction.
    {
        let tx = make_transaction(25);
        assert!(bincode::serialized_size(&tx).unwrap() > PACKET_DATA_SIZE as u64);
        assert_matches!(
            bank.verify_transaction(tx.into(), TransactionVerificationMode::FullVerification),
            Err(TransactionError::SanitizeFailure)
        );
    }
    // Assert that verify fails as soon as serialized
    // size exceeds packet data size.
    for size in 1..30 {
        let tx = make_transaction(size);
        assert_eq!(
            bincode::serialized_size(&tx).unwrap() <= PACKET_DATA_SIZE as u64,
            bank.verify_transaction(tx.into(), TransactionVerificationMode::FullVerification)
                .is_ok(),
        );
    }
}

#[test_case(false; "pre_simd160_static_instruction_limit")]
#[test_case(true; "simd160_static_instruction_limit")]
fn test_verify_transactions_instruction_limit(simd_0160_enabled: bool) {
    let GenesisConfigInfo { genesis_config, .. } =
        create_genesis_config_with_leader(42, &solana_pubkey::new_rand(), 42);
    let mut bank = Bank::new_for_tests(&genesis_config);
    if !simd_0160_enabled {
        bank.deactivate_feature(&feature_set::static_instruction_limit::id());
    }

    let recent_blockhash = Hash::new_unique();
    let keypair = Keypair::new();
    let pubkey = keypair.pubkey();
    let ix_count = 65;
    let ixs: Vec<_> = std::iter::repeat_with(|| CompiledInstruction {
        program_id_index: 1,
        accounts: vec![0],
        data: vec![],
    })
    .take(ix_count)
    .collect();
    let message = Message::new_with_compiled_instructions(
        1,
        0,
        1,
        vec![pubkey, Pubkey::new_unique()],
        recent_blockhash,
        ixs,
    );
    let tx = Transaction::new(&[&keypair], message, recent_blockhash);

    assert!(bincode::serialized_size(&tx).unwrap() <= PACKET_DATA_SIZE as u64);

    if simd_0160_enabled {
        assert_matches!(
            bank.verify_transaction(tx.into(), TransactionVerificationMode::FullVerification),
            Err(TransactionError::SanitizeFailure)
        );
    } else {
        assert!(bank
            .verify_transaction(tx.into(), TransactionVerificationMode::FullVerification)
            .is_ok());
    }
}

#[test]
fn test_check_reserved_keys() {
    let (genesis_config, _mint_keypair) = create_genesis_config(1);
    let bank = Bank::new_for_tests(&genesis_config);
    let mut bank = Bank::new_from_parent(Arc::new(bank), &Pubkey::new_unique(), 1);

    let transaction =
        SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
            &Keypair::new(),
            &Pubkey::new_unique(),
            1,
            genesis_config.hash(),
        ));

    assert_eq!(bank.check_reserved_keys(&transaction), Ok(()));

    Arc::make_mut(&mut bank.reserved_account_keys)
        .active
        .insert(transaction.account_keys()[1]);
    assert_eq!(
        bank.check_reserved_keys(&transaction),
        Err(TransactionError::ResanitizationNeeded)
    );
}

#[test]
fn test_call_precomiled_program() {
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(42, &Pubkey::new_unique(), 42);
    activate_all_features(&mut genesis_config);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    // libsecp256k1
    // Since libsecp256k1 is still using the old version of rand, this test
    // copies the `random` implementation at:
    // https://docs.rs/libsecp256k1/latest/src/libsecp256k1/lib.rs.html#430
    let secp_privkey = {
        use rand::RngCore;
        let mut rng = rand::rng();
        loop {
            let mut ret = [0u8; libsecp256k1::util::SECRET_KEY_SIZE];
            rng.fill_bytes(&mut ret);
            if let Ok(key) = libsecp256k1::SecretKey::parse(&ret) {
                break key;
            }
        }
    };
    let message_arr = b"hello";
    let pubkey = libsecp256k1::PublicKey::from_secret_key(&secp_privkey);
    let eth_address = solana_secp256k1_program::eth_address_from_pubkey(
        &pubkey.serialize()[1..].try_into().unwrap(),
    );
    let (signature, recovery_id) =
        solana_secp256k1_program::sign_message(&secp_privkey.serialize(), &message_arr[..])
            .unwrap();
    let instruction = solana_secp256k1_program::new_secp256k1_instruction_with_signature(
        &message_arr[..],
        &signature,
        recovery_id,
        &eth_address,
    );

    let tx = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair],
        bank.last_blockhash(),
    );
    // calling the program should be successful when called from the bank
    // even if the program itself is not called
    bank.process_transaction(&tx).unwrap();

    // ed25519
    // Since ed25519_dalek is still using the old version of rand, this test
    // copies the `generate` implementation at:
    // https://docs.rs/ed25519-dalek/1.0.1/src/ed25519_dalek/secret.rs.html#167
    let privkey = {
        use rand::RngCore;
        let mut rng = rand::rng();
        let mut seed = [0u8; ed25519_dalek::SECRET_KEY_LENGTH];
        rng.fill_bytes(&mut seed);
        let secret =
            ed25519_dalek::SecretKey::from_bytes(&seed[..ed25519_dalek::SECRET_KEY_LENGTH])
                .unwrap();
        let public = ed25519_dalek::PublicKey::from(&secret);
        ed25519_dalek::Keypair { secret, public }
    };
    let message_arr = b"hello";
    let signature = privkey.sign(message_arr).to_bytes();
    let pubkey = privkey.public.to_bytes();
    let instruction = solana_ed25519_program::new_ed25519_instruction_with_signature(
        message_arr,
        &signature,
        &pubkey,
    );
    let tx = Transaction::new_signed_with_payer(
        &[instruction],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair],
        bank.last_blockhash(),
    );
    // calling the program should be successful when called from the bank
    // even if the program itself is not called
    bank.process_transaction(&tx).unwrap();
}

fn calculate_test_fee(
    message: &impl SVMMessage,
    lamports_per_signature: u64,
    fee_structure: &FeeStructure,
) -> u64 {
    let fee_budget_limits = FeeBudgetLimits::from(
        process_compute_budget_instructions(
            message.program_instructions_iter(),
            &FeatureSet::default(),
        )
        .unwrap_or_default(),
    );
    solana_fee::calculate_fee(
        message,
        lamports_per_signature == 0,
        fee_structure.lamports_per_signature,
        fee_budget_limits.prioritization_fee,
        FeeFeatures {
            enable_secp256r1_precompile: true,
        },
    )
}

#[test]
fn test_calculate_fee() {
    // Default: no fee.
    let message = new_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique())));
    assert_eq!(
        calculate_test_fee(
            &message,
            0,
            &FeeStructure {
                lamports_per_signature: 0,
                ..FeeStructure::default()
            },
        ),
        0
    );

    // One signature, a fee.
    assert_eq!(
        calculate_test_fee(
            &message,
            1,
            &FeeStructure {
                lamports_per_signature: 1,
                ..FeeStructure::default()
            },
        ),
        1
    );

    // Two signatures, double the fee.
    let key0 = Pubkey::new_unique();
    let key1 = Pubkey::new_unique();
    let ix0 = system_instruction::transfer(&key0, &key1, 1);
    let ix1 = system_instruction::transfer(&key1, &key0, 1);
    let message = new_sanitized_message(Message::new(&[ix0, ix1], Some(&key0)));
    assert_eq!(
        calculate_test_fee(
            &message,
            2,
            &FeeStructure {
                lamports_per_signature: 2,
                ..FeeStructure::default()
            },
        ),
        4
    );
}

#[test]
fn test_calculate_fee_compute_units() {
    let fee_structure = FeeStructure {
        lamports_per_signature: 1,
        ..FeeStructure::default()
    };
    let max_fee = fee_structure.compute_fee_bins.last().unwrap().fee;
    let lamports_per_signature = fee_structure.lamports_per_signature;

    // One signature, no unit request

    let message = new_sanitized_message(Message::new(&[], Some(&Pubkey::new_unique())));
    assert_eq!(
        calculate_test_fee(&message, 1, &fee_structure,),
        max_fee + lamports_per_signature
    );

    // Three signatures, two instructions, no unit request

    let ix0 = system_instruction::transfer(&Pubkey::new_unique(), &Pubkey::new_unique(), 1);
    let ix1 = system_instruction::transfer(&Pubkey::new_unique(), &Pubkey::new_unique(), 1);
    let message = new_sanitized_message(Message::new(&[ix0, ix1], Some(&Pubkey::new_unique())));
    assert_eq!(
        calculate_test_fee(&message, 1, &fee_structure,),
        max_fee + 3 * lamports_per_signature
    );

    // Explicit fee schedule

    for requested_compute_units in [
        0,
        5_000,
        10_000,
        100_000,
        300_000,
        500_000,
        700_000,
        900_000,
        1_100_000,
        1_300_000,
        MAX_COMPUTE_UNIT_LIMIT,
    ] {
        const PRIORITIZATION_FEE_RATE: u64 = 42;
        let message = new_sanitized_message(Message::new(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(requested_compute_units),
                ComputeBudgetInstruction::set_compute_unit_price(PRIORITIZATION_FEE_RATE),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
            ],
            Some(&Pubkey::new_unique()),
        ));
        let fee = calculate_test_fee(&message, 1, &fee_structure);
        let fee_budget_limits = FeeBudgetLimits::from(ComputeBudgetLimits {
            compute_unit_price: PRIORITIZATION_FEE_RATE,
            compute_unit_limit: requested_compute_units,
            ..ComputeBudgetLimits::default()
        });
        assert_eq!(
            fee,
            lamports_per_signature + fee_budget_limits.prioritization_fee
        );
    }
}

#[test]
fn test_calculate_prioritization_fee() {
    let fee_structure = FeeStructure {
        lamports_per_signature: 1,
        ..FeeStructure::default()
    };

    let request_units = 1_000_000_u32;
    let request_unit_price = 2_000_000_000_u64;
    let fee_budget_limits = FeeBudgetLimits::from(ComputeBudgetLimits {
        compute_unit_price: request_unit_price,
        compute_unit_limit: request_units,
        ..ComputeBudgetLimits::default()
    });

    let message = new_sanitized_message(Message::new(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(request_units),
            ComputeBudgetInstruction::set_compute_unit_price(request_unit_price),
        ],
        Some(&Pubkey::new_unique()),
    ));

    let fee = calculate_test_fee(
        &message,
        fee_structure.lamports_per_signature,
        &fee_structure,
    );
    assert_eq!(
        fee,
        fee_structure.lamports_per_signature + fee_budget_limits.prioritization_fee
    );
}

#[test]
fn test_calculate_fee_secp256k1() {
    let fee_structure = FeeStructure {
        lamports_per_signature: 1,
        ..FeeStructure::default()
    };
    let key0 = Pubkey::new_unique();
    let key1 = Pubkey::new_unique();
    let ix0 = system_instruction::transfer(&key0, &key1, 1);

    let mut secp_instruction1 = Instruction {
        program_id: secp256k1_program::id(),
        accounts: vec![],
        data: vec![],
    };
    let mut secp_instruction2 = Instruction {
        program_id: secp256k1_program::id(),
        accounts: vec![],
        data: vec![1],
    };

    let message = new_sanitized_message(Message::new(
        &[
            ix0.clone(),
            secp_instruction1.clone(),
            secp_instruction2.clone(),
        ],
        Some(&key0),
    ));
    assert_eq!(calculate_test_fee(&message, 1, &fee_structure,), 2);

    secp_instruction1.data = vec![0];
    secp_instruction2.data = vec![10];
    let message = new_sanitized_message(Message::new(
        &[ix0, secp_instruction1, secp_instruction2],
        Some(&key0),
    ));
    assert_eq!(calculate_test_fee(&message, 1, &fee_structure,), 11);
}

#[test]
fn test_an_empty_instruction_without_program() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee_no_rent(1);
    let destination = solana_pubkey::new_rand();
    let mut ix = system_instruction::transfer(&mint_keypair.pubkey(), &destination, 0);
    ix.program_id = native_loader::id(); // Empty executable account chain
    let message = Message::new(&[ix], Some(&mint_keypair.pubkey()));
    let tx = Transaction::new(&[&mint_keypair], message, genesis_config.hash());

    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();

    let err = bank.process_transaction(&tx).unwrap_err();
    assert_eq!(err, TransactionError::ProgramAccountNotFound);
}

#[test]
fn test_transaction_log_collector_get_logs_for_address() {
    let address = Pubkey::new_unique();
    let mut mentioned_address_map = HashMap::new();
    mentioned_address_map.insert(address, vec![0]);
    let transaction_log_collector = TransactionLogCollector {
        mentioned_address_map,
        ..TransactionLogCollector::default()
    };
    assert_eq!(
        transaction_log_collector.get_logs_for_address(Some(&address)),
        Some(Vec::<TransactionLogInfo>::new()),
    );
}

/// Test processing a good transaction correctly modifies the accounts data size
#[test]
fn test_accounts_data_size_with_good_transaction() {
    const ACCOUNT_SIZE: u64 = MAX_PERMITTED_DATA_LENGTH;
    let (genesis_config, mint_keypair) = create_genesis_config(1_000 * LAMPORTS_PER_SOL);
    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let transaction = system_transaction::create_account(
        &mint_keypair,
        &Keypair::new(),
        bank.last_blockhash(),
        genesis_config
            .rent
            .minimum_balance(ACCOUNT_SIZE.try_into().unwrap()),
        ACCOUNT_SIZE,
        &solana_system_interface::program::id(),
    );

    let accounts_data_size_before = bank.load_accounts_data_size();
    let accounts_data_size_delta_before = bank.load_accounts_data_size_delta();
    let accounts_data_size_delta_on_chain_before = bank.load_accounts_data_size_delta_on_chain();
    let result = bank.process_transaction(&transaction);
    let accounts_data_size_after = bank.load_accounts_data_size();
    let accounts_data_size_delta_after = bank.load_accounts_data_size_delta();
    let accounts_data_size_delta_on_chain_after = bank.load_accounts_data_size_delta_on_chain();

    assert!(result.is_ok());
    assert_eq!(
        accounts_data_size_after - accounts_data_size_before,
        ACCOUNT_SIZE,
    );
    assert_eq!(
        accounts_data_size_delta_after - accounts_data_size_delta_before,
        ACCOUNT_SIZE as i64,
    );
    assert_eq!(
        accounts_data_size_delta_on_chain_after - accounts_data_size_delta_on_chain_before,
        ACCOUNT_SIZE as i64,
    );
}

/// Test processing a bad transaction correctly modifies the accounts data size
#[test]
fn test_accounts_data_size_with_bad_transaction() {
    const ACCOUNT_SIZE: u64 = MAX_PERMITTED_DATA_LENGTH;
    let (genesis_config, _mint_keypair) = create_genesis_config(1_000 * LAMPORTS_PER_SOL);
    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let transaction = system_transaction::create_account(
        &Keypair::new(),
        &Keypair::new(),
        bank.last_blockhash(),
        LAMPORTS_PER_SOL,
        ACCOUNT_SIZE,
        &solana_system_interface::program::id(),
    );

    let accounts_data_size_before = bank.load_accounts_data_size();
    let accounts_data_size_delta_before = bank.load_accounts_data_size_delta();
    let accounts_data_size_delta_on_chain_before = bank.load_accounts_data_size_delta_on_chain();
    let result = bank.process_transaction(&transaction);
    let accounts_data_size_after = bank.load_accounts_data_size();
    let accounts_data_size_delta_after = bank.load_accounts_data_size_delta();
    let accounts_data_size_delta_on_chain_after = bank.load_accounts_data_size_delta_on_chain();

    assert!(result.is_err());
    assert_eq!(accounts_data_size_after, accounts_data_size_before,);
    assert_eq!(
        accounts_data_size_delta_after,
        accounts_data_size_delta_before,
    );
    assert_eq!(
        accounts_data_size_delta_on_chain_after,
        accounts_data_size_delta_on_chain_before,
    );
}

#[derive(Serialize, Deserialize)]
enum MockTransferInstruction {
    Transfer(u64),
}

declare_process_instruction!(MockTransferBuiltin, 1, |invoke_context| {
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let instruction_data = instruction_context.get_instruction_data();
    if let Ok(instruction) = bincode::deserialize(instruction_data) {
        match instruction {
            MockTransferInstruction::Transfer(amount) => {
                instruction_context
                    .try_borrow_instruction_account(1)?
                    .checked_sub_lamports(amount)?;
                instruction_context
                    .try_borrow_instruction_account(2)?
                    .checked_add_lamports(amount)?;
                Ok(())
            }
        }
    } else {
        Err(InstructionError::InvalidInstructionData)
    }
});

fn create_mock_transfer(
    payer: &Keypair,
    from: &Keypair,
    to: &Keypair,
    amount: u64,
    mock_program_id: Pubkey,
    recent_blockhash: Hash,
) -> Transaction {
    let account_metas = vec![
        AccountMeta::new(payer.pubkey(), true),
        AccountMeta::new(from.pubkey(), true),
        AccountMeta::new(to.pubkey(), true),
    ];
    let transfer_instruction = Instruction::new_with_bincode(
        mock_program_id,
        &MockTransferInstruction::Transfer(amount),
        account_metas,
    );
    Transaction::new_signed_with_payer(
        &[transfer_instruction],
        Some(&payer.pubkey()),
        &[payer, from, to],
        recent_blockhash,
    )
}

#[test]
fn test_invalid_rent_state_changes_existing_accounts() {
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(100 * LAMPORTS_PER_SOL, &Pubkey::new_unique(), 42);
    genesis_config.rent = Rent::default();

    let mock_program_id = Pubkey::new_unique();
    let account_data_size = 100;
    let rent_exempt_minimum = genesis_config.rent.minimum_balance(account_data_size);

    // Create legacy accounts of various kinds
    let rent_paying_account = Keypair::new();
    genesis_config.accounts.insert(
        rent_paying_account.pubkey(),
        Account::new_rent_epoch(
            rent_exempt_minimum - 1,
            account_data_size,
            &mock_program_id,
            INITIAL_RENT_EPOCH + 1,
        ),
    );
    let rent_exempt_account = Keypair::new();
    genesis_config.accounts.insert(
        rent_exempt_account.pubkey(),
        Account::new_rent_epoch(
            rent_exempt_minimum,
            account_data_size,
            &mock_program_id,
            INITIAL_RENT_EPOCH + 1,
        ),
    );

    let (bank, _bank_forks) = Bank::new_with_mockup_builtin_for_tests(
        &genesis_config,
        mock_program_id,
        MockTransferBuiltin::vm,
    );
    let recent_blockhash = bank.last_blockhash();

    let check_account_is_rent_exempt = |pubkey: &Pubkey| -> bool {
        let account = bank.get_account(pubkey).unwrap();
        Rent::default().is_exempt(account.lamports(), account.data().len())
    };

    // RentPaying account can be left as Uninitialized, in other RentPaying states, or RentExempt
    let tx = create_mock_transfer(
        &mint_keypair,        // payer
        &rent_paying_account, // from
        &mint_keypair,        // to
        1,
        mock_program_id,
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert!(!check_account_is_rent_exempt(&rent_paying_account.pubkey()));
    let tx = create_mock_transfer(
        &mint_keypair,        // payer
        &rent_paying_account, // from
        &mint_keypair,        // to
        rent_exempt_minimum - 2,
        mock_program_id,
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert!(bank.get_account(&rent_paying_account.pubkey()).is_none());

    bank.store_account(
        // restore program-owned account
        &rent_paying_account.pubkey(),
        &AccountSharedData::new(rent_exempt_minimum - 1, account_data_size, &mock_program_id),
    );
    let result = bank.transfer(1, &mint_keypair, &rent_paying_account.pubkey());
    assert!(result.is_ok());
    assert!(check_account_is_rent_exempt(&rent_paying_account.pubkey()));

    // RentExempt account can only remain RentExempt or be Uninitialized
    let tx = create_mock_transfer(
        &mint_keypair,        // payer
        &rent_exempt_account, // from
        &mint_keypair,        // to
        1,
        mock_program_id,
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_err());
    assert!(check_account_is_rent_exempt(&rent_exempt_account.pubkey()));
    let result = bank.transfer(1, &mint_keypair, &rent_exempt_account.pubkey());
    assert!(result.is_ok());
    assert!(check_account_is_rent_exempt(&rent_exempt_account.pubkey()));
    let tx = create_mock_transfer(
        &mint_keypair,        // payer
        &rent_exempt_account, // from
        &mint_keypair,        // to
        rent_exempt_minimum + 1,
        mock_program_id,
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert!(bank.get_account(&rent_exempt_account.pubkey()).is_none());
}

#[test]
fn test_invalid_rent_state_changes_new_accounts() {
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(100 * LAMPORTS_PER_SOL, &Pubkey::new_unique(), 42);
    genesis_config.rent = Rent::default();

    let mock_program_id = Pubkey::new_unique();
    let account_data_size = 100;
    let rent_exempt_minimum = genesis_config.rent.minimum_balance(account_data_size);

    let (bank, _bank_forks) = Bank::new_with_mockup_builtin_for_tests(
        &genesis_config,
        mock_program_id,
        MockTransferBuiltin::vm,
    );
    let recent_blockhash = bank.last_blockhash();

    let check_account_is_rent_exempt = |pubkey: &Pubkey| -> bool {
        let account = bank.get_account(pubkey).unwrap();
        Rent::default().is_exempt(account.lamports(), account.data().len())
    };

    // Try to create RentPaying account
    let rent_paying_account = Keypair::new();
    let tx = system_transaction::create_account(
        &mint_keypair,
        &rent_paying_account,
        recent_blockhash,
        rent_exempt_minimum - 1,
        account_data_size as u64,
        &mock_program_id,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_err());
    assert!(bank.get_account(&rent_paying_account.pubkey()).is_none());

    // Try to create RentExempt account
    let rent_exempt_account = Keypair::new();
    let tx = system_transaction::create_account(
        &mint_keypair,
        &rent_exempt_account,
        recent_blockhash,
        rent_exempt_minimum,
        account_data_size as u64,
        &mock_program_id,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert!(check_account_is_rent_exempt(&rent_exempt_account.pubkey()));
}

#[test]
fn test_drained_created_account() {
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(100 * LAMPORTS_PER_SOL, &Pubkey::new_unique(), 42);
    genesis_config.rent = Rent::default();
    activate_all_features(&mut genesis_config);

    let mock_program_id = Pubkey::new_unique();
    // small enough to not pay rent, thus bypassing the data clearing rent
    // mechanism
    let data_size_no_rent = 100;
    // large enough to pay rent, will have data cleared
    let data_size_rent = 10000;
    let lamports_to_transfer = 100;

    // Create legacy accounts of various kinds
    let created_keypair = Keypair::new();

    let (bank, _bank_forks) = Bank::new_with_mockup_builtin_for_tests(
        &genesis_config,
        mock_program_id,
        MockTransferBuiltin::vm,
    );
    let recent_blockhash = bank.last_blockhash();

    // Create and drain a small data size account
    let create_instruction = system_instruction::create_account(
        &mint_keypair.pubkey(),
        &created_keypair.pubkey(),
        lamports_to_transfer,
        data_size_no_rent,
        &mock_program_id,
    );
    let account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new(created_keypair.pubkey(), true),
        AccountMeta::new(mint_keypair.pubkey(), false),
    ];
    let transfer_from_instruction = Instruction::new_with_bincode(
        mock_program_id,
        &MockTransferInstruction::Transfer(lamports_to_transfer),
        account_metas,
    );
    let tx = Transaction::new_signed_with_payer(
        &[create_instruction, transfer_from_instruction],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair, &created_keypair],
        recent_blockhash,
    );

    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    // account data is not stored because of zero balance even though its
    // data wasn't cleared
    assert!(bank.get_account(&created_keypair.pubkey()).is_none());

    // Create and drain a large data size account
    let create_instruction = system_instruction::create_account(
        &mint_keypair.pubkey(),
        &created_keypair.pubkey(),
        lamports_to_transfer,
        data_size_rent,
        &mock_program_id,
    );
    let account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new(created_keypair.pubkey(), true),
        AccountMeta::new(mint_keypair.pubkey(), false),
    ];
    let transfer_from_instruction = Instruction::new_with_bincode(
        mock_program_id,
        &MockTransferInstruction::Transfer(lamports_to_transfer),
        account_metas,
    );
    let tx = Transaction::new_signed_with_payer(
        &[create_instruction, transfer_from_instruction],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair, &created_keypair],
        recent_blockhash,
    );

    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    // account data is not stored because of zero balance
    assert!(bank.get_account(&created_keypair.pubkey()).is_none());
}

#[test]
fn test_rent_state_changes_sysvars() {
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(100 * LAMPORTS_PER_SOL, &Pubkey::new_unique(), 42);
    genesis_config.rent = Rent::default();

    let validator_pubkey = Pubkey::new_unique();
    let validator_stake_lamports = LAMPORTS_PER_SOL;
    let validator_vote_account_pubkey = Pubkey::new_unique();
    let validator_voting_keypair = Keypair::new();

    let validator_vote_account = vote_state::create_v4_account_with_authorized(
        &validator_pubkey,
        &validator_voting_keypair.pubkey(),
        &validator_voting_keypair.pubkey(),
        None,
        0,
        validator_stake_lamports,
    );

    genesis_config.accounts.insert(
        validator_pubkey,
        Account::new(
            genesis_config.rent.minimum_balance(0),
            0,
            &system_program::id(),
        ),
    );
    genesis_config.accounts.insert(
        validator_vote_account_pubkey,
        Account::from(validator_vote_account),
    );

    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    // Ensure transactions with sysvars succeed, even though sysvars appear RentPaying by balance
    let tx = Transaction::new_signed_with_payer(
        &[vote_instruction::authorize(
            &validator_vote_account_pubkey,
            &validator_voting_keypair.pubkey(),
            &Pubkey::new_unique(),
            VoteAuthorize::Voter,
        )],
        Some(&mint_keypair.pubkey()),
        &[&mint_keypair, &validator_voting_keypair],
        bank.last_blockhash(),
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
}

#[test]
fn test_invalid_rent_state_changes_fee_payer() {
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(100 * LAMPORTS_PER_SOL, &Pubkey::new_unique(), 42);
    genesis_config.rent = Rent::default();
    genesis_config.fee_rate_governor = FeeRateGovernor::new(
        solana_fee_calculator::DEFAULT_TARGET_LAMPORTS_PER_SIGNATURE,
        solana_fee_calculator::DEFAULT_TARGET_SIGNATURES_PER_SLOT,
    );
    let rent_exempt_minimum = genesis_config.rent.minimum_balance(0);

    // Create legacy rent-paying System account
    let rent_paying_fee_payer = Keypair::new();
    genesis_config.accounts.insert(
        rent_paying_fee_payer.pubkey(),
        Account::new(rent_exempt_minimum - 1, 0, &system_program::id()),
    );
    // Create RentExempt recipient account
    let recipient = Pubkey::new_unique();
    genesis_config.accounts.insert(
        recipient,
        Account::new(rent_exempt_minimum, 0, &system_program::id()),
    );

    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let recent_blockhash = bank.last_blockhash();

    let check_account_is_rent_exempt = |pubkey: &Pubkey| -> bool {
        let account = bank.get_account(pubkey).unwrap();
        Rent::default().is_exempt(account.lamports(), account.data().len())
    };

    // Create just-rent-exempt fee-payer
    let rent_exempt_fee_payer = Keypair::new();
    bank.transfer(
        rent_exempt_minimum,
        &mint_keypair,
        &rent_exempt_fee_payer.pubkey(),
    )
    .unwrap();

    // Dummy message to determine fee amount
    let dummy_message = new_sanitized_message(Message::new_with_blockhash(
        &[system_instruction::transfer(
            &rent_exempt_fee_payer.pubkey(),
            &recipient,
            LAMPORTS_PER_SOL,
        )],
        Some(&rent_exempt_fee_payer.pubkey()),
        &recent_blockhash,
    ));
    let fee = bank.get_fee_for_message(&dummy_message).unwrap();

    // RentPaying fee-payer can remain RentPaying
    let tx = Transaction::new(
        &[&rent_paying_fee_payer, &mint_keypair],
        Message::new(
            &[system_instruction::transfer(
                &mint_keypair.pubkey(),
                &recipient,
                rent_exempt_minimum,
            )],
            Some(&rent_paying_fee_payer.pubkey()),
        ),
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert!(!check_account_is_rent_exempt(
        &rent_paying_fee_payer.pubkey()
    ));

    // RentPaying fee-payer can remain RentPaying on failed executed tx
    let sender = Keypair::new();
    let fee_payer_balance = bank.get_balance(&rent_paying_fee_payer.pubkey());
    let tx = Transaction::new(
        &[&rent_paying_fee_payer, &sender],
        Message::new(
            &[system_instruction::transfer(
                &sender.pubkey(),
                &recipient,
                rent_exempt_minimum,
            )],
            Some(&rent_paying_fee_payer.pubkey()),
        ),
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert_eq!(
        result.unwrap_err(),
        TransactionError::InstructionError(0, InstructionError::Custom(1))
    );
    assert_ne!(
        fee_payer_balance,
        bank.get_balance(&rent_paying_fee_payer.pubkey())
    );
    assert!(!check_account_is_rent_exempt(
        &rent_paying_fee_payer.pubkey()
    ));

    // RentPaying fee-payer can be emptied with fee and transaction
    let tx = Transaction::new(
        &[&rent_paying_fee_payer],
        Message::new(
            &[system_instruction::transfer(
                &rent_paying_fee_payer.pubkey(),
                &recipient,
                bank.get_balance(&rent_paying_fee_payer.pubkey()) - fee,
            )],
            Some(&rent_paying_fee_payer.pubkey()),
        ),
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert_eq!(0, bank.get_balance(&rent_paying_fee_payer.pubkey()));

    // RentExempt fee-payer cannot become RentPaying from transaction fee
    let tx = Transaction::new(
        &[&rent_exempt_fee_payer, &mint_keypair],
        Message::new(
            &[system_instruction::transfer(
                &mint_keypair.pubkey(),
                &recipient,
                rent_exempt_minimum,
            )],
            Some(&rent_exempt_fee_payer.pubkey()),
        ),
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert_eq!(
        result.unwrap_err(),
        TransactionError::InsufficientFundsForRent { account_index: 0 }
    );
    assert!(check_account_is_rent_exempt(
        &rent_exempt_fee_payer.pubkey()
    ));

    // RentExempt fee-payer cannot become RentPaying via failed executed tx
    let tx = Transaction::new(
        &[&rent_exempt_fee_payer, &sender],
        Message::new(
            &[system_instruction::transfer(
                &sender.pubkey(),
                &recipient,
                rent_exempt_minimum,
            )],
            Some(&rent_exempt_fee_payer.pubkey()),
        ),
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert_eq!(
        result.unwrap_err(),
        TransactionError::InsufficientFundsForRent { account_index: 0 }
    );
    assert!(check_account_is_rent_exempt(
        &rent_exempt_fee_payer.pubkey()
    ));

    // For good measure, show that a RentExempt fee-payer that is also debited by a transaction
    // cannot become RentPaying by that debit, but can still be charged for the fee
    bank.transfer(fee, &mint_keypair, &rent_exempt_fee_payer.pubkey())
        .unwrap();
    let fee_payer_balance = bank.get_balance(&rent_exempt_fee_payer.pubkey());
    assert_eq!(fee_payer_balance, rent_exempt_minimum + fee);
    let tx = Transaction::new(
        &[&rent_exempt_fee_payer],
        Message::new(
            &[system_instruction::transfer(
                &rent_exempt_fee_payer.pubkey(),
                &recipient,
                fee,
            )],
            Some(&rent_exempt_fee_payer.pubkey()),
        ),
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert_eq!(
        result.unwrap_err(),
        TransactionError::InsufficientFundsForRent { account_index: 0 }
    );
    assert_eq!(
        fee_payer_balance - fee,
        bank.get_balance(&rent_exempt_fee_payer.pubkey())
    );
    assert!(check_account_is_rent_exempt(
        &rent_exempt_fee_payer.pubkey()
    ));

    // Also show that a RentExempt fee-payer can be completely emptied via fee and transaction
    bank.transfer(fee + 1, &mint_keypair, &rent_exempt_fee_payer.pubkey())
        .unwrap();
    assert!(bank.get_balance(&rent_exempt_fee_payer.pubkey()) > rent_exempt_minimum + fee);
    let tx = Transaction::new(
        &[&rent_exempt_fee_payer],
        Message::new(
            &[system_instruction::transfer(
                &rent_exempt_fee_payer.pubkey(),
                &recipient,
                bank.get_balance(&rent_exempt_fee_payer.pubkey()) - fee,
            )],
            Some(&rent_exempt_fee_payer.pubkey()),
        ),
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert_eq!(0, bank.get_balance(&rent_exempt_fee_payer.pubkey()));

    // ... but not if the fee alone would make it RentPaying
    bank.transfer(
        rent_exempt_minimum + 1,
        &mint_keypair,
        &rent_exempt_fee_payer.pubkey(),
    )
    .unwrap();
    assert!(bank.get_balance(&rent_exempt_fee_payer.pubkey()) < rent_exempt_minimum + fee);
    let tx = Transaction::new(
        &[&rent_exempt_fee_payer],
        Message::new(
            &[system_instruction::transfer(
                &rent_exempt_fee_payer.pubkey(),
                &recipient,
                bank.get_balance(&rent_exempt_fee_payer.pubkey()) - fee,
            )],
            Some(&rent_exempt_fee_payer.pubkey()),
        ),
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert_eq!(
        result.unwrap_err(),
        TransactionError::InsufficientFundsForRent { account_index: 0 }
    );
    assert!(check_account_is_rent_exempt(
        &rent_exempt_fee_payer.pubkey()
    ));
}

// Ensure System transfers of any size can be made to the incinerator
#[test]
fn test_rent_state_incinerator() {
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(100 * LAMPORTS_PER_SOL, &Pubkey::new_unique(), 42);
    genesis_config.rent = Rent::default();
    let rent_exempt_minimum = genesis_config.rent.minimum_balance(0);

    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    for amount in [rent_exempt_minimum - 1, rent_exempt_minimum] {
        bank.transfer(amount, &mint_keypair, &solana_sdk_ids::incinerator::id())
            .unwrap();
    }
}

#[test]
fn test_update_accounts_data_size() {
    // Test: Subtraction saturates at 0
    {
        let bank = create_simple_test_bank(100);
        let initial_data_size = bank.load_accounts_data_size() as i64;
        let data_size = 567;
        bank.accounts_data_size_delta_on_chain
            .store(data_size, Release);
        bank.update_accounts_data_size_delta_on_chain(
            (initial_data_size + data_size + 1).saturating_neg(),
        );
        assert_eq!(bank.load_accounts_data_size(), 0);
    }

    // Test: Addition saturates at u64::MAX
    {
        let mut bank = create_simple_test_bank(100);
        let data_size_remaining = 567;
        bank.accounts_data_size_initial = u64::MAX - data_size_remaining;
        bank.accounts_data_size_delta_off_chain
            .store((data_size_remaining + 1) as i64, Release);
        assert_eq!(bank.load_accounts_data_size(), u64::MAX);
    }

    // Test: Updates work as expected
    {
        // Set the accounts data size to be in the middle, then perform a bunch of small
        // updates, checking the results after each one.
        let mut bank = create_simple_test_bank(100);
        bank.accounts_data_size_initial = u32::MAX as u64;
        let mut rng = rand::rng();
        for _ in 0..100 {
            let initial = bank.load_accounts_data_size() as i64;
            let delta1 = rng.random_range(-500..500);
            bank.update_accounts_data_size_delta_on_chain(delta1);
            let delta2 = rng.random_range(-500..500);
            bank.update_accounts_data_size_delta_off_chain(delta2);
            assert_eq!(
                bank.load_accounts_data_size() as i64,
                initial.saturating_add(delta1).saturating_add(delta2),
            );
        }
    }
}

#[derive(Serialize, Deserialize)]
enum MockReallocInstruction {
    Realloc(usize, u64, Pubkey),
}

declare_process_instruction!(MockReallocBuiltin, 1, |invoke_context| {
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let instruction_data = instruction_context.get_instruction_data();
    if let Ok(instruction) = bincode::deserialize(instruction_data) {
        match instruction {
            MockReallocInstruction::Realloc(new_size, new_balance, _) => {
                // Set data length
                instruction_context
                    .try_borrow_instruction_account(1)?
                    .set_data_length(new_size)?;

                // set balance
                let current_balance = instruction_context
                    .try_borrow_instruction_account(1)?
                    .get_lamports();
                let diff_balance = (new_balance as i64).saturating_sub(current_balance as i64);
                let amount = diff_balance.unsigned_abs();
                if diff_balance.is_positive() {
                    instruction_context
                        .try_borrow_instruction_account(0)?
                        .checked_sub_lamports(amount)?;
                    instruction_context
                        .try_borrow_instruction_account(1)?
                        .set_lamports(new_balance)?;
                } else {
                    instruction_context
                        .try_borrow_instruction_account(0)?
                        .checked_add_lamports(amount)?;
                    instruction_context
                        .try_borrow_instruction_account(1)?
                        .set_lamports(new_balance)?;
                }
                Ok(())
            }
        }
    } else {
        Err(InstructionError::InvalidInstructionData)
    }
});

fn create_mock_realloc_tx(
    payer: &Keypair,
    funder: &Keypair,
    reallocd: &Pubkey,
    new_size: usize,
    new_balance: u64,
    mock_program_id: Pubkey,
    recent_blockhash: Hash,
) -> Transaction {
    let account_metas = vec![
        AccountMeta::new(funder.pubkey(), false),
        AccountMeta::new(*reallocd, false),
    ];
    let instruction = Instruction::new_with_bincode(
        mock_program_id,
        &MockReallocInstruction::Realloc(new_size, new_balance, Pubkey::new_unique()),
        account_metas,
    );
    Transaction::new_signed_with_payer(
        &[instruction],
        Some(&payer.pubkey()),
        &[payer],
        recent_blockhash,
    )
}

#[test]
fn test_resize_and_rent() {
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(1_000_000_000, &Pubkey::new_unique(), 42);
    genesis_config.rent = Rent::default();
    activate_all_features(&mut genesis_config);

    let mock_program_id = Pubkey::new_unique();
    let (bank, _bank_forks) = Bank::new_with_mockup_builtin_for_tests(
        &genesis_config,
        mock_program_id,
        MockReallocBuiltin::vm,
    );

    let recent_blockhash = bank.last_blockhash();

    let account_data_size_small = 1024;
    let rent_exempt_minimum_small = genesis_config.rent.minimum_balance(account_data_size_small);
    let account_data_size_large = 2048;
    let rent_exempt_minimum_large = genesis_config.rent.minimum_balance(account_data_size_large);

    let funding_keypair = Keypair::new();
    bank.store_account(
        &funding_keypair.pubkey(),
        &AccountSharedData::new(1_000_000_000, 0, &mock_program_id),
    );

    let rent_paying_pubkey = solana_pubkey::new_rand();
    let mut rent_paying_account = AccountSharedData::new(
        rent_exempt_minimum_small - 1,
        account_data_size_small,
        &mock_program_id,
    );
    rent_paying_account.set_rent_epoch(1);

    // restore program-owned account
    bank.store_account(&rent_paying_pubkey, &rent_paying_account);

    // rent paying, realloc larger, fail because not rent exempt
    let tx = create_mock_realloc_tx(
        &mint_keypair,
        &funding_keypair,
        &rent_paying_pubkey,
        account_data_size_large,
        rent_exempt_minimum_small - 1,
        mock_program_id,
        recent_blockhash,
    );
    let expected_err = {
        let account_index = tx
            .message
            .account_keys
            .iter()
            .position(|key| key == &rent_paying_pubkey)
            .unwrap() as u8;
        TransactionError::InsufficientFundsForRent { account_index }
    };
    assert_eq!(bank.process_transaction(&tx).unwrap_err(), expected_err);
    assert_eq!(
        rent_exempt_minimum_small - 1,
        bank.get_account(&rent_paying_pubkey).unwrap().lamports()
    );

    // rent paying, realloc larger and rent exempt
    let tx = create_mock_realloc_tx(
        &mint_keypair,
        &funding_keypair,
        &rent_paying_pubkey,
        account_data_size_large,
        rent_exempt_minimum_large,
        mock_program_id,
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert_eq!(
        rent_exempt_minimum_large,
        bank.get_account(&rent_paying_pubkey).unwrap().lamports()
    );

    // rent exempt, realloc small, fail because not rent exempt
    let tx = create_mock_realloc_tx(
        &mint_keypair,
        &funding_keypair,
        &rent_paying_pubkey,
        account_data_size_small,
        rent_exempt_minimum_small - 1,
        mock_program_id,
        recent_blockhash,
    );
    let expected_err = {
        let account_index = tx
            .message
            .account_keys
            .iter()
            .position(|key| key == &rent_paying_pubkey)
            .unwrap() as u8;
        TransactionError::InsufficientFundsForRent { account_index }
    };
    assert_eq!(bank.process_transaction(&tx).unwrap_err(), expected_err);
    assert_eq!(
        rent_exempt_minimum_large,
        bank.get_account(&rent_paying_pubkey).unwrap().lamports()
    );

    // rent exempt, realloc smaller and rent exempt
    let tx = create_mock_realloc_tx(
        &mint_keypair,
        &funding_keypair,
        &rent_paying_pubkey,
        account_data_size_small,
        rent_exempt_minimum_small,
        mock_program_id,
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert_eq!(
        rent_exempt_minimum_small,
        bank.get_account(&rent_paying_pubkey).unwrap().lamports()
    );

    // rent exempt, realloc large, fail because not rent exempt
    let tx = create_mock_realloc_tx(
        &mint_keypair,
        &funding_keypair,
        &rent_paying_pubkey,
        account_data_size_large,
        rent_exempt_minimum_large - 1,
        mock_program_id,
        recent_blockhash,
    );
    let expected_err = {
        let account_index = tx
            .message
            .account_keys
            .iter()
            .position(|key| key == &rent_paying_pubkey)
            .unwrap() as u8;
        TransactionError::InsufficientFundsForRent { account_index }
    };
    assert_eq!(bank.process_transaction(&tx).unwrap_err(), expected_err);
    assert_eq!(
        rent_exempt_minimum_small,
        bank.get_account(&rent_paying_pubkey).unwrap().lamports()
    );

    // rent exempt, realloc large and rent exempt
    let tx = create_mock_realloc_tx(
        &mint_keypair,
        &funding_keypair,
        &rent_paying_pubkey,
        account_data_size_large,
        rent_exempt_minimum_large,
        mock_program_id,
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert_eq!(
        rent_exempt_minimum_large,
        bank.get_account(&rent_paying_pubkey).unwrap().lamports()
    );

    let created_keypair = Keypair::new();

    // create account, not rent exempt
    let tx = system_transaction::create_account(
        &mint_keypair,
        &created_keypair,
        recent_blockhash,
        rent_exempt_minimum_small - 1,
        account_data_size_small as u64,
        &system_program::id(),
    );
    let expected_err = {
        let account_index = tx
            .message
            .account_keys
            .iter()
            .position(|key| key == &created_keypair.pubkey())
            .unwrap() as u8;
        TransactionError::InsufficientFundsForRent { account_index }
    };
    assert_eq!(bank.process_transaction(&tx).unwrap_err(), expected_err);

    // create account, rent exempt
    let tx = system_transaction::create_account(
        &mint_keypair,
        &created_keypair,
        recent_blockhash,
        rent_exempt_minimum_small,
        account_data_size_small as u64,
        &system_program::id(),
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert_eq!(
        rent_exempt_minimum_small,
        bank.get_account(&created_keypair.pubkey())
            .unwrap()
            .lamports()
    );

    let created_keypair = Keypair::new();
    // create account, no data
    let tx = system_transaction::create_account(
        &mint_keypair,
        &created_keypair,
        recent_blockhash,
        rent_exempt_minimum_small - 1,
        0,
        &system_program::id(),
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert_eq!(
        rent_exempt_minimum_small - 1,
        bank.get_account(&created_keypair.pubkey())
            .unwrap()
            .lamports()
    );

    // alloc but not rent exempt
    let tx = system_transaction::allocate(
        &mint_keypair,
        &created_keypair,
        recent_blockhash,
        (account_data_size_small + 1) as u64,
    );
    let expected_err = {
        let account_index = tx
            .message
            .account_keys
            .iter()
            .position(|key| key == &created_keypair.pubkey())
            .unwrap() as u8;
        TransactionError::InsufficientFundsForRent { account_index }
    };
    assert_eq!(bank.process_transaction(&tx).unwrap_err(), expected_err);

    // bring balance of account up to rent exemption
    let tx = system_transaction::transfer(
        &mint_keypair,
        &created_keypair.pubkey(),
        1,
        recent_blockhash,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert_eq!(
        rent_exempt_minimum_small,
        bank.get_account(&created_keypair.pubkey())
            .unwrap()
            .lamports()
    );

    // allocate as rent exempt
    let tx = system_transaction::allocate(
        &mint_keypair,
        &created_keypair,
        recent_blockhash,
        account_data_size_small as u64,
    );
    let result = bank.process_transaction(&tx);
    assert!(result.is_ok());
    assert_eq!(
        rent_exempt_minimum_small,
        bank.get_account(&created_keypair.pubkey())
            .unwrap()
            .lamports()
    );
}

/// Ensure that accounts data size is updated correctly on resize transactions
#[test]
fn test_accounts_data_size_and_resize_transactions() {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = genesis_utils::create_genesis_config(100 * LAMPORTS_PER_SOL);
    let mock_program_id = Pubkey::new_unique();
    let (bank, _bank_forks) = Bank::new_with_mockup_builtin_for_tests(
        &genesis_config,
        mock_program_id,
        MockReallocBuiltin::vm,
    );

    let recent_blockhash = bank.last_blockhash();

    let funding_keypair = Keypair::new();
    bank.store_account(
        &funding_keypair.pubkey(),
        &AccountSharedData::new(10 * LAMPORTS_PER_SOL, 0, &mock_program_id),
    );

    let mut rng = rand::rng();

    // Test case: Grow account
    {
        let account_pubkey = Pubkey::new_unique();
        let account_balance = LAMPORTS_PER_SOL;
        let account_size =
            rng.random_range(1..MAX_PERMITTED_DATA_LENGTH as usize - MAX_PERMITTED_DATA_INCREASE);
        let account_data = AccountSharedData::new(account_balance, account_size, &mock_program_id);
        bank.store_account(&account_pubkey, &account_data);

        let accounts_data_size_before = bank.load_accounts_data_size();
        let account_grow_size = rng.random_range(1..MAX_PERMITTED_DATA_INCREASE);
        let transaction = create_mock_realloc_tx(
            &mint_keypair,
            &funding_keypair,
            &account_pubkey,
            account_size + account_grow_size,
            account_balance,
            mock_program_id,
            recent_blockhash,
        );
        let result = bank.process_transaction(&transaction);
        assert!(result.is_ok());
        let accounts_data_size_after = bank.load_accounts_data_size();
        assert_eq!(
            accounts_data_size_after,
            accounts_data_size_before.saturating_add(account_grow_size as u64),
        );
    }

    // Test case: Shrink account
    {
        let account_pubkey = Pubkey::new_unique();
        let account_balance = LAMPORTS_PER_SOL;
        let account_size =
            rng.random_range(MAX_PERMITTED_DATA_LENGTH / 2..MAX_PERMITTED_DATA_LENGTH) as usize;
        let account_data = AccountSharedData::new(account_balance, account_size, &mock_program_id);
        bank.store_account(&account_pubkey, &account_data);

        let accounts_data_size_before = bank.load_accounts_data_size();
        let account_shrink_size = rng.random_range(1..account_size);
        let transaction = create_mock_realloc_tx(
            &mint_keypair,
            &funding_keypair,
            &account_pubkey,
            account_size - account_shrink_size,
            account_balance,
            mock_program_id,
            recent_blockhash,
        );
        let result = bank.process_transaction(&transaction);
        assert!(result.is_ok());
        let accounts_data_size_after = bank.load_accounts_data_size();
        assert_eq!(
            accounts_data_size_after,
            accounts_data_size_before.saturating_sub(account_shrink_size as u64),
        );
    }
}

#[test]
fn test_accounts_data_size_with_default_bank() {
    let bank = Bank::default_for_tests();
    assert_eq!(
        bank.load_accounts_data_size(),
        bank.calculate_accounts_data_size().unwrap()
    );
}

#[test]
fn test_accounts_data_size_from_genesis() {
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = genesis_utils::create_genesis_config_with_leader(
        1_000_000 * LAMPORTS_PER_SOL,
        &Pubkey::new_unique(),
        100 * LAMPORTS_PER_SOL,
    );
    genesis_config.rent = Rent::default();
    genesis_config.ticks_per_slot = 3;

    let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    assert_eq!(
        bank.load_accounts_data_size(),
        bank.calculate_accounts_data_size().unwrap()
    );

    // Create accounts over a number of banks and ensure the accounts data size remains correct
    for _ in 0..10 {
        let slot = bank.slot() + 1;
        bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank,
            &Pubkey::default(),
            slot,
        );

        // Store an account into the bank that is rent-exempt and has data
        let data_size = rand::rng().random_range(3333..4444);
        let transaction = system_transaction::create_account(
            &mint_keypair,
            &Keypair::new(),
            bank.last_blockhash(),
            genesis_config.rent.minimum_balance(data_size),
            data_size as u64,
            &solana_system_interface::program::id(),
        );
        bank.process_transaction(&transaction).unwrap();
        bank.fill_bank_with_ticks_for_tests();

        assert_eq!(
            bank.load_accounts_data_size(),
            bank.calculate_accounts_data_size().unwrap(),
        );
    }
}

/// Ensures that if a transaction exceeds the maximum allowed accounts data allocation size:
/// 1. The transaction fails
/// 2. The bank's accounts_data_size is unmodified
#[test]
fn test_cap_accounts_data_allocations_per_transaction() {
    const NUM_MAX_SIZE_ALLOCATIONS_PER_TRANSACTION: usize =
        MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION as usize
            / MAX_PERMITTED_DATA_LENGTH as usize;

    let (genesis_config, mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    let mut instructions = Vec::new();
    let mut keypairs = vec![mint_keypair.insecure_clone()];
    for _ in 0..=NUM_MAX_SIZE_ALLOCATIONS_PER_TRANSACTION {
        let keypair = Keypair::new();
        let instruction = system_instruction::create_account(
            &mint_keypair.pubkey(),
            &keypair.pubkey(),
            bank.rent_collector()
                .rent
                .minimum_balance(MAX_PERMITTED_DATA_LENGTH as usize),
            MAX_PERMITTED_DATA_LENGTH,
            &solana_system_interface::program::id(),
        );
        keypairs.push(keypair);
        instructions.push(instruction);
    }
    let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
    let signers: Vec<_> = keypairs.iter().collect();
    let transaction = Transaction::new(&signers, message, bank.last_blockhash());

    let accounts_data_size_before = bank.load_accounts_data_size();
    let result = bank.process_transaction(&transaction);
    let accounts_data_size_after = bank.load_accounts_data_size();

    assert_eq!(accounts_data_size_before, accounts_data_size_after);
    assert_eq!(
        result,
        Err(TransactionError::InstructionError(
            NUM_MAX_SIZE_ALLOCATIONS_PER_TRANSACTION as u8,
            solana_instruction::error::InstructionError::MaxAccountsDataAllocationsExceeded,
        )),
    );
}

#[test]
fn test_calculate_fee_with_congestion_multiplier() {
    let lamports_scale: u64 = 5;
    let base_lamports_per_signature: u64 = 5_000;
    let cheap_lamports_per_signature: u64 = base_lamports_per_signature / lamports_scale;
    let expensive_lamports_per_signature: u64 = base_lamports_per_signature * lamports_scale;
    let signature_count: u64 = 2;
    let signature_fee: u64 = 10;
    let fee_structure = FeeStructure {
        lamports_per_signature: signature_fee,
        ..FeeStructure::default()
    };

    // Two signatures, double the fee.
    let key0 = Pubkey::new_unique();
    let key1 = Pubkey::new_unique();
    let ix0 = system_instruction::transfer(&key0, &key1, 1);
    let ix1 = system_instruction::transfer(&key1, &key0, 1);
    let message = new_sanitized_message(Message::new(&[ix0, ix1], Some(&key0)));

    // assert when lamports_per_signature is less than BASE_LAMPORTS, turnning on/off
    // congestion_multiplier has no effect on fee.
    assert_eq!(
        calculate_test_fee(&message, cheap_lamports_per_signature, &fee_structure),
        signature_fee * signature_count
    );

    // assert when lamports_per_signature is more than BASE_LAMPORTS, turnning on/off
    // congestion_multiplier will change calculated fee.
    assert_eq!(
        calculate_test_fee(&message, expensive_lamports_per_signature, &fee_structure,),
        signature_fee * signature_count
    );
}

#[test]
fn test_calculate_fee_with_request_heap_frame_flag() {
    let key0 = Pubkey::new_unique();
    let key1 = Pubkey::new_unique();
    let lamports_per_signature: u64 = 5_000;
    let signature_fee: u64 = 10;
    let request_cu: u64 = 1;
    let lamports_per_cu: u64 = 5;
    let fee_structure = FeeStructure {
        lamports_per_signature: signature_fee,
        ..FeeStructure::default()
    };
    let message = new_sanitized_message(Message::new(
        &[
            system_instruction::transfer(&key0, &key1, 1),
            ComputeBudgetInstruction::set_compute_unit_limit(request_cu as u32),
            ComputeBudgetInstruction::request_heap_frame(40 * 1024),
            ComputeBudgetInstruction::set_compute_unit_price(lamports_per_cu * 1_000_000),
        ],
        Some(&key0),
    ));

    // assert when request_heap_frame is presented in tx, prioritization fee will be counted
    // into transaction fee
    assert_eq!(
        calculate_test_fee(&message, lamports_per_signature, &fee_structure),
        signature_fee + request_cu * lamports_per_cu
    );
}

#[test]
fn test_is_in_slot_hashes_history() {
    use solana_slot_hashes::MAX_ENTRIES;

    let (bank0, _bank_forks) = create_simple_test_arc_bank(1);
    assert!(!bank0.is_in_slot_hashes_history(&0));
    assert!(!bank0.is_in_slot_hashes_history(&1));
    let mut last_bank = bank0;
    for _ in 0..MAX_ENTRIES {
        let new_bank = Arc::new(new_from_parent(last_bank));
        assert!(new_bank.is_in_slot_hashes_history(&0));
        last_bank = new_bank;
    }
    let new_bank = Arc::new(new_from_parent(last_bank));
    assert!(!new_bank.is_in_slot_hashes_history(&0));
}

#[test]
fn test_feature_activation_loaded_programs_cache_preparation_phase() {
    agave_logger::setup();

    // Bank Setup
    let (genesis_config, mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
    let mut bank = Bank::new_for_tests(&genesis_config);
    let mut feature_set = FeatureSet::all_enabled();
    feature_set.deactivate(&feature_set::disable_sbpf_v0_execution::id());
    feature_set.deactivate(&feature_set::reenable_sbpf_v0_execution::id());
    bank.feature_set = Arc::new(feature_set);
    let (root_bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();

    // Program Setup
    let program_keypair = Keypair::new();
    let program_data = include_bytes!("../../../programs/bpf_loader/test_elfs/out/noop_aligned.so");
    let program_account = AccountSharedData::from(Account {
        lamports: Rent::default().minimum_balance(program_data.len()).min(1),
        data: program_data.to_vec(),
        owner: bpf_loader::id(),
        executable: true,
        rent_epoch: 0,
    });
    root_bank.store_account(&program_keypair.pubkey(), &program_account);

    // Compose message using the desired program.
    let instruction = Instruction::new_with_bytes(program_keypair.pubkey(), &[], Vec::new());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let binding = mint_keypair.insecure_clone();
    let signers = vec![&binding];

    // Advance the bank so that the program becomes effective.
    goto_end_of_slot(root_bank.clone());
    let bank = new_from_parent_with_fork_next_slot(root_bank, bank_forks.as_ref());

    // Load the program with the old environment.
    let transaction = Transaction::new(&signers, message.clone(), bank.last_blockhash());
    let result_without_feature_enabled = bank.process_transaction(&transaction);
    assert_eq!(result_without_feature_enabled, Ok(()));

    // Schedule feature activation to trigger a change of environment at the epoch boundary.
    let feature_account_balance =
        std::cmp::max(genesis_config.rent.minimum_balance(Feature::size_of()), 1);
    bank.store_account(
        &feature_set::disable_sbpf_v0_execution::id(),
        &feature::create_account(&Feature { activated_at: None }, feature_account_balance),
    );

    // Advance the bank to middle of epoch to start the recompilation phase.
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), 16);
    let current_env = bank
        .transaction_processor
        .get_environments_for_epoch(0)
        .program_runtime_v1;
    let upcoming_env = bank
        .transaction_processor
        .get_environments_for_epoch(1)
        .program_runtime_v1;

    // Advance the bank to recompile the program.
    {
        let program_cache = bank
            .transaction_processor
            .global_program_cache
            .read()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert_eq!(slot_versions.len(), 1);
        assert!(Arc::ptr_eq(
            slot_versions[0].program.get_environment().unwrap(),
            &current_env
        ));
    }
    goto_end_of_slot(bank.clone());
    let bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
    {
        let program_cache = bank
            .transaction_processor
            .global_program_cache
            .write()
            .unwrap();
        let slot_versions = program_cache.get_slot_versions_for_tests(&program_keypair.pubkey());
        assert_eq!(slot_versions.len(), 2);
        assert!(Arc::ptr_eq(
            slot_versions[0].program.get_environment().unwrap(),
            &upcoming_env
        ));
        assert!(Arc::ptr_eq(
            slot_versions[1].program.get_environment().unwrap(),
            &current_env
        ));
    }

    // Advance the bank to cross the epoch boundary and activate the feature.
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), 33);

    // Load the program with the new environment.
    let transaction = Transaction::new(&signers, message, bank.last_blockhash());
    let result_with_feature_enabled = bank.process_transaction(&transaction);
    assert_eq!(
        result_with_feature_enabled,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::UnsupportedProgramId
        ))
    );
}

#[test]
fn test_feature_activation_loaded_programs_epoch_transition() {
    agave_logger::setup();

    // Bank Setup
    let (mut genesis_config, mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
    genesis_config
        .accounts
        .remove(&feature_set::disable_fees_sysvar::id());
    genesis_config
        .accounts
        .remove(&feature_set::reenable_sbpf_v0_execution::id());
    let (root_bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    // Program Setup
    let program_keypair = Keypair::new();
    let program_data = include_bytes!("../../../programs/bpf_loader/test_elfs/out/noop_aligned.so");
    let program_account = AccountSharedData::from(Account {
        lamports: Rent::default().minimum_balance(program_data.len()).min(1),
        data: program_data.to_vec(),
        owner: bpf_loader::id(),
        executable: true,
        rent_epoch: 0,
    });
    root_bank.store_account(&program_keypair.pubkey(), &program_account);

    // Compose message using the desired program.
    let instruction = Instruction::new_with_bytes(program_keypair.pubkey(), &[], Vec::new());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let binding = mint_keypair.insecure_clone();
    let signers = vec![&binding];

    // Advance the bank so that the program becomes effective.
    goto_end_of_slot(root_bank.clone());
    let bank = new_from_parent_with_fork_next_slot(root_bank, bank_forks.as_ref());

    // Load the program with the old environment.
    let transaction = Transaction::new(&signers, message.clone(), bank.last_blockhash());
    assert!(bank.process_transaction(&transaction).is_ok());

    // Schedule feature activation to trigger a change of environment at the epoch boundary.
    let feature_account_balance =
        std::cmp::max(genesis_config.rent.minimum_balance(Feature::size_of()), 1);
    bank.store_account(
        &feature_set::disable_fees_sysvar::id(),
        &feature::create_account(&Feature { activated_at: None }, feature_account_balance),
    );

    // Advance the bank to the end of the epoch to update the epoch_boundary_preparation.
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), 31);

    // Advance the bank to cross the epoch boundary and activate the feature.
    goto_end_of_slot(bank.clone());
    let bank = Bank::new_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), 32);

    // Load the program with the new environment.
    let transaction = Transaction::new(&signers, message.clone(), bank.last_blockhash());
    assert!(bank.process_transaction(&transaction).is_ok());

    {
        // Prune for rerooting and thus finishing the recompilation phase.
        let upcoming_environments = bank
            .transaction_processor
            .epoch_boundary_preparation
            .write()
            .unwrap()
            .reroot(bank.epoch());
        assert!(upcoming_environments.is_some());
        let mut program_cache = bank
            .transaction_processor
            .global_program_cache
            .write()
            .unwrap();
        program_cache.prune(bank.slot(), upcoming_environments);

        // Unload all (which is only the entry with the new environment)
        program_cache.sort_and_unload(percentage::Percentage::from(0));
    }

    // Reload the unloaded program with the new environment.
    goto_end_of_slot(bank.clone());
    let bank = new_from_parent_with_fork_next_slot(bank, bank_forks.as_ref());
    let transaction = Transaction::new(&signers, message, bank.last_blockhash());
    assert!(bank.process_transaction(&transaction).is_ok());
}

#[test]
fn test_verify_accounts() {
    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair: mint,
        ..
    } = genesis_utils::create_genesis_config_with_leader(
        1_000_000 * LAMPORTS_PER_SOL,
        &Pubkey::new_unique(),
        100 * LAMPORTS_PER_SOL,
    );
    genesis_config.rent = Rent::default();
    genesis_config.ticks_per_slot = 3;

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
        bank.transfer(amount + fee, &mint, &key1.pubkey()).unwrap();
        bank.transfer(amount + fee, &mint, &key2.pubkey()).unwrap();
        bank.transfer(amount + fee, &key2, &key3.pubkey()).unwrap();
        assert_eq!(bank.get_balance(&key2.pubkey()), 0);

        bank.fill_bank_with_ticks_for_tests();
    };

    let (mut bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);

    // make some banks, do some transactions, ensure there's some zero-lamport accounts
    for _ in 0..4 {
        let slot = bank.slot() + 1;
        bank = Bank::new_from_parent_with_bank_forks(
            bank_forks.as_ref(),
            bank,
            &Pubkey::new_unique(),
            slot,
        );
        do_transfers(&bank);
    }

    bank.squash();
    bank.force_flush_accounts_cache();

    // ensure the accounts verify successfully
    assert!(bank.verify_accounts(VerifyAccountsHashConfig::default_for_test(), None));
}

#[test]
fn test_squash_timing_add_assign() {
    let mut t0 = SquashTiming::default();

    let t1 = SquashTiming {
        squash_accounts_ms: 1,
        squash_accounts_cache_ms: 2,
        squash_accounts_index_ms: 3,
        squash_cache_ms: 5,
    };

    let expected = SquashTiming {
        squash_accounts_ms: 2,
        squash_accounts_cache_ms: 2 * 2,
        squash_accounts_index_ms: 3 * 2,
        squash_cache_ms: 5 * 2,
    };

    t0 += t1;
    t0 += t1;

    assert!(t0 == expected);
}

#[test]
fn test_system_instruction_allocate() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee(LAMPORTS_PER_SOL);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let bank_client = BankClient::new_shared(bank);
    let data_len = 2;
    let amount = genesis_config.rent.minimum_balance(data_len);

    let alice_keypair = Keypair::new();
    let alice_pubkey = alice_keypair.pubkey();
    let seed = "seed";
    let owner = Pubkey::new_unique();
    let alice_with_seed = Pubkey::create_with_seed(&alice_pubkey, seed, &owner).unwrap();

    bank_client
        .transfer_and_confirm(amount, &mint_keypair, &alice_pubkey)
        .unwrap();

    let allocate_with_seed = Message::new(
        &[system_instruction::allocate_with_seed(
            &alice_with_seed,
            &alice_pubkey,
            seed,
            data_len as u64,
            &owner,
        )],
        Some(&alice_pubkey),
    );

    assert!(bank_client
        .send_and_confirm_message(&[&alice_keypair], allocate_with_seed)
        .is_ok());

    let allocate = system_instruction::allocate(&alice_pubkey, data_len as u64);

    assert!(bank_client
        .send_and_confirm_instruction(&alice_keypair, allocate)
        .is_ok());
}

fn with_create_zero_lamport<F>(callback: F)
where
    F: Fn(&Bank),
{
    agave_logger::setup();

    let alice_keypair = Keypair::new();
    let bob_keypair = Keypair::new();

    let alice_pubkey = alice_keypair.pubkey();
    let bob_pubkey = bob_keypair.pubkey();

    let program = Pubkey::new_unique();
    let collector = Pubkey::new_unique();

    let mint_lamports = LAMPORTS_PER_SOL;
    let len1 = 123;
    let len2 = 456;

    // create initial bank and fund the alice account
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee_no_rent(mint_lamports);
    let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let bank_client = BankClient::new_shared(bank.clone());
    bank_client
        .transfer_and_confirm(mint_lamports, &mint_keypair, &alice_pubkey)
        .unwrap();

    // create a bank a few epochs in the future..
    let bank = new_from_parent_next_epoch(bank, &bank_forks, 2);

    // create the next bank in the current epoch
    let slot = bank.slot() + 1;
    let bank = Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank, &collector, slot);

    // create the next bank where we will store a zero-lamport account to be cleaned
    let slot = bank.slot() + 1;
    let bank = Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank, &collector, slot);
    let account = AccountSharedData::new(0, len1, &program);
    bank.store_account(&bob_pubkey, &account);

    // transfer some to bogus pubkey just to make previous bank (=slot) really cleanable
    let slot = bank.slot() + 1;
    let bank = Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank, &collector, slot);
    let bank_client = BankClient::new_shared(bank.clone());
    bank_client
        .transfer_and_confirm(
            genesis_config.rent.minimum_balance(0),
            &alice_keypair,
            &Pubkey::new_unique(),
        )
        .unwrap();

    // super fun time; callback chooses to .clean_accounts() or not
    let slot = bank.slot() + 1;
    let bank = Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank, &collector, slot);
    callback(&bank);

    // create a normal account at the same pubkey as the zero-lamports account
    let lamports = genesis_config.rent.minimum_balance(len2);
    let slot = bank.slot() + 1;
    let bank = Bank::new_from_parent_with_bank_forks(bank_forks.as_ref(), bank, &collector, slot);
    let bank_client = BankClient::new_shared(bank);
    let ix = system_instruction::create_account(
        &alice_pubkey,
        &bob_pubkey,
        lamports,
        len2 as u64,
        &program,
    );
    let message = Message::new(&[ix], Some(&alice_pubkey));
    let r = bank_client.send_and_confirm_message(&[&alice_keypair, &bob_keypair], message);
    assert!(r.is_ok());
}

#[test]
fn test_create_zero_lamport_with_clean() {
    // DEVELOPER TIP: To debug this test, you may want to add some logging to
    // `AccountsDb::write_accounts_to_storage` to see what accounts are actually
    // in the storage entries

    /*
        for index in 0..accounts_and_meta_to_store.len() {
            accounts_and_meta_to_store.account(index, |account| {
                println!("slot {slot} wrote account {}", account.pubkey());
            })
        }
    */

    with_create_zero_lamport(|bank| {
        bank.freeze();
        bank.squash();
        bank.force_flush_accounts_cache();
        // do clean and assert that it actually did its job
        assert_eq!(6, bank.get_snapshot_storages(None).len());
        bank.clean_accounts();
        assert_eq!(5, bank.get_snapshot_storages(None).len());
    });
}

#[test]
fn test_create_zero_lamport_without_clean() {
    with_create_zero_lamport(|_| {
        // just do nothing; this should behave identically with test_create_zero_lamport_with_clean
    });
}

#[test]
fn test_system_instruction_assign_with_seed() {
    let (genesis_config, mint_keypair) = create_genesis_config_no_tx_fee(LAMPORTS_PER_SOL);
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let bank_client = BankClient::new_shared(bank);

    let alice_keypair = Keypair::new();
    let alice_pubkey = alice_keypair.pubkey();
    let seed = "seed";
    let owner = Pubkey::new_unique();
    let alice_with_seed = Pubkey::create_with_seed(&alice_pubkey, seed, &owner).unwrap();

    bank_client
        .transfer_and_confirm(
            genesis_config.rent.minimum_balance(0),
            &mint_keypair,
            &alice_pubkey,
        )
        .unwrap();

    let assign_with_seed = Message::new(
        &[system_instruction::assign_with_seed(
            &alice_with_seed,
            &alice_pubkey,
            seed,
            &owner,
        )],
        Some(&alice_pubkey),
    );

    assert!(bank_client
        .send_and_confirm_message(&[&alice_keypair], assign_with_seed)
        .is_ok());
}

#[test]
fn test_system_instruction_unsigned_transaction() {
    let (genesis_config, alice_keypair) = create_genesis_config_no_tx_fee(LAMPORTS_PER_SOL);
    let alice_pubkey = alice_keypair.pubkey();
    let mallory_keypair = Keypair::new();
    let mallory_pubkey = mallory_keypair.pubkey();
    let amount = genesis_config.rent.minimum_balance(0);

    // Fund to account to bypass AccountNotFound error
    let (bank, _bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
    let bank_client = BankClient::new_shared(bank);
    bank_client
        .transfer_and_confirm(amount, &alice_keypair, &mallory_pubkey)
        .unwrap();

    // Erroneously sign transaction with recipient account key
    // No signature case is tested by bank `test_zero_signatures()`
    let account_metas = vec![
        AccountMeta::new(alice_pubkey, false),
        AccountMeta::new(mallory_pubkey, true),
    ];
    let malicious_instruction = Instruction::new_with_bincode(
        system_program::id(),
        &system_instruction::SystemInstruction::Transfer { lamports: amount },
        account_metas,
    );
    assert_eq!(
        bank_client
            .send_and_confirm_instruction(&mallory_keypair, malicious_instruction)
            .unwrap_err()
            .unwrap(),
        TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature)
    );
    assert_eq!(
        bank_client.get_balance(&alice_pubkey).unwrap(),
        LAMPORTS_PER_SOL - amount
    );
    assert_eq!(bank_client.get_balance(&mallory_pubkey).unwrap(), amount);
}

#[test]
fn test_calc_vote_accounts_to_store_empty() {
    let vote_account_rewards = HashMap::default();
    let result = Bank::calc_vote_accounts_to_store(vote_account_rewards);
    assert_eq!(
        result.accounts_with_rewards.len(),
        result.accounts_with_rewards.len()
    );
    assert!(result.accounts_with_rewards.is_empty());
}

#[test]
fn test_calc_vote_accounts_to_store_overflow() {
    let mut vote_account_rewards = HashMap::default();
    let pubkey = solana_pubkey::new_rand();
    let mut vote_account = AccountSharedData::default();
    vote_account.set_lamports(u64::MAX);
    vote_account_rewards.insert(
        pubkey,
        VoteReward {
            vote_account,
            commission: 0,
            vote_rewards: 1, // enough to overflow
        },
    );
    let result = Bank::calc_vote_accounts_to_store(vote_account_rewards);
    assert_eq!(
        result.accounts_with_rewards.len(),
        result.accounts_with_rewards.len()
    );
    assert!(result.accounts_with_rewards.is_empty());
}

#[test]
fn test_calc_vote_accounts_to_store_normal() {
    let pubkey = solana_pubkey::new_rand();
    for commission in 0..2 {
        for vote_rewards in 0..2 {
            let mut vote_account_rewards = HashMap::default();
            let mut vote_account = AccountSharedData::default();
            vote_account.set_lamports(1);
            vote_account_rewards.insert(
                pubkey,
                VoteReward {
                    vote_account: vote_account.clone(),
                    commission,
                    vote_rewards,
                },
            );
            let result = Bank::calc_vote_accounts_to_store(vote_account_rewards);
            assert_eq!(
                result.accounts_with_rewards.len(),
                result.accounts_with_rewards.len()
            );
            assert_eq!(result.accounts_with_rewards.len(), 1);
            let (pubkey_result, rewards, account) = &result.accounts_with_rewards[0];
            _ = vote_account.checked_add_lamports(vote_rewards);
            assert!(accounts_equal(account, &vote_account));
            assert_eq!(
                *rewards,
                RewardInfo {
                    reward_type: RewardType::Voting,
                    lamports: vote_rewards as i64,
                    post_balance: vote_account.lamports(),
                    commission: Some(commission),
                }
            );
            assert_eq!(*pubkey_result, pubkey);
        }
    }
}

#[test]
fn test_register_hard_fork() {
    fn get_hard_forks(bank: &Bank) -> Vec<Slot> {
        bank.hard_forks().iter().map(|(slot, _)| *slot).collect()
    }

    let (genesis_config, _mint_keypair) = create_genesis_config(10);
    let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));

    let bank7 = Bank::new_from_parent(bank0.clone(), &Pubkey::default(), 7);
    bank7.register_hard_fork(6);
    bank7.register_hard_fork(7);
    bank7.register_hard_fork(8);
    // Bank7 will reject slot 6 since it is older, but allow the other two hard forks
    assert_eq!(get_hard_forks(&bank7), vec![7, 8]);

    let bank9 = Bank::new_from_parent(bank0, &Pubkey::default(), 9);
    bank9.freeze();
    bank9.register_hard_fork(9);
    bank9.register_hard_fork(10);
    // Bank9 will reject slot 9 since it has already been frozen
    assert_eq!(get_hard_forks(&bank9), vec![7, 8, 10]);
}

#[test]
fn test_last_restart_slot() {
    fn last_restart_slot_dirty(bank: &Bank) -> bool {
        let dirty_accounts = bank
            .rc
            .accounts
            .accounts_db
            .get_pubkeys_for_slot(bank.slot());
        let dirty_accounts: HashSet<_> = dirty_accounts.into_iter().collect();
        dirty_accounts.contains(&sysvar::last_restart_slot::id())
    }

    fn get_last_restart_slot(bank: &Bank) -> Option<Slot> {
        bank.get_account(&sysvar::last_restart_slot::id())
            .and_then(|account| {
                let lrs: Option<LastRestartSlot> = from_account(&account);
                lrs
            })
            .map(|account| account.last_restart_slot)
    }

    let mint_lamports = 100;
    let validator_stake_lamports = 10;
    let leader_pubkey = Pubkey::default();
    let GenesisConfigInfo {
        mut genesis_config, ..
    } = create_genesis_config_with_leader(mint_lamports, &leader_pubkey, validator_stake_lamports);
    // Remove last restart slot account so we can simluate its' activation
    genesis_config
        .accounts
        .remove(&feature_set::last_restart_slot_sysvar::id())
        .unwrap();

    let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
    // Register a hard fork in the future so last restart slot will update
    bank0.register_hard_fork(6);
    assert!(!last_restart_slot_dirty(&bank0));
    assert_eq!(get_last_restart_slot(&bank0), None);

    let mut bank1 = Arc::new(Bank::new_from_parent(bank0, &Pubkey::default(), 1));
    assert!(!last_restart_slot_dirty(&bank1));
    assert_eq!(get_last_restart_slot(&bank1), None);

    // Activate the feature in slot 1, it will get initialized in slot 1's children
    Arc::get_mut(&mut bank1)
        .unwrap()
        .activate_feature(&feature_set::last_restart_slot_sysvar::id());
    let bank2 = Arc::new(Bank::new_from_parent(bank1.clone(), &Pubkey::default(), 2));
    assert!(last_restart_slot_dirty(&bank2));
    assert_eq!(get_last_restart_slot(&bank2), Some(0));
    let bank3 = Arc::new(Bank::new_from_parent(bank1, &Pubkey::default(), 3));
    assert!(last_restart_slot_dirty(&bank3));
    assert_eq!(get_last_restart_slot(&bank3), Some(0));

    // Not dirty in children where the last restart slot has not changed
    let bank4 = Arc::new(Bank::new_from_parent(bank2, &Pubkey::default(), 4));
    assert!(!last_restart_slot_dirty(&bank4));
    assert_eq!(get_last_restart_slot(&bank4), Some(0));
    let bank5 = Arc::new(Bank::new_from_parent(bank3, &Pubkey::default(), 5));
    assert!(!last_restart_slot_dirty(&bank5));
    assert_eq!(get_last_restart_slot(&bank5), Some(0));

    // Last restart slot has now changed so it will be dirty again
    let bank6 = Arc::new(Bank::new_from_parent(bank4, &Pubkey::default(), 6));
    assert!(last_restart_slot_dirty(&bank6));
    assert_eq!(get_last_restart_slot(&bank6), Some(6));

    // Last restart will not change for a hard fork that has not occurred yet
    bank6.register_hard_fork(10);
    let bank7 = Arc::new(Bank::new_from_parent(bank6, &Pubkey::default(), 7));
    assert!(!last_restart_slot_dirty(&bank7));
    assert_eq!(get_last_restart_slot(&bank7), Some(6));
}

/// Test that simulations report the compute units of failed transactions
#[test]
fn test_failed_simulation_compute_units() {
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let program_id = Pubkey::new_unique();
    let (bank, _bank_forks) =
        Bank::new_with_mockup_builtin_for_tests(&genesis_config, program_id, MockBuiltin::vm);

    const TEST_UNITS: u64 = 10_000;
    const MOCK_BUILTIN_UNITS: u64 = 1;
    let expected_consumed_units = TEST_UNITS + MOCK_BUILTIN_UNITS;
    // two accounts: program plus fee-payer (programdata does not exist)
    let expected_loaded_program_account_data_size =
        (bank.get_account(&program_id).unwrap().data().len() + TRANSACTION_ACCOUNT_BASE_SIZE * 2)
            as u32;
    declare_process_instruction!(MockBuiltin, MOCK_BUILTIN_UNITS, |invoke_context| {
        invoke_context.consume_checked(TEST_UNITS).unwrap();
        Err(InstructionError::InvalidInstructionData)
    });

    let message = Message::new(
        &[Instruction::new_with_bincode(program_id, &0, vec![])],
        Some(&mint_keypair.pubkey()),
    );
    let transaction = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());

    bank.freeze();
    let sanitized = RuntimeTransaction::from_transaction_for_tests(transaction);
    let simulation = bank.simulate_transaction(&sanitized, false);
    assert_eq!(expected_consumed_units, simulation.units_consumed);
    assert_eq!(
        expected_loaded_program_account_data_size,
        simulation.loaded_accounts_data_size
    );
}

/// Test that simulations report the load error of fees-only transactions
#[test]
fn test_failed_simulation_load_error() {
    let (genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, _bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let missing_program_id = Pubkey::new_unique();
    let message = Message::new(
        &[Instruction::new_with_bincode(
            missing_program_id,
            &0,
            vec![],
        )],
        Some(&mint_keypair.pubkey()),
    );
    let transaction = Transaction::new(&[&mint_keypair], message, bank.last_blockhash());

    bank.freeze();
    let mint_balance = bank.get_account(&mint_keypair.pubkey()).unwrap().lamports();
    let sanitized = RuntimeTransaction::from_transaction_for_tests(transaction);
    let simulation = bank.simulate_transaction(&sanitized, false);
    assert_eq!(
        simulation,
        TransactionSimulationResult {
            result: Err(TransactionError::ProgramAccountNotFound),
            logs: vec![],
            post_simulation_accounts: vec![],
            units_consumed: 0,
            loaded_accounts_data_size: 0,
            return_data: None,
            inner_instructions: None,
            fee: Some(0),
            pre_balances: Some(vec![mint_balance, 0]),
            post_balances: Some(vec![mint_balance, 0]),
            pre_token_balances: Some(vec![]),
            post_token_balances: Some(vec![]),
        }
    );
}

#[test]
fn test_filter_program_errors_and_collect_fee_details() {
    // TX  | PROCESSING RESULT           | COLLECT            | COLLECT
    //     |                             | (TX_FEE, PRIO_FEE) | RESULT
    // ---------------------------------------------------------------------------------
    // tx1 | not processed               | (0    , 0)         | Original Err
    // tx2 | processed but not executed  | (5_000, 1_000)     | Ok
    // tx3 | executed has error          | (5_000, 1_000)     | Ok
    // tx4 | executed and no error       | (5_000, 1_000)     | Ok
    //
    let initial_payer_balance = 7_000;
    let tx_fee = 5000;
    let priority_fee = 1000;
    let fee_details = FeeDetails::new(tx_fee, priority_fee);
    let expected_collected_fee_details = CollectorFeeDetails {
        transaction_fee: 3 * tx_fee,
        priority_fee: 3 * priority_fee,
    };

    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(initial_payer_balance, &Pubkey::new_unique(), 3);
    let bank = Bank::new_for_tests(&genesis_config);

    let results = vec![
        Err(TransactionError::AccountNotFound),
        Ok(ProcessedTransaction::FeesOnly(Box::new(
            FeesOnlyTransaction {
                load_error: TransactionError::InvalidProgramForExecution,
                rollback_accounts: RollbackAccounts::default(),
                fee_details,
            },
        ))),
        new_executed_processing_result(
            Err(TransactionError::InstructionError(
                0,
                SystemError::ResultWithNegativeLamports.into(),
            )),
            fee_details,
        ),
        new_executed_processing_result(Ok(()), fee_details),
    ];

    bank.filter_program_errors_and_collect_fee_details(&results);

    assert_eq!(
        expected_collected_fee_details,
        *bank.collector_fee_details.read().unwrap()
    );
    assert_eq!(
        initial_payer_balance,
        bank.get_balance(&mint_keypair.pubkey())
    );
}

#[test]
fn test_deploy_last_epoch_slot() {
    agave_logger::setup();

    // Bank Setup
    let (mut genesis_config, mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
    activate_feature(
        &mut genesis_config,
        agave_feature_set::enable_loader_v4::id(),
    );
    let bank = Bank::new_for_tests(&genesis_config);

    // go to the last slot in the epoch
    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let slots_in_epoch = bank.epoch_schedule().get_slots_in_epoch(0);
    let bank = Bank::new_from_parent_with_bank_forks(
        &bank_forks,
        bank,
        &Pubkey::default(),
        slots_in_epoch - 1,
    );
    eprintln!("now at slot {} epoch {}", bank.slot(), bank.epoch());

    // deploy a program
    let payer_keypair = Keypair::new();
    let program_keypair = Keypair::new();
    let mut file = File::open("../programs/bpf_loader/test_elfs/out/noop_aligned.so").unwrap();
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    let min_program_balance = bank.get_minimum_balance_for_rent_exemption(
        LoaderV4State::program_data_offset().saturating_add(elf.len()),
    );
    let upgrade_authority_keypair = Keypair::new();

    let mut program_account = AccountSharedData::new(
        min_program_balance,
        LoaderV4State::program_data_offset().saturating_add(elf.len()),
        &solana_sdk_ids::loader_v4::id(),
    );
    let program_state = <&mut [u8; LoaderV4State::program_data_offset()]>::try_from(
        program_account
            .data_as_mut_slice()
            .get_mut(0..LoaderV4State::program_data_offset())
            .unwrap(),
    )
    .unwrap();
    let program_state = unsafe {
        std::mem::transmute::<&mut [u8; LoaderV4State::program_data_offset()], &mut LoaderV4State>(
            program_state,
        )
    };
    program_state.authority_address_or_next_version = upgrade_authority_keypair.pubkey();
    program_account
        .data_as_mut_slice()
        .get_mut(LoaderV4State::program_data_offset()..)
        .unwrap()
        .copy_from_slice(&elf);

    let payer_base_balance = LAMPORTS_PER_SOL;
    let deploy_fees = {
        let fee_calculator = genesis_config.fee_rate_governor.create_fee_calculator();
        3 * fee_calculator.lamports_per_signature
    };
    let min_payer_balance = min_program_balance.saturating_add(deploy_fees);
    bank.store_account(
        &payer_keypair.pubkey(),
        &AccountSharedData::new(
            payer_base_balance.saturating_add(min_payer_balance),
            0,
            &system_program::id(),
        ),
    );
    bank.store_account(&program_keypair.pubkey(), &program_account);
    let message = Message::new(
        &[loader_v4::deploy(
            &program_keypair.pubkey(),
            &upgrade_authority_keypair.pubkey(),
        )],
        Some(&payer_keypair.pubkey()),
    );
    let signers = &[&payer_keypair, &upgrade_authority_keypair];
    let transaction = Transaction::new(signers, message.clone(), bank.last_blockhash());
    let ret = bank.process_transaction(&transaction);
    assert!(ret.is_ok(), "ret: {ret:?}");
    goto_end_of_slot(bank.clone());

    // go to the first slot in the new epoch
    let bank = Bank::new_from_parent_with_bank_forks(
        &bank_forks,
        bank,
        &Pubkey::default(),
        slots_in_epoch,
    );
    eprintln!("now at slot {} epoch {}", bank.slot(), bank.epoch());

    let instruction = Instruction::new_with_bytes(program_keypair.pubkey(), &[], Vec::new());
    let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
    let binding = mint_keypair.insecure_clone();
    let signers = vec![&binding];
    let transaction = Transaction::new(&signers, message, bank.last_blockhash());
    let result_with_feature_enabled = bank.process_transaction(&transaction);
    assert_eq!(result_with_feature_enabled, Ok(()));
}

#[test]
fn test_loader_v3_to_v4_migration() {
    agave_logger::setup();

    // Bank Setup
    let (mut genesis_config, mint_keypair) = create_genesis_config(1_000_000 * LAMPORTS_PER_SOL);
    activate_feature(
        &mut genesis_config,
        agave_feature_set::enable_loader_v4::id(),
    );
    let bank = Bank::new_for_tests(&genesis_config);
    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let fee_calculator = genesis_config.fee_rate_governor.create_fee_calculator();
    let mut next_slot = 1;

    // deploy a program
    let mut file = File::open("../programs/bpf_loader/test_elfs/out/noop_aligned.so").unwrap();
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();

    let program_keypair = Keypair::new();
    let (programdata_address, _) = Pubkey::find_program_address(
        &[program_keypair.pubkey().as_ref()],
        &bpf_loader_upgradeable::id(),
    );
    let payer_keypair = Keypair::new();
    let upgrade_authority_keypair = Keypair::new();

    let min_program_balance =
        bank.get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::size_of_program());
    let mut program_account = AccountSharedData::new(
        min_program_balance,
        UpgradeableLoaderState::size_of_program(),
        &bpf_loader_upgradeable::id(),
    );
    program_account
        .set_state(&UpgradeableLoaderState::Program {
            programdata_address,
        })
        .unwrap();
    bank.store_account(&program_keypair.pubkey(), &program_account);

    let closed_programdata_account = AccountSharedData::new(0, 0, &bpf_loader_upgradeable::id());

    let mut uninitialized_programdata_account = AccountSharedData::new(
        0,
        UpgradeableLoaderState::size_of_programdata_metadata(),
        &bpf_loader_upgradeable::id(),
    );
    uninitialized_programdata_account
        .set_state(&UpgradeableLoaderState::Uninitialized)
        .unwrap();

    let mut finalized_programdata_account = AccountSharedData::new(
        0,
        UpgradeableLoaderState::size_of_programdata(elf.len()),
        &bpf_loader_upgradeable::id(),
    );
    finalized_programdata_account
        .set_state(&UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: None,
        })
        .unwrap();
    finalized_programdata_account
        .data_as_mut_slice()
        .get_mut(UpgradeableLoaderState::size_of_programdata_metadata()..)
        .unwrap()
        .copy_from_slice(&elf);
    let message = Message::new(
        &[
            solana_loader_v3_interface::instruction::migrate_program(
                &programdata_address,
                &program_keypair.pubkey(),
                &program_keypair.pubkey(),
            ),
            ComputeBudgetInstruction::set_compute_unit_limit(12_000),
        ],
        Some(&payer_keypair.pubkey()),
    );
    let signers = &[&payer_keypair, &program_keypair];
    let finalized_migration_transaction =
        Transaction::new(signers, message.clone(), bank.last_blockhash());

    let mut upgradeable_programdata_account = AccountSharedData::new(
        0,
        UpgradeableLoaderState::size_of_programdata(elf.len()),
        &bpf_loader_upgradeable::id(),
    );
    let min_programdata_balance =
        bank.get_minimum_balance_for_rent_exemption(upgradeable_programdata_account.data().len());
    upgradeable_programdata_account.set_lamports(min_programdata_balance);
    upgradeable_programdata_account
        .set_state(&UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(upgrade_authority_keypair.pubkey()),
        })
        .unwrap();
    upgradeable_programdata_account
        .data_as_mut_slice()
        .get_mut(UpgradeableLoaderState::size_of_programdata_metadata()..)
        .unwrap()
        .copy_from_slice(&elf);
    let message = Message::new(
        &[
            solana_loader_v3_interface::instruction::migrate_program(
                &programdata_address,
                &program_keypair.pubkey(),
                &upgrade_authority_keypair.pubkey(),
            ),
            ComputeBudgetInstruction::set_compute_unit_limit(10_000),
        ],
        Some(&payer_keypair.pubkey()),
    );
    let signers = &[&payer_keypair, &upgrade_authority_keypair];
    let upgradeable_migration_transaction =
        Transaction::new(signers, message.clone(), bank.last_blockhash());

    let payer_account = AccountSharedData::new(LAMPORTS_PER_SOL, 0, &system_program::id());
    bank.store_account(
        &programdata_address,
        &upgradeable_programdata_account.clone(),
    );
    bank.store_account(&payer_keypair.pubkey(), &payer_account);

    // Error case: Program was deployed in this block already
    let case_redeployment_cooldown = vec![
        AccountMeta::new(programdata_address, false),
        AccountMeta::new(programdata_address, false),
        AccountMeta::new(programdata_address, false),
    ];
    let message = Message::new(
        &[Instruction::new_with_bincode(
            bpf_loader_upgradeable::id(),
            &UpgradeableLoaderInstruction::Migrate,
            case_redeployment_cooldown,
        )],
        Some(&payer_keypair.pubkey()),
    );
    let signers = &[&payer_keypair];
    let transaction = Transaction::new(signers, message.clone(), bank.last_blockhash());
    let error = bank.process_transaction(&transaction).unwrap_err();
    assert_eq!(
        error,
        TransactionError::InstructionError(0, InstructionError::InvalidArgument)
    );

    let bank = Bank::new_from_parent_with_bank_forks(
        &bank_forks,
        bank.clone(),
        &Pubkey::default(),
        next_slot,
    );
    next_slot += 1;

    // All other error cases
    let case_too_few_accounts = vec![
        AccountMeta::new(programdata_address, false),
        AccountMeta::new_readonly(upgrade_authority_keypair.pubkey(), true),
    ];
    let case_readonly_programdata = vec![
        AccountMeta::new_readonly(programdata_address, false),
        AccountMeta::new(program_keypair.pubkey(), false),
        AccountMeta::new_readonly(upgrade_authority_keypair.pubkey(), true),
    ];
    let case_incorrect_authority = vec![
        AccountMeta::new(programdata_address, false),
        AccountMeta::new_readonly(upgrade_authority_keypair.pubkey(), true),
        AccountMeta::new(program_keypair.pubkey(), false),
    ];
    let case_missing_signature = vec![
        AccountMeta::new(programdata_address, false),
        AccountMeta::new(program_keypair.pubkey(), false),
        AccountMeta::new_readonly(upgrade_authority_keypair.pubkey(), false),
    ];
    let case_readonly_program = vec![
        AccountMeta::new(programdata_address, false),
        AccountMeta::new_readonly(program_keypair.pubkey(), false),
        AccountMeta::new_readonly(upgrade_authority_keypair.pubkey(), true),
    ];
    let case_program_has_wrong_owner = vec![
        AccountMeta::new(programdata_address, false),
        AccountMeta::new(upgrade_authority_keypair.pubkey(), false),
        AccountMeta::new_readonly(upgrade_authority_keypair.pubkey(), true),
    ];
    let case_incorrect_programdata_address = vec![
        AccountMeta::new(program_keypair.pubkey(), false),
        AccountMeta::new(program_keypair.pubkey(), false),
        AccountMeta::new_readonly(program_keypair.pubkey(), true),
        AccountMeta::new_readonly(upgrade_authority_keypair.pubkey(), true),
    ];
    let case_invalid_program_account = vec![
        AccountMeta::new(programdata_address, false),
        AccountMeta::new(programdata_address, false),
        AccountMeta::new_readonly(upgrade_authority_keypair.pubkey(), true),
    ];
    let case_missing_loader_v4 = vec![
        AccountMeta::new(programdata_address, false),
        AccountMeta::new(program_keypair.pubkey(), false),
        AccountMeta::new_readonly(upgrade_authority_keypair.pubkey(), true),
    ];
    for (instruction_accounts, expected_error) in [
        (case_too_few_accounts, InstructionError::MissingAccount),
        (case_readonly_programdata, InstructionError::InvalidArgument),
        (
            case_incorrect_authority,
            InstructionError::IncorrectAuthority,
        ),
        (
            case_missing_signature,
            InstructionError::MissingRequiredSignature,
        ),
        (case_readonly_program, InstructionError::InvalidArgument),
        (
            case_program_has_wrong_owner,
            InstructionError::IncorrectProgramId,
        ),
        (
            case_incorrect_programdata_address,
            InstructionError::InvalidArgument,
        ),
        (
            case_invalid_program_account,
            InstructionError::InvalidAccountData,
        ),
        (case_missing_loader_v4, InstructionError::MissingAccount),
    ] {
        let message = Message::new(
            &[Instruction::new_with_bincode(
                bpf_loader_upgradeable::id(),
                &UpgradeableLoaderInstruction::Migrate,
                instruction_accounts,
            )],
            Some(&payer_keypair.pubkey()),
        );
        let signers = &[&payer_keypair, &upgrade_authority_keypair, &program_keypair];
        let transaction = Transaction::new(
            &signers[..message.header.num_required_signatures as usize],
            message.clone(),
            bank.last_blockhash(),
        );
        let error = bank.process_transaction(&transaction).unwrap_err();
        assert_eq!(error, TransactionError::InstructionError(0, expected_error));
    }

    for (mut programdata_account, transaction, expected_execution_result) in [
        (
            closed_programdata_account,
            finalized_migration_transaction.clone(),
            Err(TransactionError::InstructionError(
                0,
                InstructionError::UnsupportedProgramId,
            )),
        ),
        (
            uninitialized_programdata_account,
            finalized_migration_transaction.clone(),
            Err(TransactionError::InstructionError(
                0,
                InstructionError::UnsupportedProgramId,
            )),
        ),
        (
            finalized_programdata_account,
            finalized_migration_transaction,
            Ok(()),
        ),
        (
            upgradeable_programdata_account,
            upgradeable_migration_transaction,
            Ok(()),
        ),
    ] {
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank.clone(),
            &Pubkey::default(),
            next_slot,
        );
        next_slot += 1;

        let min_programdata_balance =
            bank.get_minimum_balance_for_rent_exemption(programdata_account.data().len());
        programdata_account.set_lamports(min_programdata_balance);
        let payer_balance = min_program_balance
            .saturating_add(min_programdata_balance)
            .saturating_add(LAMPORTS_PER_SOL)
            .saturating_add(fee_calculator.lamports_per_signature);
        let payer_account = AccountSharedData::new(payer_balance, 0, &system_program::id());
        bank.store_account(&programdata_address, &programdata_account);
        bank.store_account(&payer_keypair.pubkey(), &payer_account);
        let result = bank.process_transaction(&transaction);
        assert!(result.is_ok(), "result: {result:?}");

        goto_end_of_slot(bank.clone());
        let bank =
            Bank::new_from_parent_with_bank_forks(&bank_forks, bank, &Pubkey::default(), next_slot);
        next_slot += 1;

        let instruction = Instruction::new_with_bytes(program_keypair.pubkey(), &[], Vec::new());
        let message = Message::new(&[instruction], Some(&mint_keypair.pubkey()));
        let binding = mint_keypair.insecure_clone();
        let transaction = Transaction::new(&[&binding], message, bank.last_blockhash());
        let execution_result = bank.process_transaction(&transaction);
        assert_eq!(execution_result, expected_execution_result);
    }
}

#[test]
fn test_blockhash_last_valid_block_height() {
    let genesis_config = GenesisConfig::default();
    let mut bank = Arc::new(Bank::new_for_tests(&genesis_config));

    // valid until MAX_PROCESSING_AGE
    let last_blockhash = bank.last_blockhash();
    for i in 1..=MAX_PROCESSING_AGE as u64 {
        goto_end_of_slot(bank.clone());
        bank = Arc::new(new_from_parent(bank));
        assert_eq!(i, bank.block_height);

        let last_valid_block_height = bank
            .get_blockhash_last_valid_block_height(&last_blockhash)
            .unwrap();
        assert_eq!(
            last_valid_block_height,
            bank.block_height + MAX_PROCESSING_AGE as u64 - i
        );
        assert!(bank.is_blockhash_valid(&last_blockhash));
    }

    // Make sure it stays in the queue until `MAX_RECENT_BLOCKHASHES`
    for i in MAX_PROCESSING_AGE + 1..=MAX_RECENT_BLOCKHASHES {
        goto_end_of_slot(bank.clone());
        bank = Arc::new(new_from_parent(bank));
        assert_eq!(bank.block_height, i as u64);

        // the blockhash is outside of the processing age, but still present in the queue,
        // so the call to get the block height still works even though it's in the past
        let last_valid_block_height = bank
            .get_blockhash_last_valid_block_height(&last_blockhash)
            .unwrap();
        assert_eq!(last_valid_block_height, MAX_PROCESSING_AGE as u64);

        // But it isn't valid for processing
        assert!(!bank.is_blockhash_valid(&last_blockhash));
    }

    // one past MAX_RECENT_BLOCKHASHES is no longer present at all
    goto_end_of_slot(bank.clone());
    bank = Arc::new(new_from_parent(bank));
    assert_eq!(
        bank.get_blockhash_last_valid_block_height(&last_blockhash),
        None
    );
    assert!(!bank.is_blockhash_valid(&last_blockhash));
}

#[test]
fn test_bank_epoch_stakes() {
    agave_logger::setup();
    let num_of_nodes: u64 = 30;
    let stakes = (1..num_of_nodes.checked_add(1).expect("Shouldn't be big")).collect::<Vec<_>>();
    let voting_keypairs = stakes
        .iter()
        .map(|_| ValidatorVoteKeypairs::new_rand())
        .collect::<Vec<_>>();
    let total_stake = stakes.iter().sum();
    let GenesisConfigInfo { genesis_config, .. } =
        create_genesis_config_with_vote_accounts(1_000_000_000, &voting_keypairs, stakes.clone());

    let bank0 = Arc::new(Bank::new_for_tests(&genesis_config));
    let mut bank1 = Bank::new_from_parent(
        bank0.clone(),
        &Pubkey::default(),
        bank0.get_slots_in_epoch(0) + 1,
    );

    let initial_epochs = bank0.epoch_stake_keys();
    assert_eq!(initial_epochs, vec![0, 1]);

    // Bank 0:

    // First query with bank's epoch. As noted by the above check, the epoch
    // stakes cache actually contains values for the _leader schedule_ epoch
    // (N + 1). Therefore, we should be able to query both.
    assert_eq!(bank0.epoch(), 0);
    assert_eq!(bank0.epoch_total_stake(0), Some(total_stake));
    assert_eq!(bank0.epoch_node_id_to_stake(0, &Pubkey::new_unique()), None);
    for (i, keypair) in voting_keypairs.iter().enumerate() {
        assert_eq!(
            bank0.epoch_node_id_to_stake(0, &keypair.node_keypair.pubkey()),
            Some(stakes[i])
        );
    }

    // Now query for epoch 1 on bank 0.
    assert_eq!(bank0.epoch().saturating_add(1), 1);
    assert_eq!(bank0.epoch_total_stake(1), Some(total_stake));
    assert_eq!(bank0.epoch_node_id_to_stake(1, &Pubkey::new_unique()), None);
    for (i, keypair) in voting_keypairs.iter().enumerate() {
        assert_eq!(
            bank0.epoch_node_id_to_stake(1, &keypair.node_keypair.pubkey()),
            Some(stakes[i])
        );
    }

    // Note using bank's `current_epoch_stake_*` methods should return the
    // same values.
    assert_eq!(bank0.get_current_epoch_total_stake(), total_stake);
    assert_eq!(
        bank0.get_current_epoch_total_stake(),
        bank0.epoch_total_stake(1).unwrap(),
    );
    assert_eq!(
        bank0.get_current_epoch_vote_accounts().len(),
        voting_keypairs.len()
    );
    assert_eq!(
        bank0.epoch_vote_accounts(1).unwrap(),
        bank0.get_current_epoch_vote_accounts(),
    );

    // Bank 1:

    // Run the same exercise. First query the bank's epoch.
    assert_eq!(bank1.epoch(), 1);
    assert_eq!(bank1.epoch_total_stake(1), Some(total_stake));
    assert_eq!(bank1.epoch_node_id_to_stake(1, &Pubkey::new_unique()), None);
    for (i, keypair) in voting_keypairs.iter().enumerate() {
        assert_eq!(
            bank1.epoch_node_id_to_stake(1, &keypair.node_keypair.pubkey()),
            Some(stakes[i])
        );
    }

    // Now query for epoch 2 on bank 1.
    assert_eq!(bank1.epoch().saturating_add(1), 2);
    assert_eq!(bank1.epoch_total_stake(2), Some(total_stake));
    assert_eq!(bank1.epoch_node_id_to_stake(2, &Pubkey::new_unique()), None);
    for (i, keypair) in voting_keypairs.iter().enumerate() {
        assert_eq!(
            bank1.epoch_node_id_to_stake(2, &keypair.node_keypair.pubkey()),
            Some(stakes[i])
        );
    }

    // Again, using bank's `current_epoch_stake_*` methods should return the
    // same values.
    assert_eq!(bank1.get_current_epoch_total_stake(), total_stake);
    assert_eq!(
        bank1.get_current_epoch_total_stake(),
        bank1.epoch_total_stake(2).unwrap(),
    );
    assert_eq!(
        bank1.get_current_epoch_vote_accounts().len(),
        voting_keypairs.len()
    );
    assert_eq!(
        bank1.epoch_vote_accounts(2).unwrap(),
        bank1.get_current_epoch_vote_accounts(),
    );

    // Setup new epoch stakes on Bank 1 for both leader schedule epochs.
    let make_new_epoch_stakes = |stake_coefficient: u64| {
        VersionedEpochStakes::new_for_tests(
            voting_keypairs
                .iter()
                .map(|keypair| {
                    let node_id = keypair.node_keypair.pubkey();
                    let authorized_voter = keypair.vote_keypair.pubkey();
                    let vote_account = VoteAccount::try_from(create_v4_account_with_authorized(
                        &node_id,
                        &authorized_voter,
                        &node_id,
                        None,
                        0,
                        100,
                    ))
                    .unwrap();
                    (authorized_voter, (stake_coefficient, vote_account))
                })
                .collect::<HashMap<_, _>>(),
            1,
        )
    };
    let stake_coefficient_epoch_1 = 100;
    let stake_coefficient_epoch_2 = 500;
    bank1.set_epoch_stakes_for_test(1, make_new_epoch_stakes(stake_coefficient_epoch_1));
    bank1.set_epoch_stakes_for_test(2, make_new_epoch_stakes(stake_coefficient_epoch_2));

    // Run the exercise again with the new stake. Now epoch 1 should have the
    // stake added for epoch 1 (`stake_coefficient_epoch_1`).
    assert_eq!(
        bank1.epoch_total_stake(1),
        Some(stake_coefficient_epoch_1 * num_of_nodes)
    );
    assert_eq!(bank1.epoch_node_id_to_stake(1, &Pubkey::new_unique()), None);
    for keypair in voting_keypairs.iter() {
        assert_eq!(
            bank1.epoch_node_id_to_stake(1, &keypair.node_keypair.pubkey()),
            Some(stake_coefficient_epoch_1)
        );
    }

    // Now query for epoch 2 on bank 1. Epoch 2 should have the stake added for
    // epoch 2 (`stake_coefficient_epoch_2`).
    assert_eq!(
        bank1.epoch_total_stake(2),
        Some(stake_coefficient_epoch_2 * num_of_nodes)
    );
    assert_eq!(bank1.epoch_node_id_to_stake(2, &Pubkey::new_unique()), None);
    for keypair in voting_keypairs.iter() {
        assert_eq!(
            bank1.epoch_node_id_to_stake(2, &keypair.node_keypair.pubkey()),
            Some(stake_coefficient_epoch_2)
        );
    }
}

/// Ensure rehash() does *not* change the bank hash if accounts are unmodified
#[test]
fn test_rehash_accounts_unmodified() {
    let ten_sol = 10 * LAMPORTS_PER_SOL;
    let genesis_config_info = genesis_utils::create_genesis_config(ten_sol);
    let bank = Bank::new_for_tests(&genesis_config_info.genesis_config);

    let lamports = 123_456_789;
    let account = AccountSharedData::new(lamports, 0, &Pubkey::default());
    let pubkey = Pubkey::new_unique();
    bank.store_account_and_update_capitalization(&pubkey, &account);

    // freeze the bank to trigger hash calculation
    bank.freeze();

    // ensure the bank hash is the same before and after rehashing
    let prev_bank_hash = bank.hash();
    bank.rehash();
    let post_bank_hash = bank.hash();
    assert_eq!(post_bank_hash, prev_bank_hash);
}

#[test]
fn test_should_use_vote_keyed_leader_schedule() {
    let genesis_config = genesis_utils::create_genesis_config(10_000).genesis_config;
    let epoch_schedule = &genesis_config.epoch_schedule;
    let create_test_bank = |bank_epoch: Epoch, feature_activation_slot: Option<Slot>| -> Bank {
        let mut bank = Bank::new_for_tests(&genesis_config);
        bank.epoch = bank_epoch;
        let mut feature_set = FeatureSet::default();
        if let Some(feature_activation_slot) = feature_activation_slot {
            let feature_activation_epoch = bank.epoch_schedule().get_epoch(feature_activation_slot);
            assert!(feature_activation_epoch <= bank_epoch);
            feature_set.activate(
                &agave_feature_set::enable_vote_address_leader_schedule::id(),
                feature_activation_slot,
            );
        }
        bank.feature_set = Arc::new(feature_set);
        bank
    };

    // Test feature activation at genesis
    let test_bank = create_test_bank(0, Some(0));
    for epoch in 0..10 {
        assert_eq!(
            test_bank.should_use_vote_keyed_leader_schedule(epoch),
            Some(true),
        );
    }

    // Test feature activated in previous epoch
    let slot_in_prev_epoch = epoch_schedule.get_first_slot_in_epoch(1);
    let test_bank = create_test_bank(2, Some(slot_in_prev_epoch));
    for epoch in 0..=(test_bank.epoch + 1) {
        assert_eq!(
            test_bank.should_use_vote_keyed_leader_schedule(epoch),
            Some(epoch >= test_bank.epoch),
        );
    }

    // Test feature activated in current epoch
    let current_epoch_slot = epoch_schedule.get_last_slot_in_epoch(1);
    let test_bank = create_test_bank(1, Some(current_epoch_slot));
    for epoch in 0..=(test_bank.epoch + 1) {
        assert_eq!(
            test_bank.should_use_vote_keyed_leader_schedule(epoch),
            Some(epoch > test_bank.epoch),
        );
    }

    // Test feature not activated yet
    let test_bank = create_test_bank(1, None);
    let max_cached_leader_schedule = epoch_schedule.get_leader_schedule_epoch(test_bank.slot());
    for epoch in 0..=(max_cached_leader_schedule + 1) {
        if epoch <= max_cached_leader_schedule {
            assert_eq!(
                test_bank.should_use_vote_keyed_leader_schedule(epoch),
                Some(false),
            );
        } else {
            assert_eq!(test_bank.should_use_vote_keyed_leader_schedule(epoch), None);
        }
    }
}

#[test]
fn test_apply_builtin_program_feature_transitions_for_new_epoch() {
    let (genesis_config, _mint_keypair) = create_genesis_config(100_000);

    let mut bank = Bank::new_for_tests(&genesis_config);
    bank.feature_set = Arc::new(FeatureSet::all_enabled());
    bank.compute_and_apply_genesis_features();

    // Overwrite precompile accounts to simulate a cluster which already added precompiles.
    for precompile in get_precompiles() {
        bank.store_account(&precompile.program_id, &AccountSharedData::default());
        // Simulate cluster which added ed25519 precompile with a system program owner
        if precompile.program_id == ed25519_program::id() {
            bank.add_precompiled_account_with_owner(
                &precompile.program_id,
                solana_system_interface::program::id(),
            );
        } else {
            bank.add_precompiled_account(&precompile.program_id);
        }
    }

    // Normally feature transitions are applied to a bank that hasn't been
    // frozen yet.  Freeze the bank early to ensure that no account changes
    // are made.
    bank.freeze();

    // Simulate crossing an epoch boundary for a new bank
    bank.compute_and_apply_new_feature_activations();
}

#[test]
fn test_startup_from_snapshot_after_precompile_transition() {
    let (genesis_config, _mint_keypair) = create_genesis_config(100_000);

    let mut bank = Bank::new_for_tests(&genesis_config);
    bank.feature_set = Arc::new(FeatureSet::all_enabled());
    bank.compute_and_apply_genesis_features();

    // Overwrite precompile accounts to simulate a cluster which already added precompiles.
    for precompile in get_precompiles() {
        bank.store_account(&precompile.program_id, &AccountSharedData::default());
        bank.add_precompiled_account(&precompile.program_id);
    }

    bank.freeze();

    // Simulate starting up from snapshot finishing the initialization for a frozen bank
    bank.compute_and_apply_features_after_snapshot_restore();
}

#[test]
fn test_parent_block_id() {
    // Setup parent bank and populate block ID.
    let (genesis_config, _mint_keypair) = create_genesis_config(100_000);
    let parent_bank = Arc::new(Bank::new_for_tests(&genesis_config));
    let parent_block_id = Some(Hash::new_unique());
    parent_bank.set_block_id(parent_block_id);

    // Create child from parent and ensure parent block ID links back to the
    // expected value.
    let child_bank = Bank::new_from_parent(parent_bank, &Pubkey::new_unique(), 1);
    assert_eq!(parent_block_id, child_bank.parent_block_id());
}
