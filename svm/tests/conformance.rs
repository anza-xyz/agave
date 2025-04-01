use {
    crate::{
        mock_bank::{MockBankCallback, MockForkGraph},
        transaction_builder::SanitizedTransactionBuilder,
    },
    agave_feature_set::{FeatureSet, FEATURE_NAMES},
    lazy_static::lazy_static,
    prost::Message,
    solana_bpf_loader_program::syscalls::create_program_runtime_environment_v1,
    solana_log_collector::LogCollector,
    solana_program_runtime::{
        execution_budget::{SVMTransactionExecutionBudget, SVMTransactionExecutionCost},
        invoke_context::{EnvironmentConfig, InvokeContext},
        loaded_programs::{ProgramCacheEntry, ProgramCacheForTxBatch},
    },
    solana_sdk::{
        account::{AccountSharedData, ReadableAccount, WritableAccount},
        clock::Clock,
        epoch_schedule::EpochSchedule,
        hash::Hash,
        instruction::AccountMeta,
        message::SanitizedMessage,
        pubkey::Pubkey,
        rent::Rent,
        signature::Signature,
        sysvar::{last_restart_slot, SysvarId},
    },
    solana_svm::{program_loader, transaction_processor::TransactionBatchProcessor},
    solana_svm_callback::TransactionProcessingCallback,
    solana_svm_conformance::proto::{AcctState, InstrEffects, InstrFixture},
    solana_timings::ExecuteTimings,
    solana_transaction_context::{
        ExecutionRecord, IndexOfAccount, InstructionAccount, TransactionAccount, TransactionContext,
    },
    std::{
        collections::{hash_map::Entry, HashMap},
        env,
        ffi::OsString,
        fs::{self, File},
        io::Read,
        path::PathBuf,
        process::Command,
        sync::{Arc, RwLock},
    },
};

mod mock_bank;
mod transaction_builder;

const fn feature_u64(feature: &Pubkey) -> u64 {
    let feature_id = feature.to_bytes();
    feature_id[0] as u64
        | ((feature_id[1] as u64) << 8)
        | ((feature_id[2] as u64) << 16)
        | ((feature_id[3] as u64) << 24)
        | ((feature_id[4] as u64) << 32)
        | ((feature_id[5] as u64) << 40)
        | ((feature_id[6] as u64) << 48)
        | ((feature_id[7] as u64) << 56)
}

lazy_static! {
    static ref INDEXED_FEATURES: HashMap<u64, Pubkey> = {
        FEATURE_NAMES
            .iter()
            .map(|(pubkey, _)| (feature_u64(pubkey), *pubkey))
            .collect()
    };
}

fn setup() -> PathBuf {
    let mut dir = env::current_dir().unwrap();
    dir.push("test-vectors");
    if !dir.exists() {
        std::println!("Cloning test-vectors ...");
        Command::new("git")
            .args([
                "clone",
                "https://github.com/firedancer-io/test-vectors",
                dir.as_os_str().to_str().unwrap(),
            ])
            .status()
            .expect("Failed to download test-vectors");

        std::println!("Checking out commit 4abb2046cf51efe809498f4fd717023684050d2f");
        Command::new("git")
            .current_dir(&dir)
            .args(["checkout", "4abb2046cf51efe809498f4fd717023684050d2f"])
            .status()
            .expect("Failed to checkout to proper test-vector commit");

        std::println!("Setup done!");
    }

    dir
}

fn cleanup() {
    let mut dir = env::current_dir().unwrap();
    dir.push("test-vectors");

    if dir.exists() {
        fs::remove_dir_all(dir).expect("Failed to delete test-vectors repository");
    }
}

#[test]
fn execute_fixtures() {
    let mut base_dir = setup();
    base_dir.push("instr");
    base_dir.push("fixtures");

    // bpf-loader tests
    base_dir.push("bpf-loader");
    run_from_folder(&base_dir);
    base_dir.pop();

    // System program tests
    base_dir.push("system");
    run_from_folder(&base_dir);

    cleanup();
}

fn run_from_folder(base_dir: &PathBuf) {
    for path in std::fs::read_dir(base_dir).unwrap() {
        let filename = path.as_ref().unwrap().file_name();
        let mut file = File::open(path.as_ref().unwrap().path()).expect("file not found");
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer).expect("Failed to read file");
        let fixture = InstrFixture::decode(buffer.as_slice()).unwrap();
        run_fixture(fixture, filename);
    }
}

fn run_fixture(fixture: InstrFixture, filename: OsString) {
    let input = fixture.input.unwrap();
    let output = fixture.output.as_ref().unwrap();

    let mut transaction_builder = SanitizedTransactionBuilder::default();
    let program_id = Pubkey::new_from_array(input.program_id.try_into().unwrap());
    let mut accounts: Vec<AccountMeta> = Vec::with_capacity(input.instr_accounts.len());
    let mut signatures: HashMap<Pubkey, Signature> =
        HashMap::with_capacity(input.instr_accounts.len());

    for item in input.instr_accounts {
        let pubkey = Pubkey::new_from_array(
            input.accounts[item.index as usize]
                .address
                .clone()
                .try_into()
                .unwrap(),
        );
        accounts.push(AccountMeta {
            pubkey,
            is_signer: item.is_signer,
            is_writable: item.is_writable,
        });
        if item.is_signer {
            signatures.insert(pubkey, Signature::new_unique());
        }
    }

    transaction_builder.create_instruction(program_id, accounts, signatures, input.data);

    let mut feature_set = FeatureSet::default();
    if let Some(features) = &input.epoch_context.as_ref().unwrap().features {
        for id in &features.features {
            if let Some(pubkey) = INDEXED_FEATURES.get(id) {
                feature_set.activate(pubkey, 0);
            }
        }
    }

    let mut fee_payer = Pubkey::new_unique();
    let mut mock_bank = MockBankCallback::default();
    {
        let mut account_data_map = mock_bank.account_shared_data.write().unwrap();
        for item in input.accounts {
            let pubkey = Pubkey::new_from_array(item.address.try_into().unwrap());
            let mut account_data = AccountSharedData::default();
            account_data.set_lamports(item.lamports);
            account_data.set_data(item.data);
            account_data.set_owner(Pubkey::new_from_array(
                item.owner.clone().try_into().unwrap(),
            ));
            account_data.set_executable(item.executable);
            account_data.set_rent_epoch(item.rent_epoch);

            account_data_map.insert(pubkey, account_data);
        }
        let mut account_data = AccountSharedData::default();
        account_data.set_lamports(800000);

        while account_data_map.contains_key(&fee_payer) {
            // The fee payer must not coincide with any of the previous accounts
            fee_payer = Pubkey::new_unique();
        }
        account_data_map.insert(fee_payer, account_data);
    }

    let Ok(transaction) = transaction_builder.build(
        Hash::default(),
        (fee_payer, Signature::new_unique()),
        false,
        true,
    ) else {
        // If we can't build a sanitized transaction,
        // the output must be a failed instruction as well
        assert_ne!(output.result, 0);
        return;
    };

    let transactions = vec![transaction];

    let compute_budget = SVMTransactionExecutionBudget {
        compute_unit_limit: input.cu_avail,
        ..SVMTransactionExecutionBudget::default()
    };

    let v1_environment =
        create_program_runtime_environment_v1(&feature_set, &compute_budget, false, false).unwrap();

    mock_bank.override_feature_set(feature_set);

    let fork_graph = Arc::new(RwLock::new(MockForkGraph {}));
    let batch_processor = TransactionBatchProcessor::new(
        42,
        2,
        Arc::downgrade(&fork_graph),
        Some(Arc::new(v1_environment)),
        None,
    );

    batch_processor
        .writable_sysvar_cache()
        .write()
        .unwrap()
        .fill_missing_entries(|pubkey, callbackback| {
            if let Some(account) = mock_bank.get_account_shared_data(pubkey) {
                if account.lamports() > 0 {
                    callbackback(account.data());
                    return;
                }
            }

            if *pubkey == Clock::id() {
                let default_clock = Clock {
                    slot: 10,
                    ..Default::default()
                };
                let clock_data = bincode::serialize(&default_clock).unwrap();
                callbackback(&clock_data);
            } else if *pubkey == EpochSchedule::id() {
                callbackback(&bincode::serialize(&EpochSchedule::default()).unwrap());
            } else if *pubkey == Rent::id() {
                callbackback(&bincode::serialize(&Rent::default()).unwrap());
            } else if *pubkey == last_restart_slot::id() {
                let slot_val = 5000_u64;
                callbackback(&bincode::serialize(&slot_val).unwrap());
            }
        });

    execute_fixture_as_instr(
        &mock_bank,
        &batch_processor,
        transactions[0].message(),
        compute_budget,
        output,
        filename,
        input.cu_avail,
    );
}

fn execute_fixture_as_instr(
    mock_bank: &MockBankCallback,
    batch_processor: &TransactionBatchProcessor<MockForkGraph>,
    sanitized_message: &SanitizedMessage,
    compute_budget: SVMTransactionExecutionBudget,
    output: &InstrEffects,
    filename: OsString,
    cu_avail: u64,
) {
    let rent = if let Ok(rent) = batch_processor.sysvar_cache().get_rent() {
        (*rent).clone()
    } else {
        Rent::default()
    };

    let transaction_accounts: Vec<TransactionAccount> = sanitized_message
        .account_keys()
        .iter()
        .map(|key| (*key, mock_bank.get_account_shared_data(key).unwrap()))
        .collect();

    let mut transaction_context = TransactionContext::new(
        transaction_accounts,
        rent,
        compute_budget.max_instruction_stack_depth,
        compute_budget.max_instruction_trace_length,
    );
    transaction_context.set_remove_accounts_executable_flag_checks(false);

    let mut loaded_programs = ProgramCacheForTxBatch::new(
        42,
        batch_processor
            .program_cache
            .read()
            .unwrap()
            .environments
            .clone(),
        None,
        2,
    );

    let program_idx = sanitized_message.instructions()[0].program_id_index as usize;
    let program_id = *sanitized_message.account_keys().get(program_idx).unwrap();

    let loaded_program = program_loader::load_program_with_pubkey(
        mock_bank,
        &batch_processor.get_environments_for_epoch(2).unwrap(),
        &program_id,
        42,
        &mut ExecuteTimings::default(),
        false,
    )
    .unwrap();

    loaded_programs.replenish(program_id, loaded_program);
    loaded_programs.replenish(
        solana_system_program::id(),
        Arc::new(ProgramCacheEntry::new_builtin(
            0u64,
            0usize,
            solana_system_program::system_processor::Entrypoint::vm,
        )),
    );

    let log_collector = LogCollector::new_ref();

    let sysvar_cache = &batch_processor.sysvar_cache();
    #[allow(deprecated)]
    let (blockhash, lamports_per_signature) = batch_processor
        .sysvar_cache()
        .get_recent_blockhashes()
        .ok()
        .and_then(|x| (*x).last().cloned())
        .map(|x| (x.blockhash, x.fee_calculator.lamports_per_signature))
        .unwrap_or_default();

    let env_config = EnvironmentConfig::new(
        blockhash,
        lamports_per_signature,
        mock_bank,
        mock_bank.feature_set.clone(),
        sysvar_cache,
    );

    let mut invoke_context = InvokeContext::new(
        &mut transaction_context,
        &mut loaded_programs,
        env_config,
        Some(log_collector.clone()),
        compute_budget,
        SVMTransactionExecutionCost::default(),
    );

    let mut instruction_accounts: Vec<InstructionAccount> =
        Vec::with_capacity(sanitized_message.instructions()[0].accounts.len());

    for (instruction_acct_idx, index_txn) in sanitized_message.instructions()[0]
        .accounts
        .iter()
        .enumerate()
    {
        let index_in_callee = sanitized_message.instructions()[0]
            .accounts
            .get(0..instruction_acct_idx)
            .unwrap()
            .iter()
            .position(|idx| *idx == *index_txn)
            .unwrap_or(instruction_acct_idx);

        instruction_accounts.push(InstructionAccount {
            index_in_transaction: *index_txn as IndexOfAccount,
            index_in_caller: *index_txn as IndexOfAccount,
            index_in_callee: index_in_callee as IndexOfAccount,
            is_signer: sanitized_message.is_signer(*index_txn as usize),
            is_writable: sanitized_message.is_writable(*index_txn as usize),
        });
    }

    let mut compute_units_consumed = 0u64;
    let mut timings = ExecuteTimings::default();
    let result = invoke_context.process_instruction(
        &sanitized_message.instructions()[0].data,
        &instruction_accounts,
        &[program_idx as IndexOfAccount],
        &mut compute_units_consumed,
        &mut timings,
    );

    if output.result == 0 {
        assert!(
            result.is_ok(),
            "Instruction execution was NOT successful, but should have been: {:?}",
            filename
        );
    } else {
        assert!(
            result.is_err(),
            "Instruction execution was successful, but should NOT have been: {:?}",
            filename
        );
        return;
    }

    let ExecutionRecord {
        accounts,
        return_data,
        ..
    } = transaction_context.into();

    verify_accounts_and_data(
        &accounts,
        output,
        compute_units_consumed,
        cu_avail,
        &return_data.data,
        filename,
    );
}

fn verify_accounts_and_data(
    accounts: &[TransactionAccount],
    output: &InstrEffects,
    consumed_units: u64,
    cu_avail: u64,
    return_data: &Vec<u8>,
    filename: OsString,
) {
    // The input created by firedancer is malformed in that there may be repeated accounts in the
    // instruction execution output. This happens because the set system program as the program ID,
    // as pass it as an account to be modified in the instruction.
    let mut idx_map: HashMap<Pubkey, Vec<usize>> = HashMap::new();
    for (idx, item) in accounts.iter().enumerate() {
        match idx_map.entry(item.0) {
            Entry::Occupied(mut this) => {
                this.get_mut().push(idx);
            }
            Entry::Vacant(this) => {
                this.insert(vec![idx]);
            }
        }
    }

    for item in &output.modified_accounts {
        let pubkey = Pubkey::new_from_array(item.address.clone().try_into().unwrap());
        let indexes = *idx_map
            .get(&pubkey)
            .as_ref()
            .expect("Account not in expected results");

        let mut error: Option<String> = Some("err".to_string());
        for idx in indexes {
            let received_data = &accounts[*idx].1;
            let check_result = check_account(received_data, item, &filename);

            if error.is_some() && check_result.is_none() {
                // If at least one of the accounts pass the check, we have no error.
                error = None;
            } else if error.is_some() && check_result.is_some() {
                error = check_result;
            }
        }

        if let Some(error) = error {
            panic!("{}", error);
        }
    }

    assert_eq!(
        consumed_units,
        cu_avail.saturating_sub(output.cu_avail),
        "Execution units differs in case: {:?}",
        filename
    );

    if return_data.is_empty() {
        assert!(output.return_data.is_empty());
    } else {
        assert_eq!(&output.return_data, return_data);
    }
}

fn check_account(
    received: &AccountSharedData,
    expected: &AcctState,
    filename: &OsString,
) -> Option<String> {
    macro_rules! format_args {
        ($received:expr, $expected:expr) => {
            format!("received: {:?}\nexpected: {:?}", $received, $expected).as_str()
        };
    }

    if received.lamports() != expected.lamports {
        return Some(
            format!("Lamports differ in case: {:?}\n", filename)
                + format_args!(received.lamports(), expected.lamports),
        );
    }

    if received.data() != expected.data.as_slice() {
        return Some(
            format!("Account data differs in case: {:?}\n", filename)
                + format_args!(received.data(), expected.data.as_slice()),
        );
    }

    let expected_owner = Pubkey::new_from_array(expected.owner.clone().try_into().unwrap());
    if received.owner() != &expected_owner {
        return Some(
            format!("Account owner differs in case: {:?}\n", filename)
                + format_args!(received.owner(), expected_owner),
        );
    }

    if received.executable() != expected.executable {
        return Some(
            format!("Executable boolean differs in case: {:?}\n", filename)
                + format_args!(received.executable(), expected.executable),
        );
    }

    // u64::MAX means we are not considering the epoch
    if received.rent_epoch() != u64::MAX
        && expected.rent_epoch != u64::MAX
        && received.rent_epoch() != expected.rent_epoch
    {
        return Some(
            format!("Rent epoch differs in case: {:?}\n", filename)
                + format_args!(received.rent_epoch(), expected.rent_epoch),
        );
    }

    None
}
