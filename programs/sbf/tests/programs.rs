#![cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
#![allow(clippy::clone_on_copy)]
#![allow(clippy::needless_range_loop)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::cmp_owned)]
#![allow(clippy::match_like_matches_macro)]
#![allow(clippy::unnecessary_cast)]
#![allow(clippy::uninlined_format_args)]

#[cfg(not(feature = "sbf_sanity_list"))]
use solana_program_runtime::execution_budget::MAX_COMPUTE_UNIT_LIMIT;
#[cfg(feature = "sbf_rust")]
use {
    agave_feature_set::{self as feature_set, FeatureSet},
    agave_reserved_account_keys::ReservedAccountKeys,
    borsh::{BorshDeserialize, BorshSerialize, from_slice, to_vec},
    solana_account::ReadableAccount,
    solana_account_info::MAX_PERMITTED_DATA_INCREASE,
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_instruction::{AccountMeta, Instruction, error::InstructionError},
    solana_keypair::Keypair,
    solana_loader_v3_interface::{
        instruction as loader_v3_instruction, state::UpgradeableLoaderState,
    },
    solana_message::{Message, SanitizedMessage},
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sbf_rust_invoke_dep::*,
    solana_sbf_rust_realloc_dep::*,
    solana_sbf_rust_realloc_invoke_dep::*,
    solana_sdk_ids::sysvar::{self as sysvar, clock},
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable},
    solana_signer::Signer,
    solana_svm::conformance::{
        setup::sysvar_cache_from_accounts,
        txn::{context::TxnContext, effects::TxnEffects, harness::execute_txn},
    },
    solana_svm_feature_set::SVMFeatureSet,
    solana_svm_type_overrides::rand,
    solana_system_interface::{MAX_PERMITTED_DATA_LENGTH, program as system_program},
    solana_transaction_error::TransactionError,
    std::{assert_eq, str::FromStr},
    test_case::{test_case, test_matrix},
};
#[cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
use {
    solana_account::Account,
    solana_program_runtime::{loaded_programs::ProgramCacheForTxBatch, sysvar_cache::SysvarCache},
    solana_sdk_ids::sysvar::rent,
    solana_svm::conformance::{
        instr::{context::InstrContext, harness::execute_instr},
        programs::{
            add_program_to_program_cache, keyed_account_for_bpf_loader_program,
            keyed_account_for_bpf_loader_upgradeable_program,
            keyed_account_for_compute_budget_program, keyed_account_for_system_program,
            new_program_cache_with_builtins,
        },
    },
    std::{fs::File, io::Read, path::PathBuf},
};

#[cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
fn load_program_elf(program_name: &str) -> Vec<u8> {
    let sbf_out_dir =
        std::env::var("SBF_OUT_DIR").expect("SBF_OUT_DIR must be set to locate program ELFs");
    let path = PathBuf::from(sbf_out_dir).join(format!("{program_name}.so"));
    let mut file = File::open(&path)
        .unwrap_or_else(|e| panic!("Failed to open file {}: {}", path.display(), e));
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .unwrap_or_else(|e| panic!("Failed to read file {}: {}", path.display(), e));
    data
}

#[cfg(feature = "sbf_rust")]
fn default_program_cache() -> solana_program_runtime::loaded_programs::ProgramCacheForTxBatch {
    new_program_cache_with_builtins(/* slot */ 1)
}

fn default_program_cache_with_program(
    program_id: &Pubkey,
    program_elf: &[u8],
    feature_set: &SVMFeatureSet,
) -> ProgramCacheForTxBatch {
    let mut program_cache = default_program_cache();
    add_program_to_program_cache(
        &mut program_cache,
        program_id,
        &bpf_loader_upgradeable::id(),
        program_elf,
        feature_set,
    );
    program_cache
}

fn upgradeable_program_accounts(program_id: &Pubkey, program_elf: &[u8]) -> Vec<(Pubkey, Account)> {
    solana_program_binaries::bpf_loader_upgradeable_program_accounts(
        program_id,
        program_elf,
        &Rent::default(),
    )
    .into()
}

fn program_accounts(
    program_id: &Pubkey,
    program_elf: &[u8],
    loader_id: Pubkey,
) -> Vec<(Pubkey, Account)> {
    if bpf_loader_upgradeable::check_id(&loader_id) {
        return upgradeable_program_accounts(program_id, program_elf);
    }

    assert!(bpf_loader::check_id(&loader_id) || bpf_loader_deprecated::check_id(&loader_id));
    let (program_id, mut account) = solana_program_binaries::bpf_loader_program_account(
        program_id,
        program_elf,
        &Rent::default(),
    );
    account.owner = loader_id;
    vec![(program_id, account)]
}

#[cfg(feature = "sbf_rust")]
fn default_sysvar_cache() -> SysvarCache {
    let mut sysvar_cache = SysvarCache::default();
    sysvar_cache.fill_missing_entries(|pubkey, callback| {
        if pubkey == &rent::id() {
            let rent = Rent::default();
            let rent_data = bincode::serialize(&rent).unwrap();
            callback(&rent_data);
        } else if pubkey == &clock::id() {
            let clock = solana_clock::Clock::default();
            let clock_data = bincode::serialize(&clock).unwrap();
            callback(&clock_data);
        }
    });
    sysvar_cache
}

#[cfg(feature = "sbf_rust")]
const LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST: u32 = 64 * 1024 * 1024;

#[test]
#[cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
fn test_program_sbf_sanity() {
    agave_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[
            ("alloc", true),
            ("alt_bn128", true),
            ("alt_bn128_compression", true),
            ("big_mod_exp", true),
            ("sbf_to_sbf", true),
            ("float", true),
            ("multiple_static", true),
            ("noop", true),
            ("noop++", true),
            ("panic", false),
            ("poseidon", true),
            ("relative_call", true),
            ("return_data", true),
            ("sanity", true),
            ("sanity++", true),
            ("secp256k1_recover", true),
            ("sha", true),
            ("stdlib", true),
            ("struct_pass", true),
            ("struct_ret", true),
        ]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[
            ("solana_sbf_rust_128bit", true),
            ("solana_sbf_rust_alloc", true),
            ("solana_sbf_rust_alt_bn128", true),
            ("solana_sbf_rust_alt_bn128_compression", true),
            ("solana_sbf_rust_big_mod_exp", true),
            ("solana_sbf_rust_curve25519", true),
            ("solana_sbf_rust_custom_heap", true),
            ("solana_sbf_rust_dep_crate", true),
            ("solana_sbf_rust_external_spend", false),
            ("solana_sbf_rust_iter", true),
            ("solana_sbf_rust_many_args", true),
            ("solana_sbf_rust_mem", true),
            ("solana_sbf_rust_membuiltins", true),
            ("solana_sbf_rust_noop", true),
            ("solana_sbf_rust_panic", false),
            ("solana_sbf_rust_param_passing", true),
            ("solana_sbf_rust_poseidon", true),
            ("solana_sbf_rust_rand", true),
            ("solana_sbf_rust_remaining_compute_units", true),
            ("solana_sbf_rust_sanity", true),
            ("solana_sbf_rust_secp256k1_recover", true),
            ("solana_sbf_rust_sha", true),
        ]);
    }

    #[cfg(all(feature = "sbf_rust", feature = "sbf_sanity_list"))]
    {
        // This code generates the list of sanity programs for a CI job to build with
        // cargo-build-sbf and ensure it is working correctly.
        use std::{env, fs::File, io::Write};
        let current_dir = env::current_dir().unwrap();
        let mut file =
            File::create(current_dir.join("target").join("sanity_programs.txt")).unwrap();
        for program in programs.iter() {
            writeln!(file, "{}", program.0.trim_start_matches("solana_sbf_rust_"))
                .expect("Failed to write to file");
        }
    }

    #[cfg(not(feature = "sbf_sanity_list"))]
    for program in programs.iter() {
        println!("Test program: {:?}", program.0);

        let program_elf = load_program_elf(program.0);
        let program_id = Pubkey::new_unique();

        let feature_set = SVMFeatureSet::all_enabled();

        let pubkey1 = Pubkey::new_unique();
        let pubkey2 = Pubkey::new_unique();

        let account_metas = vec![
            AccountMeta::new(pubkey1, true),
            AccountMeta::new(pubkey2, false),
        ];
        let instruction = Instruction::new_with_bytes(program_id, &[1], account_metas);

        let mut accounts = upgradeable_program_accounts(&program_id, &program_elf);
        accounts.extend([(pubkey1, Account::default()), (pubkey2, Account::default())]);

        let mut program_cache =
            default_program_cache_with_program(&program_id, &program_elf, &feature_set);

        let context = InstrContext {
            feature_set,
            accounts,
            instruction,
            cu_avail: MAX_COMPUTE_UNIT_LIMIT as u64, // Widen to 1.4 M
        };

        let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());

        let result = match effects.result {
            Some(err) => Err(err),
            None => Ok(()),
        };

        if program.1 {
            assert!(result.is_ok(), "{result:?}");
        } else {
            assert!(result.is_err(), "{result:?}");
        }
    }
}

#[test]
#[cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
fn test_program_sbf_loader_deprecated() {
    agave_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[("deprecated_loader")]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[("solana_sbf_rust_deprecated_loader")]);
    }

    for program in programs.iter() {
        println!("Test program: {:?}", program);

        let program_elf = load_program_elf(program);
        let program_id = Pubkey::new_unique();

        let feature_set = SVMFeatureSet::all_enabled();

        let pubkey = Pubkey::new_unique();
        let accounts = vec![
            (
                program_id,
                Account {
                    lamports: 1,
                    data: vec![],
                    owner: bpf_loader_deprecated::id(),
                    executable: true,
                    rent_epoch: u64::MAX,
                },
            ),
            (pubkey, Account::default()),
        ];

        let mut program_cache = default_program_cache();
        add_program_to_program_cache(
            &mut program_cache,
            &program_id,
            &bpf_loader_deprecated::id(),
            &program_elf,
            &feature_set,
        );

        let account_metas = vec![AccountMeta::new(pubkey, true)];
        let instruction = Instruction::new_with_bytes(program_id, &[255], account_metas);

        let context = InstrContext::new_with_default_budget(feature_set, accounts, instruction);

        let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());

        assert!(effects.result.is_none());
    }
}

#[test]
#[cfg(all(feature = "sbf_rust", not(feature = "sbpf-v3")))]
// In SBPFv3, we don't have a verification step for undefined syscalls, and we don't do dynamic
// symbol resolution, so this test would pass.
fn test_sol_alloc_free_no_longer_deployable_with_upgradeable_loader() {
    agave_logger::setup();

    // Populate loader account with `solana_sbf_rust_deprecated_loader` elf, which
    // depends on `sol_alloc_free_` syscall. This can be verified with
    // $ elfdump solana_sbf_rust_deprecated_loader.so
    // : 0000000000001ab8  000000070000000a R_BPF_64_32            0000000000000000 sol_alloc_free_
    // In the symbol table, there is `sol_alloc_free_`.
    // In fact, `sol_alloc_free_` is called from sbf allocator, which is originated from
    // AccountInfo::realloc() in the program code.

    let program_elf = load_program_elf("solana_sbf_rust_deprecated_loader");
    let program_id = Pubkey::new_unique();
    let authority_pubkey = Pubkey::new_unique();
    let buffer_pubkey = Pubkey::new_unique();
    let payer_pubkey = Pubkey::new_unique();

    let feature_set = SVMFeatureSet::all_enabled();

    let (programdata_address, _) =
        Pubkey::find_program_address(&[program_id.as_ref()], &bpf_loader_upgradeable::id());

    // Build the buffer account: UpgradeableLoaderState::Buffer header + ELF
    let buffer_state = UpgradeableLoaderState::Buffer {
        authority_address: Some(authority_pubkey),
    };
    let mut buffer_data = bincode::serialize(&buffer_state).unwrap();
    buffer_data.extend_from_slice(&program_elf);

    // Build sysvar accounts for the deploy instruction
    let rent_data = bincode::serialize(&Rent::default()).unwrap();
    let clock_data = bincode::serialize(&solana_clock::Clock::default()).unwrap();

    let accounts = vec![
        (
            payer_pubkey,
            Account {
                lamports: 1_000_000_000,
                ..Account::default()
            },
        ),
        (programdata_address, Account::default()),
        (
            program_id,
            Account {
                lamports: 1_000_000_000,
                data: vec![0u8; UpgradeableLoaderState::size_of_program()],
                owner: bpf_loader_upgradeable::id(),
                ..Account::default()
            },
        ),
        (
            buffer_pubkey,
            Account {
                lamports: 1_000_000_000,
                data: buffer_data,
                owner: bpf_loader_upgradeable::id(),
                ..Account::default()
            },
        ),
        (
            rent::id(),
            Account {
                lamports: 1,
                data: rent_data,
                owner: sysvar::id(),
                ..Account::default()
            },
        ),
        (
            clock::id(),
            Account {
                lamports: 1,
                data: clock_data,
                owner: sysvar::id(),
                ..Account::default()
            },
        ),
        keyed_account_for_system_program(),
        solana_svm::conformance::programs::keyed_account_for_bpf_loader_upgradeable_program(),
        (authority_pubkey, Account::default()),
    ];

    let mut program_cache = new_program_cache_with_builtins(0);

    // Build the deploy instruction (DeployWithMaxDataLen only, skip CreateAccount)
    #[allow(deprecated)]
    let instruction = loader_v3_instruction::deploy_with_max_program_len(
        &payer_pubkey,
        &program_id,
        &buffer_pubkey,
        &authority_pubkey,
        1,
        program_elf.len() * 2,
    )
    .unwrap()
    .pop()
    .unwrap();

    let context = InstrContext::new_with_default_budget(feature_set, accounts, instruction);

    // Expect that deployment to fail. B/C during deployment, there is an elf
    // verification step, which uses the runtime to look up relocatable symbols
    // in elf inside syscall table. In this case, `sol_alloc_free_` can't be
    // found in syscall table. Hence, the verification fails and the deployment
    // fails.
    let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());

    assert_eq!(effects.result, Some(InstructionError::InvalidAccountData));
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_duplicate_accounts() {
    agave_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[("dup_accounts")]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[("solana_sbf_rust_dup_accounts")]);
    }

    for program in programs.iter() {
        println!("Test program: {:?}", program);

        let program_elf = load_program_elf(program);
        let program_id = Pubkey::new_unique();
        let feature_set = SVMFeatureSet::all_enabled();
        let mut program_cache =
            default_program_cache_with_program(&program_id, &program_elf, &feature_set);

        let payer_pubkey = Pubkey::new_unique();
        let payee_pubkey = Pubkey::new_unique();
        let pubkey = Pubkey::new_unique();
        let account = Account::new(10, 1, &program_id);

        let account_metas = vec![
            AccountMeta::new(payer_pubkey, true),
            AccountMeta::new(payee_pubkey, false),
            AccountMeta::new(pubkey, false),
            AccountMeta::new(pubkey, false),
        ];
        let program_accounts = upgradeable_program_accounts(&program_id, &program_elf);

        let mut execute = |data: &[u8]| {
            let mut accounts = program_accounts.clone();
            accounts.extend([
                (payer_pubkey, Account::new(100, 0, &Pubkey::default())),
                (payee_pubkey, Account::new(10, 1, &program_id)),
                (pubkey, account.clone()),
            ]);
            let instruction = Instruction::new_with_bytes(program_id, data, account_metas.clone());
            let context = InstrContext::new_with_default_budget(feature_set, accounts, instruction);
            execute_instr(&context, &mut program_cache, &default_sysvar_cache())
        };

        let effects = execute(&[1]);
        assert!(effects.result.is_none());
        let data = effects.get_account(&pubkey).unwrap().data.clone();
        assert_eq!(data[0], 1);

        let effects = execute(&[2]);
        assert!(effects.result.is_none());
        let data = effects.get_account(&pubkey).unwrap().data.clone();
        assert_eq!(data[0], 2);

        let effects = execute(&[3]);
        assert!(effects.result.is_none());
        let data = effects.get_account(&pubkey).unwrap().data.clone();
        assert_eq!(data[0], 3);

        let effects = execute(&[4]);
        assert!(effects.result.is_none());
        let lamports = effects.get_account(&pubkey).unwrap().lamports;
        assert_eq!(lamports, 11);

        let effects = execute(&[5]);
        assert!(effects.result.is_none());
        let lamports = effects.get_account(&pubkey).unwrap().lamports;
        assert_eq!(lamports, 12);

        let effects = execute(&[6]);
        assert!(effects.result.is_none());
        let lamports = effects.get_account(&pubkey).unwrap().lamports;
        assert_eq!(lamports, 13);

        let pubkey = Pubkey::new_unique();
        let account_metas = vec![
            AccountMeta::new(payer_pubkey, true),
            AccountMeta::new(payee_pubkey, false),
            AccountMeta::new(pubkey, false),
            AccountMeta::new_readonly(pubkey, true),
            AccountMeta::new_readonly(program_id, false),
        ];
        let mut accounts = upgradeable_program_accounts(&program_id, &program_elf);
        accounts.extend([
            (payer_pubkey, Account::new(100, 0, &Pubkey::default())),
            (payee_pubkey, Account::new(10, 1, &program_id)),
            (pubkey, Account::new(10, 1, &program_id)),
        ]);
        let instruction = Instruction::new_with_bytes(program_id, &[7], account_metas);
        let context = InstrContext::new_with_default_budget(feature_set, accounts, instruction);
        let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());
        assert!(effects.result.is_none());
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_error_handling() {
    agave_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[("error_handling")]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[("solana_sbf_rust_error_handling")]);
    }

    for program in programs.iter() {
        println!("Test program: {:?}", program);

        let program_elf = load_program_elf(program);
        let program_id = Pubkey::new_unique();

        let feature_set = SVMFeatureSet::all_enabled();

        let pubkey1 = Pubkey::new_unique();

        let mut accounts = upgradeable_program_accounts(&program_id, &program_elf);
        accounts.push((pubkey1, Account::default()));

        let mut program_cache =
            default_program_cache_with_program(&program_id, &program_elf, &feature_set);

        // Helper to execute an instruction with the given data byte.
        let mut execute = |data: &[u8]| {
            let account_metas = vec![AccountMeta::new(pubkey1, true)];
            let instruction = Instruction::new_with_bytes(program_id, data, account_metas);

            let context =
                InstrContext::new_with_default_budget(feature_set, accounts.clone(), instruction);

            execute_instr(&context, &mut program_cache, &default_sysvar_cache())
        };

        let effects = execute(&[1]);
        assert!(effects.result.is_none());

        let effects = execute(&[2]);
        assert_eq!(effects.result, Some(InstructionError::InvalidAccountData));

        let effects = execute(&[3]);
        assert_eq!(effects.result, Some(InstructionError::Custom(0)));

        let effects = execute(&[4]);
        assert_eq!(effects.result, Some(InstructionError::Custom(42)));

        let effects = execute(&[5]);
        assert!(
            effects.result == Some(InstructionError::InvalidInstructionData)
                || effects.result == Some(InstructionError::InvalidError)
        );

        let effects = execute(&[6]);
        assert!(
            effects.result == Some(InstructionError::InvalidInstructionData)
                || effects.result == Some(InstructionError::InvalidError)
        );

        let effects = execute(&[7]);
        assert!(
            effects.result == Some(InstructionError::InvalidInstructionData)
                || effects.result == Some(InstructionError::AccountBorrowFailed)
        );

        let effects = execute(&[8]);
        assert_eq!(
            effects.result,
            Some(InstructionError::InvalidInstructionData)
        );

        let effects = execute(&[9]);
        assert_eq!(
            effects.result,
            Some(InstructionError::MaxSeedLengthExceeded)
        );
    }
}

#[test]
#[cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
fn test_return_data_and_log_data_syscall() {
    agave_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[("log_data")]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[("solana_sbf_rust_log_data")]);
    }

    for program in programs.iter() {
        println!("Test program: {:?}", program);

        let program_elf = load_program_elf(program);
        let program_id = Pubkey::new_unique();

        let feature_set = SVMFeatureSet::all_enabled();

        let pubkey = Pubkey::new_unique();
        let mut accounts = upgradeable_program_accounts(&program_id, &program_elf);
        accounts.push((pubkey, Account::default()));

        let mut program_cache =
            default_program_cache_with_program(&program_id, &program_elf, &feature_set);

        let account_metas = vec![AccountMeta::new(pubkey, true)];
        let instruction =
            Instruction::new_with_bytes(program_id, &[1, 2, 3, 0, 4, 5, 6], account_metas);

        let context = InstrContext::new_with_default_budget(feature_set, accounts, instruction);

        let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());

        assert!(effects.result.is_none());

        assert!(
            effects
                .logs
                .iter()
                .any(|log| log == "Program data: AQID BAUG")
        );

        assert_eq!(effects.return_data, vec![0x08, 0x01, 0x44]);

        assert!(
            effects
                .logs
                .iter()
                .any(|log| log == &format!("Program return: {} CAFE", program_id))
        );
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_invoke_sanity() {
    #[derive(Debug)]
    #[allow(dead_code)]
    enum Languages {
        C,
        Rust,
    }

    let mut programs = vec![];

    #[cfg(feature = "sbf_c")]
    {
        programs.push((Languages::C, "invoke", "invoked", "noop"));
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.push((
            Languages::Rust,
            "solana_sbf_rust_invoke",
            "solana_sbf_rust_invoked",
            "solana_sbf_rust_noop",
        ));
    }

    for program in programs.iter() {
        println!("Test program: {:?}", program);

        let mut fixture = ProgramSbfTxnFixture::new(vec![
            ProgramSpec::upgradeable(program.1),
            ProgramSpec::upgradeable(program.2),
            ProgramSpec::upgradeable(program.3),
        ]);
        let mint_keypair = fixture.add_account(Some(Account::new(50, 0, &system_program::id())));
        let argument_keypair = fixture.add_account(Some(Account {
            lamports: 42,
            data: vec![0; 100],
            owner: fixture.program_ids[0],
            executable: false,
            rent_epoch: 0,
        }));
        let invoked_argument_keypair = fixture.add_account(Some(Account {
            lamports: 20,
            data: vec![0; 10],
            owner: fixture.program_ids[1],
            executable: false,
            rent_epoch: 0,
        }));
        let from_keypair = fixture.add_account(Some(Account::new(84, 0, &system_program::id())));
        let unexecutable_program_keypair = fixture.add_account(Some(Account {
            lamports: 1,
            data: Vec::new(),
            owner: bpf_loader::id(),
            executable: false,
            rent_epoch: 0,
        }));

        let invoke_program_id = fixture.program_ids[0];
        let invoked_program_id = fixture.program_ids[1];
        let noop_program_id = fixture.program_ids[2];

        let mut feature_set = fixture.feature_set.clone();
        let mut program_cache = fixture.program_cache.clone();

        let (derived_key1, bump_seed1) =
            Pubkey::find_program_address(&[b"You pass butter"], &invoke_program_id);
        let (derived_key2, bump_seed2) =
            Pubkey::find_program_address(&[b"Lil'", b"Bits"], &invoked_program_id);
        let (derived_key3, bump_seed3) =
            Pubkey::find_program_address(&[derived_key2.as_ref()], &invoked_program_id);

        fixture.add_accounts_with_pubkeys([
            (
                derived_key1,
                Account {
                    lamports: 0,
                    data: Vec::new(),
                    owner: system_program::id(),
                    executable: false,
                    rent_epoch: 0,
                },
            ),
            (
                derived_key2,
                Account {
                    lamports: 0,
                    data: Vec::new(),
                    owner: system_program::id(),
                    executable: false,
                    rent_epoch: 0,
                },
            ),
            (
                derived_key3,
                Account {
                    lamports: 0,
                    data: Vec::new(),
                    owner: system_program::id(),
                    executable: false,
                    rent_epoch: 0,
                },
            ),
            (
                solana_sdk_ids::ed25519_program::id(),
                Account {
                    lamports: 1,
                    data: Vec::new(),
                    owner: solana_sdk_ids::native_loader::id(),
                    executable: true,
                    rent_epoch: 0,
                },
            ),
            keyed_account_for_system_program(),
            keyed_account_for_compute_budget_program(),
        ]);

        let account_metas = vec![
            AccountMeta::new(mint_keypair.pubkey(), true),
            AccountMeta::new(argument_keypair.pubkey(), true),
            AccountMeta::new_readonly(invoked_program_id, false),
            AccountMeta::new(invoked_argument_keypair.pubkey(), true),
            AccountMeta::new_readonly(invoked_program_id, false),
            AccountMeta::new(argument_keypair.pubkey(), true),
            AccountMeta::new(derived_key1, false),
            AccountMeta::new(derived_key2, false),
            AccountMeta::new_readonly(derived_key3, false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new(from_keypair.pubkey(), true),
            AccountMeta::new_readonly(solana_sdk_ids::ed25519_program::id(), false),
            AccountMeta::new_readonly(invoke_program_id, false),
            AccountMeta::new_readonly(unexecutable_program_keypair.pubkey(), false),
            AccountMeta::new_readonly(noop_program_id, false),
        ];

        let cpi_programs_from_logs = |logs: &[String]| {
            logs.iter()
                .filter_map(|log| {
                    let log = log.strip_prefix("Program ")?;
                    let (program_id, depth) = log.split_once(" invoke [")?;
                    let depth = depth.strip_suffix(']')?.parse::<usize>().ok()?;
                    (depth > 1).then(|| Pubkey::from_str(program_id).unwrap())
                })
                .collect::<Vec<_>>()
        };

        let do_invoke = |test: u8,
                         additional_instructions: &[Instruction],
                         transaction_accounts: Vec<(Pubkey, Account)>,
                         feature_set: &FeatureSet,
                         program_cache: &mut ProgramCacheForTxBatch| {
            let instruction_data = &[test, bump_seed1, bump_seed2, bump_seed3];
            let mut instructions = vec![Instruction::new_with_bytes(
                invoke_program_id,
                instruction_data,
                account_metas.clone(),
            )];
            instructions.extend_from_slice(additional_instructions);
            let sanitized_message = SanitizedMessage::try_from_legacy_message(
                Message::new(&instructions, Some(&mint_keypair.pubkey())),
                &ReservedAccountKeys::empty_key_set(),
            )
            .unwrap();
            let context = TxnContext {
                feature_set: feature_set.clone(),
                accounts: transaction_accounts,
                message: sanitized_message,
                nonce_fields: None,
                cu_avail: 1_400_000,
            };

            execute_txn(&context, program_cache, &default_sysvar_cache())
        };

        // success cases
        let do_invoke_success =
            |test: u8,
             additional_instructions: &[Instruction],
             expected_invoked_programs: &[Pubkey],
             accounts: &mut Vec<(Pubkey, Account)>,
             feature_set: &FeatureSet,
             program_cache: &mut ProgramCacheForTxBatch| {
                println!("Running success test #{:?}", test);

                let effects = do_invoke(
                    test,
                    additional_instructions,
                    accounts.clone(),
                    feature_set,
                    program_cache,
                );
                assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);

                let invoked_programs = cpi_programs_from_logs(&effects.logs);
                assert_eq!(invoked_programs, expected_invoked_programs);

                *accounts = effects.resulting_accounts;
            };

        do_invoke_success(
            TEST_SUCCESS,
            &[Instruction::new_with_bytes(noop_program_id, &[], vec![])],
            match program.0 {
                Languages::C => vec![
                    system_program::id(),
                    system_program::id(),
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                    invoked_program_id,
                ],
                Languages::Rust => {
                    let mut invoked_programs = vec![system_program::id(), system_program::id()];
                    invoked_programs.extend([invoked_program_id; 19]);
                    invoked_programs.push(system_program::id());
                    invoked_programs.extend([invoked_program_id; 2]);
                    invoked_programs
                }
            }
            .as_ref(),
            &mut fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        // With SIMD-0268 enabled, eight nested invokes should pass.
        assert!(feature_set.snapshot().raise_cpi_nesting_limit_to_8);

        // Reset the account balances for `ARGUMENT` and `INVOKED_ARGUMENT`.
        fixture.replace_account(
            argument_keypair.pubkey(),
            Account {
                lamports: 42,
                data: vec![0; 100],
                owner: invoke_program_id,
                executable: false,
                rent_epoch: 0,
            },
        );
        fixture.replace_account(
            invoked_argument_keypair.pubkey(),
            Account {
                lamports: 20,
                data: vec![0; 10],
                owner: invoked_program_id,
                executable: false,
                rent_epoch: 0,
            },
        );

        do_invoke_success(
            TEST_NESTED_INVOKE_SIMD_0268_OK,
            &[],
            &[invoked_program_id; 16], // 16, 8 for each invoke
            &mut fixture.accounts,
            &feature_set,
            &mut program_cache,
        );
        do_invoke_success(
            TEST_MAX_ACCOUNT_INFOS_OK,
            &[],
            std::slice::from_ref(&invoked_program_id),
            &mut fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_success(
            TEST_CU_USAGE_MINIMUM,
            &[],
            std::slice::from_ref(&noop_program_id),
            &mut fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_success(
            TEST_CU_USAGE_BASELINE,
            &[],
            std::slice::from_ref(&noop_program_id),
            &mut fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_success(
            TEST_CU_USAGE_MAX,
            &[],
            std::slice::from_ref(&noop_program_id),
            &mut fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        feature_set.deactivate(&feature_set::increase_tx_account_lock_limit::id());
        assert!(!feature_set.snapshot().increase_tx_account_lock_limit);

        do_invoke_success(
            TEST_MAX_ACCOUNT_INFOS_OK_BEFORE_INCREASE_TX_ACCOUNT_LOCK_BEFORE_SIMD_0339,
            &[],
            std::slice::from_ref(&invoked_program_id),
            &mut fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        feature_set.activate(&feature_set::increase_tx_account_lock_limit::id(), 0);
        assert!(feature_set.snapshot().increase_tx_account_lock_limit);

        // failure cases
        let do_invoke_failure_test_local_with_compute_check =
            |test: u8,
             expected_error: TransactionError,
             expected_invoked_programs: &[Pubkey],
             expected_log_messages: Option<Vec<String>>,
             should_deplete_compute_meter: bool,
             accounts: &[(Pubkey, Account)],
             feature_set: &FeatureSet,
             program_cache: &mut ProgramCacheForTxBatch| {
                let compute_unit_limit = 1_000_000;

                let effects = do_invoke(
                    test,
                    &[ComputeBudgetInstruction::set_compute_unit_limit(
                        compute_unit_limit,
                    )],
                    accounts.to_vec(),
                    feature_set,
                    program_cache,
                );
                assert_eq!(effects.status, Err(expected_error));

                // TxnEffects has no inner-instruction trace. Logs contain entered programs, but
                // omit invocations rejected before entry by executable lookup or call-depth checks.
                if !matches!(
                    test,
                    TEST_PPROGRAM_NOT_OWNED_BY_LOADER
                        | TEST_PPROGRAM_NOT_EXECUTABLE
                        | TEST_NESTED_INVOKE_TOO_DEEP
                        | TEST_NESTED_INVOKE_SIMD_0268_TOO_DEEP
                ) {
                    assert_eq!(
                        cpi_programs_from_logs(&effects.logs),
                        expected_invoked_programs,
                    );
                }
                if should_deplete_compute_meter {
                    assert_eq!(effects.executed_units, compute_unit_limit as u64);
                } else {
                    assert!(effects.executed_units < compute_unit_limit as u64);
                }
                if let Some(expected_log_messages) = expected_log_messages {
                    assert_eq!(effects.logs.len(), expected_log_messages.len());
                    expected_log_messages
                        .into_iter()
                        .zip(effects.logs)
                        .for_each(|(expected_log_message, log_message)| {
                            if expected_log_message != String::from("skip") {
                                assert_eq!(log_message, expected_log_message);
                            }
                        });
                }
            };

        let do_invoke_failure_test_local =
            |test: u8,
             expected_error: TransactionError,
             expected_invoked_programs: &[Pubkey],
             expected_log_messages: Option<Vec<String>>,
             accounts: &[(Pubkey, Account)],
             feature_set: &FeatureSet,
             program_cache: &mut ProgramCacheForTxBatch| {
                do_invoke_failure_test_local_with_compute_check(
                    test,
                    expected_error,
                    expected_invoked_programs,
                    expected_log_messages,
                    false, // should_deplete_compute_meter
                    accounts,
                    feature_set,
                    program_cache,
                )
            };

        let program_lang = match program.0 {
            Languages::Rust => "Rust",
            Languages::C => "C",
        };

        do_invoke_failure_test_local(
            TEST_PRIVILEGE_ESCALATION_SIGNER,
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation),
            std::slice::from_ref(&invoked_program_id),
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_PRIVILEGE_ESCALATION_WRITABLE,
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation),
            std::slice::from_ref(&invoked_program_id),
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_PPROGRAM_NOT_OWNED_BY_LOADER,
            TransactionError::InstructionError(0, InstructionError::UnsupportedProgramId),
            &[argument_keypair.pubkey()],
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_PPROGRAM_NOT_EXECUTABLE,
            TransactionError::InstructionError(0, InstructionError::UnsupportedProgramId),
            &[unexecutable_program_keypair.pubkey()],
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_EMPTY_ACCOUNTS_SLICE,
            TransactionError::InstructionError(0, InstructionError::MissingAccount),
            &[],
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_CAP_SEEDS,
            TransactionError::InstructionError(0, InstructionError::MaxSeedLengthExceeded),
            &[],
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_CAP_SIGNERS,
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete),
            &[],
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_MAX_INSTRUCTION_DATA_LEN_EXCEEDED,
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete),
            &[],
            Some(vec![
                format!("Program {invoke_program_id} invoke [1]"),
                format!("Program log: invoke {program_lang} program"),
                "Program log: Test max instruction data len exceeded".into(),
                "skip".into(), // don't compare compute consumption logs
                format!(
                    "Program {invoke_program_id} failed: Invoked an instruction with data that is \
                     too large (10241 > 10240)"
                ),
            ]),
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_MAX_INSTRUCTION_ACCOUNTS_EXCEEDED,
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete),
            &[],
            Some(vec![
                format!("Program {invoke_program_id} invoke [1]"),
                format!("Program log: invoke {program_lang} program"),
                "Program log: Test max instruction accounts exceeded".into(),
                "skip".into(), // don't compare compute consumption logs
                format!(
                    "Program {invoke_program_id} failed: Invoked an instruction with too many \
                     accounts (256 > 255)"
                ),
            ]),
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_MAX_ACCOUNT_INFOS_EXCEEDED,
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete),
            &[],
            Some(vec![
                format!("Program {invoke_program_id} invoke [1]"),
                format!("Program log: invoke {program_lang} program"),
                "Program log: Test max account infos exceeded".into(),
                "skip".into(), // don't compare compute consumption logs
                format!(
                    "Program {invoke_program_id} failed: Invoked an instruction with too many \
                     account info's (256 > 255)"
                ),
            ]),
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        feature_set.deactivate(&feature_set::increase_tx_account_lock_limit::id());
        assert!(!feature_set.snapshot().increase_tx_account_lock_limit);

        do_invoke_failure_test_local(
            TEST_RETURN_ERROR,
            TransactionError::InstructionError(0, InstructionError::Custom(42)),
            std::slice::from_ref(&invoked_program_id),
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_PRIVILEGE_DEESCALATION_ESCALATION_SIGNER,
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation),
            std::slice::from_ref(&invoked_program_id),
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_PRIVILEGE_DEESCALATION_ESCALATION_WRITABLE,
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation),
            std::slice::from_ref(&invoked_program_id),
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local_with_compute_check(
            TEST_WRITABLE_DEESCALATION_WRITABLE,
            TransactionError::InstructionError(0, InstructionError::ReadonlyDataModified),
            std::slice::from_ref(&invoked_program_id),
            None,
            true, // should_deplete_compute_meter
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        // With SIMD-0268 disabled, five nested invokes is too deep.
        feature_set.deactivate(&feature_set::raise_cpi_nesting_limit_to_8::id());
        assert!(!feature_set.snapshot().raise_cpi_nesting_limit_to_8);

        do_invoke_failure_test_local(
            TEST_NESTED_INVOKE_TOO_DEEP,
            TransactionError::InstructionError(0, InstructionError::CallDepth),
            &[
                invoked_program_id,
                invoked_program_id,
                invoked_program_id,
                invoked_program_id,
                invoked_program_id,
            ],
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        // With SIMD-0268 enabled, nine nested invokes is too deep.
        feature_set.activate(&feature_set::raise_cpi_nesting_limit_to_8::id(), 0);
        assert!(feature_set.snapshot().raise_cpi_nesting_limit_to_8);

        do_invoke_failure_test_local(
            TEST_NESTED_INVOKE_SIMD_0268_TOO_DEEP,
            TransactionError::InstructionError(0, InstructionError::CallDepth),
            &[
                invoked_program_id,
                invoked_program_id,
                invoked_program_id,
                invoked_program_id,
                invoked_program_id,
                invoked_program_id,
                invoked_program_id,
                invoked_program_id,
                invoked_program_id,
            ],
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_RETURN_DATA_TOO_LARGE,
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete),
            &[],
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_DUPLICATE_PRIVILEGE_ESCALATION_SIGNER,
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation),
            std::slice::from_ref(&invoked_program_id),
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        do_invoke_failure_test_local(
            TEST_DUPLICATE_PRIVILEGE_ESCALATION_WRITABLE,
            TransactionError::InstructionError(0, InstructionError::PrivilegeEscalation),
            std::slice::from_ref(&invoked_program_id),
            None,
            &fixture.accounts,
            &feature_set,
            &mut program_cache,
        );

        // Check resulting state
        let account = fixture
            .accounts
            .iter()
            .find(|(pubkey, _)| pubkey == &derived_key1)
            .map(|(_, account)| account)
            .unwrap();
        assert_eq!(account.lamports, 43);
        assert_eq!(account.owner, invoke_program_id);
        assert_eq!(account.data.len(), MAX_PERMITTED_DATA_INCREASE);
        for i in 0..20 {
            assert_eq!(i as u8, account.data[i]);
        }

        // Attempt to realloc into unauthorized address space
        fixture.replace_account(
            from_keypair.pubkey(),
            Account {
                lamports: 84,
                data: Vec::new(),
                owner: system_program::id(),
                executable: false,
                rent_epoch: 0,
            },
        );
        fixture.replace_account(
            derived_key1,
            Account {
                lamports: 0,
                data: Vec::new(),
                owner: system_program::id(),
                executable: false,
                rent_epoch: 0,
            },
        );

        let effects = do_invoke(
            TEST_ALLOC_ACCESS_VIOLATION,
            &[],
            fixture.accounts.clone(),
            &feature_set,
            &mut program_cache,
        );
        assert!(cpi_programs_from_logs(&effects.logs).is_empty());
        assert_eq!(
            effects.status,
            Err(TransactionError::InstructionError(
                0,
                InstructionError::ProgramFailedToComplete,
            )),
        );
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_program_id_spoofing() {
    let spoof1_elf = load_program_elf("solana_sbf_rust_spoof1");
    let spoof1_system_elf = load_program_elf("solana_sbf_rust_spoof1_system");

    let malicious_swap_pubkey = Pubkey::new_unique();
    let malicious_system_pubkey = Pubkey::new_unique();

    let feature_set = SVMFeatureSet::all_enabled();

    let from_pubkey = Pubkey::new_unique();
    let to_pubkey = Pubkey::new_unique();

    let mut accounts = vec![keyed_account_for_system_program()];
    accounts.extend(upgradeable_program_accounts(
        &malicious_swap_pubkey,
        &spoof1_elf,
    ));
    accounts.extend(upgradeable_program_accounts(
        &malicious_system_pubkey,
        &spoof1_system_elf,
    ));
    accounts.extend([
        (from_pubkey, Account::new(10, 0, &system_program::id())),
        (to_pubkey, Account::new(0, 0, &system_program::id())),
    ]);

    let mut program_cache = default_program_cache();
    add_program_to_program_cache(
        &mut program_cache,
        &malicious_swap_pubkey,
        &bpf_loader_upgradeable::id(),
        &spoof1_elf,
        &feature_set,
    );
    add_program_to_program_cache(
        &mut program_cache,
        &malicious_system_pubkey,
        &bpf_loader_upgradeable::id(),
        &spoof1_system_elf,
        &feature_set,
    );

    let account_metas = vec![
        AccountMeta::new_readonly(system_program::id(), false),
        AccountMeta::new_readonly(malicious_system_pubkey, false),
        AccountMeta::new(from_pubkey, false),
        AccountMeta::new(to_pubkey, false),
    ];

    let instruction =
        Instruction::new_with_bytes(malicious_swap_pubkey, &[], account_metas.clone());

    let context = InstrContext::new_with_default_budget(feature_set, accounts, instruction);

    let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());

    assert_eq!(
        effects.result,
        Some(InstructionError::MissingRequiredSignature)
    );

    let from_account = effects.get_account(&from_pubkey).unwrap();
    assert_eq!(10, from_account.lamports);

    let to_account = effects.get_account(&to_pubkey).unwrap();
    assert_eq!(0, to_account.lamports);
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_caller_has_access_to_cpi_program() {
    let caller_access_elf = load_program_elf("solana_sbf_rust_caller_access");

    let caller_pubkey = Pubkey::new_unique();
    let caller2_pubkey = Pubkey::new_unique();

    let feature_set = SVMFeatureSet::all_enabled();

    let mut accounts = upgradeable_program_accounts(&caller_pubkey, &caller_access_elf);
    accounts.extend(upgradeable_program_accounts(
        &caller2_pubkey,
        &caller_access_elf,
    ));

    let mut program_cache = default_program_cache();
    add_program_to_program_cache(
        &mut program_cache,
        &caller_pubkey,
        &bpf_loader_upgradeable::id(),
        &caller_access_elf,
        &feature_set,
    );
    add_program_to_program_cache(
        &mut program_cache,
        &caller2_pubkey,
        &bpf_loader_upgradeable::id(),
        &caller_access_elf,
        &feature_set,
    );

    let account_metas = vec![
        AccountMeta::new_readonly(caller_pubkey, false),
        AccountMeta::new_readonly(caller2_pubkey, false),
    ];
    let instruction = Instruction::new_with_bytes(caller_pubkey, &[1], account_metas);

    let context = InstrContext::new_with_default_budget(feature_set, accounts, instruction);

    let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());

    assert_eq!(effects.result, Some(InstructionError::MissingAccount));
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_ro_modify() {
    agave_logger::setup();

    let program_elf = load_program_elf("solana_sbf_rust_ro_modify");
    let program_id = Pubkey::new_unique();

    let feature_set = SVMFeatureSet::all_enabled();

    let test_pubkey = Pubkey::new_unique();
    let mut accounts = vec![keyed_account_for_system_program()];
    accounts.extend(upgradeable_program_accounts(&program_id, &program_elf));
    accounts.push((test_pubkey, Account::new(10, 0, &system_program::id())));

    let mut program_cache =
        default_program_cache_with_program(&program_id, &program_elf, &feature_set);

    let account_metas = vec![
        AccountMeta::new_readonly(system_program::id(), false),
        AccountMeta::new(test_pubkey, true),
    ];

    for instr_data in [1u8, 3, 4] {
        let instruction =
            Instruction::new_with_bytes(program_id, &[instr_data], account_metas.clone());

        let context =
            InstrContext::new_with_default_budget(feature_set, accounts.clone(), instruction);

        let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());

        assert_eq!(
            effects.result,
            Some(InstructionError::ProgramFailedToComplete)
        );
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_call_depth() {
    agave_logger::setup();

    let program_elf = load_program_elf("solana_sbf_rust_call_depth");
    let program_id = Pubkey::new_unique();

    let feature_set = SVMFeatureSet::all_enabled();
    let compute_budget = ComputeBudget::new_with_defaults(false);

    let mut program_cache =
        default_program_cache_with_program(&program_id, &program_elf, &feature_set);
    let program_accounts = upgradeable_program_accounts(&program_id, &program_elf);

    let mut execute = |depth: usize| {
        let instruction = Instruction::new_with_bincode(program_id, &depth, vec![]);

        let context = InstrContext::new_with_default_budget(
            feature_set,
            program_accounts.clone(),
            instruction,
        );

        execute_instr(&context, &mut program_cache, &default_sysvar_cache())
    };

    let effects = execute(compute_budget.max_call_depth - 1);
    assert!(effects.result.is_none());

    let effects = execute(compute_budget.max_call_depth);
    assert!(effects.result.is_some());
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_compute_budget() {
    agave_logger::setup();

    let program_elf = load_program_elf("solana_sbf_rust_noop");
    let program_id = Pubkey::new_unique();

    let feature_set = SVMFeatureSet::all_enabled();

    let accounts = upgradeable_program_accounts(&program_id, &program_elf);

    let mut program_cache =
        default_program_cache_with_program(&program_id, &program_elf, &feature_set);

    let instruction = Instruction::new_with_bincode(program_id, &0, vec![]);

    let context = InstrContext {
        feature_set,
        accounts,
        instruction,
        cu_avail: 0,
    };

    let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());

    assert_eq!(
        effects.result,
        Some(InstructionError::ProgramFailedToComplete),
    );
}

#[test]
fn assert_instruction_count() {
    agave_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[
            ("alloc", 18572),
            ("sbf_to_sbf", 316),
            ("multiple_static", 210),
            ("noop", 5),
            ("noop++", 5),
            ("relative_call", 212),
            ("return_data", 1026),
            ("sanity", 2371),
            ("sanity++", 2271),
            ("secp256k1_recover", 25421),
            ("sha", 1446),
            ("struct_pass", 108),
            ("struct_ret", 122),
        ]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[
            ("solana_sbf_rust_128bit", 784),
            ("solana_sbf_rust_alloc", 4940),
            ("solana_sbf_rust_custom_heap", 343),
            ("solana_sbf_rust_dep_crate", 22),
            ("solana_sbf_rust_iter", 1514),
            ("solana_sbf_rust_many_args", 1287),
            ("solana_sbf_rust_mem", 1326),
            ("solana_sbf_rust_membuiltins", 329),
            ("solana_sbf_rust_noop", 342),
            ("solana_sbf_rust_param_passing", 108),
            ("solana_sbf_rust_rand", 315),
            ("solana_sbf_rust_sanity", 14228),
            ("solana_sbf_rust_secp256k1_recover", 88615),
            ("solana_sbf_rust_sha", 21998),
        ]);
    }

    let feature_set = SVMFeatureSet::all_enabled();

    println!("\n  {:36} expected actual  diff", "SBF program");
    for (program_name, expected_consumption) in programs.iter() {
        let program_elf = load_program_elf(program_name);
        let program_id = Pubkey::new_unique();

        let mut program_cache =
            default_program_cache_with_program(&program_id, &program_elf, &feature_set);

        let account_pubkey = Pubkey::new_unique();
        let mut accounts = upgradeable_program_accounts(&program_id, &program_elf);
        accounts.push((account_pubkey, Account::new(0, 0, &program_id)));

        let instruction_accounts = vec![AccountMeta {
            pubkey: account_pubkey,
            is_signer: false,
            is_writable: false,
        }];
        let instruction = Instruction::new_with_bytes(program_id, &[], instruction_accounts);

        let context = InstrContext::new_with_default_budget(feature_set, accounts, instruction);

        let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());

        let consumption = context.cu_avail.saturating_sub(effects.cu_avail);
        let diff: i64 = consumption as i64 - *expected_consumption as i64;
        println!(
            "  {:36} {:8}{:6} {:+5} ({:+3.0}%)",
            program_name,
            *expected_consumption,
            consumption,
            diff,
            100.0_f64 * consumption as f64 / *expected_consumption as f64 - 100.0_f64,
        );
        assert!(consumption <= *expected_consumption);
    }
}

// Program and loader used to deploy it in tests fixture.
#[cfg(feature = "sbf_rust")]
#[derive(Clone, Copy)]
struct ProgramSpec<'a> {
    name: &'a str,
    loader_id: Pubkey,
}

#[cfg(feature = "sbf_rust")]
impl<'a> ProgramSpec<'a> {
    fn upgradeable(name: &'a str) -> Self {
        Self {
            name,
            loader_id: bpf_loader_upgradeable::id(),
        }
    }

    fn deprecated(name: &'a str) -> Self {
        Self {
            name,
            loader_id: bpf_loader_deprecated::id(),
        }
    }
}

// Accounts, runtime configuration, and caches used to execute in tests.
#[cfg(feature = "sbf_rust")]
struct ProgramSbfTxnFixture {
    program_ids: Vec<Pubkey>,
    feature_set: FeatureSet,
    accounts: Vec<(Pubkey, Account)>,
    program_cache: ProgramCacheForTxBatch,
}

#[cfg(feature = "sbf_rust")]
impl ProgramSbfTxnFixture {
    fn new(programs: Vec<ProgramSpec<'_>>) -> Self {
        Self::new_with_feature_set(programs, FeatureSet::all_enabled())
    }

    fn new_with_feature_set(programs: Vec<ProgramSpec<'_>>, feature_set: FeatureSet) -> Self {
        agave_logger::setup();

        let program_ids = programs
            .iter()
            .map(|_| Pubkey::new_unique())
            .collect::<Vec<_>>();
        let mut program_cache = new_program_cache_with_builtins(1);

        let mut accounts = vec![];

        for (program_id, program) in program_ids.iter().zip(programs) {
            let program_elf = load_program_elf(program.name);
            add_program_to_program_cache(
                &mut program_cache,
                program_id,
                &program.loader_id,
                &program_elf,
                &feature_set.runtime_features(),
            );
            accounts.extend(program_accounts(
                program_id,
                &program_elf,
                program.loader_id,
            ));
        }

        Self {
            program_ids,
            feature_set,
            accounts,
            program_cache,
        }
    }

    fn add_account(&mut self, account: Option<Account>) -> Keypair {
        self.add_account_with_keypair(Keypair::new(), account)
    }

    fn add_account_with_keypair(&mut self, keypair: Keypair, account: Option<Account>) -> Keypair {
        self.add_account_with_pubkey(keypair.pubkey(), account);
        keypair
    }

    fn add_account_with_pubkey(&mut self, pubkey: Pubkey, account: Option<Account>) {
        self.accounts.push((
            pubkey,
            account.unwrap_or_else(|| Account::new(0, 0, &system_program::id())),
        ));
    }

    fn add_accounts_with_pubkeys(&mut self, accounts: impl IntoIterator<Item = (Pubkey, Account)>) {
        for (pubkey, account) in accounts {
            self.add_account_with_pubkey(pubkey, Some(account));
        }
    }

    fn replace_account(&mut self, pubkey: Pubkey, account: Account) {
        self.accounts
            .iter_mut()
            .find(|(account_pubkey, _)| account_pubkey == &pubkey)
            .unwrap()
            .1 = account;
    }

    // Simulates bank behaviour by preserving resulting accounts state after success in tests which need it.
    fn preserve_accounts_state(&mut self, effects: TxnEffects) {
        assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
        self.accounts = effects.resulting_accounts;
    }
}

#[cfg(feature = "sbf_rust")]
fn feature_set_with_account_mapping_features(enabled: bool) -> FeatureSet {
    let mut feature_set = FeatureSet::all_enabled();
    if !enabled {
        feature_set.deactivate(&feature_set::syscall_parameter_address_restrictions::id());
        feature_set.deactivate(&feature_set::virtual_address_space_adjustments::id());
        feature_set.deactivate(&feature_set::account_data_direct_mapping::id());
    }
    feature_set
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_instruction_introspection_passing_transaction() {
    let mut fixture = ProgramSbfTxnFixture::new(vec![ProgramSpec::upgradeable(
        "solana_sbf_rust_instruction_introspection",
    )]);
    let payer = fixture
        .add_account(Some(Account::new(50_000, 0, &system_program::id())))
        .pubkey();
    let program_id = fixture.program_ids[0];

    let account_metas = vec![
        AccountMeta::new_readonly(program_id, false),
        AccountMeta::new_readonly(sysvar::instructions::id(), false),
    ];

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[
                Instruction::new_with_bytes(program_id, &[0u8, 0u8], account_metas.clone()),
                Instruction::new_with_bytes(program_id, &[0u8, 1u8], account_metas.clone()),
                Instruction::new_with_bytes(program_id, &[0u8, 2u8], account_metas),
            ],
            Some(&payer),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    assert_eq!(effects.status, Ok(()));
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_instruction_introspection_writable_special_instructions1111() {
    let mut fixture = ProgramSbfTxnFixture::new(vec![ProgramSpec::upgradeable(
        "solana_sbf_rust_instruction_introspection",
    )]);
    let payer = fixture
        .add_account(Some(Account::new(50_000, 0, &system_program::id())))
        .pubkey();
    let program_id = fixture.program_ids[0];
    let account_metas = vec![AccountMeta::new(sysvar::instructions::id(), false)];

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(program_id, &[0], account_metas)],
            Some(&payer),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set,
        fixture.accounts,
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::ProgramFailedToComplete,
        )),
    );
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_instruction_introspection_no_accounts() {
    let mut fixture = ProgramSbfTxnFixture::new(vec![ProgramSpec::upgradeable(
        "solana_sbf_rust_instruction_introspection",
    )]);
    let payer = fixture
        .add_account(Some(Account::new(50_000, 0, &system_program::id())))
        .pubkey();
    let program_id = fixture.program_ids[0];

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(program_id, &[0], vec![])],
            Some(&payer),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set,
        fixture.accounts,
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );

    #[allow(deprecated)]
    let expected_error = Err(TransactionError::InstructionError(
        0,
        InstructionError::NotEnoughAccountKeys,
    ));
    assert_eq!(effects.status, expected_error);
}

#[test_matrix(
    [0, 1, 2, 5, 10, 15, 20],
    [1, 10, 50, 100, 255, 500, 1000, 1024]  // MAX_RETURN_DATA = 1024
)]
#[allow(clippy::arithmetic_side_effects)]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_r2_instruction_data_pointer(num_accounts: usize, input_data_len: usize) {
    agave_logger::setup();

    let program_elf = load_program_elf("solana_sbf_rust_r2_instruction_data_pointer");
    let program_id = Pubkey::new_unique();

    let feature_set = SVMFeatureSet::all_enabled();

    let mut program_cache =
        default_program_cache_with_program(&program_id, &program_elf, &feature_set);

    let mut accounts = Vec::new();
    let mut account_metas = Vec::new();

    for i in 0..num_accounts {
        let pubkey = Pubkey::new_unique();

        // Mixed account sizes.
        accounts.push((pubkey, Account::new(0, 100 + (i * 50), &program_id)));

        // Mixed account roles.
        if i % 2 == 0 {
            account_metas.push(AccountMeta::new(pubkey, false));
        } else {
            account_metas.push(AccountMeta::new_readonly(pubkey, false));
        }
    }

    accounts.extend(upgradeable_program_accounts(&program_id, &program_elf));

    // The provided instruction data will be set to the return data.
    let input_data: Vec<u8> = (0..input_data_len).map(|i| (i % 256) as u8).collect();

    let instruction = Instruction::new_with_bytes(program_id, &input_data, account_metas);

    let context = InstrContext::new_with_default_budget(feature_set, accounts, instruction);

    let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());

    assert!(effects.result.is_none());
    assert_eq!(input_data, effects.return_data);
}

#[test]
#[cfg(all(feature = "sbf_rust", feature = "sbpf-v3"))]
fn test_program_sbf_invoke_in_same_tx_as_deployment() {
    const DEPLOYMENT_SLOT: u64 = 2;

    let mut fixture = ProgramSbfTxnFixture::new(vec![ProgramSpec::upgradeable(
        "solana_sbf_rust_invoke_and_return",
    )]);
    let mint_keypair = fixture.add_account(Some(Account::new(50, 0, &system_program::id())));
    let authority_keypair = fixture.add_account(None);
    let program_keypair = fixture.add_account(None);
    let indirect_program_id = fixture.program_ids[0];

    let program_id = program_keypair.pubkey();
    let (programdata_address, _) =
        Pubkey::find_program_address(&[program_id.as_ref()], &bpf_loader_upgradeable::id());

    let program_elf = load_program_elf("solana_sbf_rust_noop");

    let buffer_state = UpgradeableLoaderState::Buffer {
        authority_address: Some(authority_keypair.pubkey()),
    };
    let mut buffer_data = bincode::serialize(&buffer_state).unwrap();
    buffer_data.extend_from_slice(&program_elf);

    let buffer_keypair = fixture.add_account(Some(Account {
        lamports: 1,
        data: buffer_data,
        owner: bpf_loader_upgradeable::id(),
        executable: false,
        rent_epoch: 0,
    }));
    fixture.add_accounts_with_pubkeys([
        (
            programdata_address,
            Account {
                lamports: 0,
                data: Vec::new(),
                owner: system_program::id(),
                executable: false,
                rent_epoch: 0,
            },
        ),
        (
            rent::id(),
            Account {
                lamports: 1,
                data: bincode::serialize(&Rent::free()).unwrap(),
                owner: sysvar::id(),
                executable: false,
                rent_epoch: 0,
            },
        ),
        (
            clock::id(),
            Account {
                lamports: 1,
                data: bincode::serialize(&solana_clock::Clock {
                    slot: DEPLOYMENT_SLOT,
                    epoch_start_timestamp: 0,
                    epoch: 0,
                    leader_schedule_epoch: 0,
                    unix_timestamp: 0,
                })
                .unwrap(),
                owner: sysvar::id(),
                executable: false,
                rent_epoch: 0,
            },
        ),
        keyed_account_for_system_program(),
        keyed_account_for_bpf_loader_upgradeable_program(),
    ]);

    let sysvar_cache = sysvar_cache_from_accounts(&fixture.accounts);
    fixture.program_cache.set_slot_for_tests(DEPLOYMENT_SLOT);

    #[allow(deprecated)]
    let deployment_instructions = loader_v3_instruction::deploy_with_max_program_len(
        &mint_keypair.pubkey(),
        &program_id,
        &buffer_keypair.pubkey(),
        &authority_keypair.pubkey(),
        1,
        program_elf.len() * 2,
    )
    .unwrap();

    let execute = |instructions: Vec<Instruction>, program_cache: &mut ProgramCacheForTxBatch| {
        let sanitized_message = SanitizedMessage::try_from_legacy_message(
            Message::new(&instructions, Some(&mint_keypair.pubkey())),
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let context = TxnContext::new_with_default_budget(
            fixture.feature_set.clone(),
            fixture.accounts.clone(),
            sanitized_message,
            None,
        );
        execute_txn(&context, program_cache, &sysvar_cache)
    };

    // Prepare invocations
    let invoke_instruction =
        Instruction::new_with_bytes(program_id, &[0], vec![AccountMeta::new(clock::id(), false)]);
    let indirect_invoke_instruction = Instruction::new_with_bytes(
        indirect_program_id,
        &[0],
        vec![
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(clock::id(), false),
        ],
    );

    // Deployment is invisible to both top-level-instructions and CPI instructions
    for invoke_instruction in [invoke_instruction, indirect_invoke_instruction] {
        let mut transaction_program_cache = fixture.program_cache.clone();
        let mut instructions = deployment_instructions.clone();
        instructions.push(invoke_instruction);

        let effects = execute(instructions, &mut transaction_program_cache);
        assert_eq!(
            effects.status,
            Err(TransactionError::InstructionError(
                2,
                InstructionError::UnsupportedProgramId,
            )),
            "{:?}",
            effects.logs,
        );
    }
}

#[test]
#[cfg(all(feature = "sbf_rust", feature = "sbpf-v3"))]
fn test_program_sbf_invoke_in_same_tx_as_redeployment() {
    const REDEPLOYMENT_SLOT: u64 = 4;

    let mut fixture = ProgramSbfTxnFixture::new(vec![
        ProgramSpec::upgradeable("solana_sbf_rust_noop"),
        ProgramSpec::upgradeable("solana_sbf_rust_invoke_and_return"),
    ]);
    let mint_keypair = fixture.add_account(Some(Account::new(50, 0, &system_program::id())));
    let authority_keypair = fixture.add_account(None);
    let program_id = fixture.program_ids[0];
    let indirect_program_id = fixture.program_ids[1];

    // Prepare redeployment: load new program into a buffer.
    let upgraded_program_elf = load_program_elf("solana_sbf_rust_panic");
    let mut buffer_data = bincode::serialize(&UpgradeableLoaderState::Buffer {
        authority_address: Some(authority_keypair.pubkey()),
    })
    .unwrap();
    buffer_data.extend_from_slice(&upgraded_program_elf);
    let buffer_keypair = fixture.add_account(Some(Account {
        lamports: 1,
        data: buffer_data,
        owner: bpf_loader_upgradeable::id(),
        executable: false,
        rent_epoch: 0,
    }));

    let feature_set = fixture.feature_set.clone();
    let mut program_cache = fixture.program_cache.clone();

    let program_elf = load_program_elf("solana_sbf_rust_noop");
    let (programdata_address, _) =
        Pubkey::find_program_address(&[program_id.as_ref()], &bpf_loader_upgradeable::id());
    let mut programdata = bincode::serialize(&UpgradeableLoaderState::ProgramData {
        slot: 0,
        upgrade_authority_address: Some(authority_keypair.pubkey()),
    })
    .unwrap();
    programdata.extend_from_slice(&program_elf);
    programdata.resize(
        UpgradeableLoaderState::size_of_programdata(program_elf.len().saturating_mul(2)),
        0,
    );
    fixture.replace_account(
        programdata_address,
        Account {
            lamports: 1,
            data: programdata,
            owner: bpf_loader_upgradeable::id(),
            executable: false,
            rent_epoch: 0,
        },
    );

    let program_data = bincode::serialize(&UpgradeableLoaderState::Program {
        programdata_address,
    })
    .unwrap();
    fixture.replace_account(
        program_id,
        Account {
            lamports: 1,
            data: program_data,
            owner: bpf_loader_upgradeable::id(),
            executable: true,
            rent_epoch: 0,
        },
    );

    fixture.add_accounts_with_pubkeys([
        (
            rent::id(),
            Account {
                lamports: 1,
                data: bincode::serialize(&Rent::free()).unwrap(),
                owner: sysvar::id(),
                executable: false,
                rent_epoch: 0,
            },
        ),
        (
            clock::id(),
            Account {
                lamports: 1,
                data: bincode::serialize(&solana_clock::Clock {
                    slot: REDEPLOYMENT_SLOT,
                    epoch_start_timestamp: 0,
                    epoch: 0,
                    leader_schedule_epoch: 0,
                    unix_timestamp: 0,
                })
                .unwrap(),
                owner: sysvar::id(),
                executable: false,
                rent_epoch: 0,
            },
        ),
        keyed_account_for_bpf_loader_upgradeable_program(),
    ]);

    program_cache.set_slot_for_tests(REDEPLOYMENT_SLOT);
    let sysvar_cache = sysvar_cache_from_accounts(&fixture.accounts);

    let redeployment_instruction = loader_v3_instruction::upgrade(
        &program_id,
        &buffer_keypair.pubkey(),
        &authority_keypair.pubkey(),
        &mint_keypair.pubkey(),
    );

    let execute = |transaction_accounts: Vec<(Pubkey, Account)>,
                   instructions: Vec<Instruction>,
                   program_cache: &mut ProgramCacheForTxBatch| {
        let sanitized_message = SanitizedMessage::try_from_legacy_message(
            Message::new(&instructions, Some(&mint_keypair.pubkey())),
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let context = TxnContext::new_with_default_budget(
            feature_set.clone(),
            transaction_accounts,
            sanitized_message,
            None,
        );
        execute_txn(&context, program_cache, &sysvar_cache)
    };

    // Prepare invocations
    let invoke_instruction =
        Instruction::new_with_bytes(program_id, &[0], vec![AccountMeta::new(clock::id(), false)]);
    let indirect_invoke_instruction = Instruction::new_with_bytes(
        indirect_program_id,
        &[0],
        vec![
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(clock::id(), false),
        ],
    );

    // Redeployment causes programs to be unavailable to both top-level-instructions
    // and CPI instructions
    for invoke_instruction in [invoke_instruction, indirect_invoke_instruction] {
        // Call upgradeable program
        let effects = execute(
            fixture.accounts.clone(),
            vec![invoke_instruction.clone()],
            &mut program_cache,
        );
        fixture.preserve_accounts_state(effects);

        // Upgrade the program and invoke in same tx
        let mut transaction_program_cache = program_cache.clone();

        let effects = execute(
            fixture.accounts.clone(),
            vec![redeployment_instruction.clone(), invoke_instruction],
            &mut transaction_program_cache,
        );
        assert_eq!(
            effects.status,
            Err(TransactionError::InstructionError(
                1,
                InstructionError::UnsupportedProgramId,
            )),
            "{:?}",
            effects.logs,
        );
    }
}

#[test]
#[cfg(all(feature = "sbf_rust", feature = "sbpf-v3"))]
fn test_program_sbf_invoke_in_same_tx_as_undeployment() {
    const UNDEPLOYMENT_SLOT: u64 = 4;
    const MINT_BALANCE_AFTER_PROGRAM_DEPLOYS: u64 = 46;

    let mut fixture = ProgramSbfTxnFixture::new(vec![
        ProgramSpec::upgradeable("solana_sbf_rust_noop"),
        ProgramSpec::upgradeable("solana_sbf_rust_invoke_and_return"),
    ]);
    let mint_keypair = fixture.add_account(Some(Account::new(
        MINT_BALANCE_AFTER_PROGRAM_DEPLOYS,
        0,
        &system_program::id(),
    )));
    let authority_keypair = fixture.add_account(None);
    let program_id = fixture.program_ids[0];
    let indirect_program_id = fixture.program_ids[1];

    // The fixture installs the program directly. Rebuild its ProgramData account to match the
    // original deployment: slot 0, the deployment authority, and twice-the-ELF capacity.
    let program_elf = load_program_elf("solana_sbf_rust_noop");
    let (programdata_address, _) =
        Pubkey::find_program_address(&[program_id.as_ref()], &bpf_loader_upgradeable::id());
    let mut programdata = bincode::serialize(&UpgradeableLoaderState::ProgramData {
        slot: 0,
        upgrade_authority_address: Some(authority_keypair.pubkey()),
    })
    .unwrap();
    programdata.extend_from_slice(&program_elf);
    programdata.resize(
        UpgradeableLoaderState::size_of_programdata(program_elf.len().saturating_mul(2)),
        0,
    );
    let mut programdata_account = fixture
        .accounts
        .iter()
        .find(|(pubkey, _)| pubkey == &programdata_address)
        .unwrap()
        .1
        .clone();
    programdata_account.lamports = 1;
    programdata_account.data = programdata;
    fixture.replace_account(programdata_address, programdata_account);

    let mut program_account = fixture
        .accounts
        .iter()
        .find(|(pubkey, _)| pubkey == &program_id)
        .unwrap()
        .1
        .clone();
    program_account.lamports = 1;

    fixture.replace_account(program_id, program_account);
    fixture.add_accounts_with_pubkeys([
        (
            clock::id(),
            Account {
                lamports: 1,
                data: bincode::serialize(&solana_clock::Clock {
                    slot: UNDEPLOYMENT_SLOT,
                    ..solana_clock::Clock::default()
                })
                .unwrap(),
                owner: sysvar::id(),
                ..Account::default()
            },
        ),
        keyed_account_for_bpf_loader_upgradeable_program(),
    ]);

    let sysvar_cache = sysvar_cache_from_accounts(&fixture.accounts);
    let feature_set = fixture.feature_set.clone();
    fixture.program_cache.set_slot_for_tests(UNDEPLOYMENT_SLOT);

    let execute = |transaction_accounts: Vec<(Pubkey, Account)>,
                   instructions: Vec<Instruction>,
                   program_cache: &mut ProgramCacheForTxBatch| {
        let sanitized_message = SanitizedMessage::try_from_legacy_message(
            Message::new(&instructions, Some(&mint_keypair.pubkey())),
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let context = TxnContext::new_with_default_budget(
            feature_set.clone(),
            transaction_accounts,
            sanitized_message,
            None,
        );
        execute_txn(&context, program_cache, &sysvar_cache)
    };

    // Prepare invocations
    let invoke_instruction =
        Instruction::new_with_bytes(program_id, &[0], vec![AccountMeta::new(clock::id(), false)]);

    let indirect_invoke_instruction = Instruction::new_with_bytes(
        indirect_program_id,
        &[0],
        vec![
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new_readonly(clock::id(), false),
        ],
    );

    // Prepare undeployment
    let undeployment_instruction = loader_v3_instruction::close_any(
        &programdata_address,
        &mint_keypair.pubkey(),
        Some(&authority_keypair.pubkey()),
        Some(&program_id),
    );

    // Undeployment causes the program to become unavailable to both top-level
    // instructions and CPI instructions
    for invoke_instruction in [invoke_instruction, indirect_invoke_instruction] {
        // Call upgradeable program
        let effects = execute(
            fixture.accounts.clone(),
            vec![invoke_instruction.clone()],
            &mut fixture.program_cache,
        );
        fixture.preserve_accounts_state(effects);

        // Undeploy the program and invoke in same tx
        // A failed transaction must not leave its cache tombstone visible to the next loop case.
        let mut transaction_program_cache = fixture.program_cache.clone();

        let effects = execute(
            fixture.accounts.clone(),
            vec![undeployment_instruction.clone(), invoke_instruction],
            &mut transaction_program_cache,
        );
        assert_eq!(
            effects.status,
            Err(TransactionError::InstructionError(
                1,
                InstructionError::UnsupportedProgramId,
            )),
        );
    }
}

#[test]
#[cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
fn test_program_sbf_disguised_as_sbf_loader() {
    agave_logger::setup();

    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.extend_from_slice(&[("noop")]);
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.extend_from_slice(&[("solana_sbf_rust_noop")]);
    }

    for program in programs.iter() {
        let program_elf = load_program_elf(program);
        let program_id = Pubkey::new_unique();

        let feature_set = SVMFeatureSet {
            remove_bpf_loader_incorrect_program_id: false,
            ..SVMFeatureSet::all_enabled()
        };

        let mut program_cache = default_program_cache();
        add_program_to_program_cache(
            &mut program_cache,
            &program_id,
            &bpf_loader::id(),
            &program_elf,
            &feature_set,
        );

        let account_metas = vec![AccountMeta::new_readonly(program_id, false)];
        let instruction = Instruction::new_with_bytes(bpf_loader::id(), &[1], account_metas);

        let context = InstrContext::new_with_default_budget(
            feature_set,
            vec![
                keyed_account_for_bpf_loader_program(),
                solana_program_binaries::bpf_loader_program_account(
                    &program_id,
                    &program_elf,
                    &Rent::default(),
                ),
            ],
            instruction,
        );

        let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());
        assert_eq!(effects.result, Some(InstructionError::UnsupportedProgramId));
    }
}

#[test]
#[cfg(feature = "sbf_c")]
fn test_program_reads_from_program_account() {
    agave_logger::setup();

    let program_elf = load_program_elf("read_program");
    let program_id = Pubkey::new_unique();

    let feature_set = SVMFeatureSet::all_enabled();

    let mut program_cache = default_program_cache();
    add_program_to_program_cache(
        &mut program_cache,
        &program_id,
        &bpf_loader::id(),
        &program_elf,
        &feature_set,
    );

    // Loader V2 (bpf_loader) stores the raw ELF in the program account.
    let program_account = Account {
        lamports: 1,
        data: program_elf.clone(),
        owner: bpf_loader::id(),
        executable: true,
        rent_epoch: u64::MAX,
    };

    let account_metas = vec![AccountMeta::new_readonly(program_id, false)];
    let instruction = Instruction::new_with_bytes(program_id, &program_elf[..48], account_metas);

    let context = InstrContext::new_with_default_budget(
        feature_set,
        vec![(program_id, program_account)],
        instruction,
    );

    let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());
    assert!(effects.result.is_none());
}

#[test]
#[cfg(feature = "sbf_c")]
fn test_program_sbf_c_dup() {
    agave_logger::setup();

    let program_elf = load_program_elf("ser");
    let program_id = Pubkey::new_unique();

    let feature_set = SVMFeatureSet::all_enabled();
    let mut program_cache = default_program_cache();
    add_program_to_program_cache(
        &mut program_cache,
        &program_id,
        &bpf_loader_upgradeable::id(),
        &program_elf,
        &feature_set,
    );

    let account_address = Pubkey::new_unique();
    let account =
        Account::new_data(42, &[1_u8, 2, 3], &solana_sdk_ids::system_program::id()).unwrap();

    let account_metas = vec![
        AccountMeta::new_readonly(account_address, false),
        AccountMeta::new_readonly(account_address, false),
    ];
    let instruction = Instruction::new_with_bytes(program_id, &[4, 5, 6, 7], account_metas);

    let mut accounts = upgradeable_program_accounts(&program_id, &program_elf);
    accounts.push((account_address, account));
    let context = InstrContext::new_with_default_budget(feature_set, accounts, instruction);

    let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());
    assert!(effects.result.is_none());
}

#[test]
#[cfg(all(feature = "sbf_rust", feature = "sbpf-v3"))]
fn test_program_sbf_upgrade() {
    const UPGRADE_SLOT: u64 = 2;
    const UPGRADE_EFFECTIVE_SLOT: u64 = 3;

    let mut fixture = ProgramSbfTxnFixture::new(vec![ProgramSpec::upgradeable(
        "solana_sbf_rust_upgradeable",
    )]);
    let new_authority_keypair = fixture.add_account(None);
    let mint_keypair = fixture.add_account(Some(Account::new(50, 0, &system_program::id())));
    let authority_keypair = fixture.add_account(None);
    let program_id = fixture.program_ids[0];
    let mut program_cache = fixture.program_cache.clone();

    // do not add to fixture
    let program_elf = load_program_elf("solana_sbf_rust_upgradeable");

    let (programdata_address, _) =
        Pubkey::find_program_address(&[program_id.as_ref()], &bpf_loader_upgradeable::id());
    let mut programdata = bincode::serialize(&UpgradeableLoaderState::ProgramData {
        slot: 0,
        upgrade_authority_address: Some(authority_keypair.pubkey()),
    })
    .unwrap();
    programdata.extend_from_slice(&program_elf);
    programdata.resize(
        UpgradeableLoaderState::size_of_programdata(program_elf.len().saturating_mul(2)),
        0,
    );
    fixture.replace_account(
        programdata_address,
        Account {
            lamports: 1,
            data: programdata,
            owner: bpf_loader_upgradeable::id(),
            executable: false,
            rent_epoch: 0,
        },
    );

    let program_data = bincode::serialize(&UpgradeableLoaderState::Program {
        programdata_address,
    })
    .unwrap();
    fixture.replace_account(
        program_id,
        Account {
            lamports: 1,
            data: program_data,
            owner: bpf_loader_upgradeable::id(),
            executable: true,
            rent_epoch: 0,
        },
    );

    fixture.add_accounts_with_pubkeys([
        (
            rent::id(),
            Account {
                lamports: 1,
                data: bincode::serialize(&Rent::free()).unwrap(),
                owner: sysvar::id(),
                executable: false,
                rent_epoch: 0,
            },
        ),
        (
            clock::id(),
            Account {
                lamports: 1,
                data: bincode::serialize(&solana_clock::Clock {
                    slot: UPGRADE_SLOT,
                    epoch_start_timestamp: 0,
                    epoch: 0,
                    leader_schedule_epoch: 0,
                    unix_timestamp: 0,
                })
                .unwrap(),
                owner: sysvar::id(),
                executable: false,
                rent_epoch: 0,
            },
        ),
        keyed_account_for_bpf_loader_upgradeable_program(),
    ]);

    program_cache.set_slot_for_tests(UPGRADE_SLOT);
    let sysvar_cache = sysvar_cache_from_accounts(&fixture.accounts);
    let feature_set = fixture.feature_set.clone();

    let execute = |transaction_accounts: Vec<(Pubkey, Account)>,
                   instructions: Vec<Instruction>,
                   program_cache: &mut ProgramCacheForTxBatch,
                   sysvar_cache: &SysvarCache| {
        let sanitized_message = SanitizedMessage::try_from_legacy_message(
            Message::new(&instructions, Some(&mint_keypair.pubkey())),
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let context = TxnContext::new_with_default_budget(
            feature_set.clone(),
            transaction_accounts,
            sanitized_message,
            None,
        );

        execute_txn(&context, program_cache, sysvar_cache)
    };

    // Call upgradeable program
    let instruction_accounts = vec![AccountMeta::new(clock::id(), false)];
    let instruction = Instruction::new_with_bytes(program_id, &[0], instruction_accounts.clone());
    assert_eq!(
        execute(
            fixture.accounts.clone(),
            vec![instruction],
            &mut program_cache,
            &sysvar_cache,
        )
        .status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::Custom(42),
        )),
    );

    // Set authority
    let set_authority_instruction = loader_v3_instruction::set_upgrade_authority(
        &program_id,
        &authority_keypair.pubkey(),
        Some(&new_authority_keypair.pubkey()),
    );

    let effects = execute(
        fixture.accounts.clone(),
        vec![set_authority_instruction],
        &mut program_cache,
        &sysvar_cache,
    );
    fixture.preserve_accounts_state(effects);

    // Upgrade program
    let upgraded_program_elf = load_program_elf("solana_sbf_rust_upgraded");
    let mut buffer_data = bincode::serialize(&UpgradeableLoaderState::Buffer {
        authority_address: Some(new_authority_keypair.pubkey()),
    })
    .unwrap();
    buffer_data.extend_from_slice(&upgraded_program_elf);

    let buffer_keypair = fixture.add_account(Some(Account {
        lamports: 1,
        data: buffer_data,
        owner: bpf_loader_upgradeable::id(),
        executable: false,
        rent_epoch: 0,
    }));

    let upgrade_instruction = loader_v3_instruction::upgrade(
        &program_id,
        &buffer_keypair.pubkey(),
        &new_authority_keypair.pubkey(),
        &mint_keypair.pubkey(),
    );

    let effects = execute(
        fixture.accounts.clone(),
        vec![upgrade_instruction],
        &mut program_cache,
        &sysvar_cache,
    );
    fixture.preserve_accounts_state(effects);

    let modified_programs = program_cache.drain_modified_entries();
    assert!(modified_programs.contains_key(&program_id));

    program_cache.merge(&modified_programs);
    program_cache.set_slot_for_tests(UPGRADE_EFFECTIVE_SLOT);

    fixture.replace_account(
        clock::id(),
        Account {
            lamports: 1,
            data: bincode::serialize(&solana_clock::Clock {
                slot: UPGRADE_EFFECTIVE_SLOT,
                epoch_start_timestamp: 0,
                epoch: 0,
                leader_schedule_epoch: 0,
                unix_timestamp: 0,
            })
            .unwrap(),
            owner: sysvar::id(),
            executable: false,
            rent_epoch: 0,
        },
    );

    let sysvar_cache = sysvar_cache_from_accounts(&fixture.accounts);

    // Call upgraded program
    let effects = execute(
        fixture.accounts.clone(),
        vec![Instruction::new_with_bytes(
            program_id,
            &[1],
            instruction_accounts,
        )],
        &mut program_cache,
        &sysvar_cache,
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::Custom(43),
        )),
    );
}

#[test]
#[cfg(all(feature = "sbf_rust", feature = "sbpf-v3"))]
fn test_program_sbf_upgrade_via_cpi() {
    const UPGRADE_SLOT: u64 = 4;
    const UPGRADE_EFFECTIVE_SLOT: u64 = 5;

    let mut fixture = ProgramSbfTxnFixture::new(vec![
        ProgramSpec::upgradeable("solana_sbf_rust_invoke_and_return"),
        ProgramSpec::upgradeable("solana_sbf_rust_upgradeable"),
    ]);
    let new_authority_keypair = fixture.add_account(None);
    let mint_keypair = fixture.add_account(Some(Account::new(50, 0, &system_program::id())));
    let authority_keypair = fixture.add_account(None);
    let invoke_and_return = fixture.program_ids[0];
    let program_id = fixture.program_ids[1];
    let mut program_cache = fixture.program_cache.clone();
    let program_elf = load_program_elf("solana_sbf_rust_upgradeable");

    let (programdata_address, _) =
        Pubkey::find_program_address(&[program_id.as_ref()], &bpf_loader_upgradeable::id());
    let mut programdata = bincode::serialize(&UpgradeableLoaderState::ProgramData {
        slot: 2,
        upgrade_authority_address: Some(authority_keypair.pubkey()),
    })
    .unwrap();
    programdata.extend_from_slice(&program_elf);
    programdata.resize(
        UpgradeableLoaderState::size_of_programdata(program_elf.len().saturating_mul(2)),
        0,
    );

    fixture.replace_account(
        programdata_address,
        Account {
            lamports: 1,
            data: programdata,
            owner: bpf_loader_upgradeable::id(),
            executable: false,
            rent_epoch: 0,
        },
    );

    let program_data = bincode::serialize(&UpgradeableLoaderState::Program {
        programdata_address,
    })
    .unwrap();

    fixture.replace_account(
        program_id,
        Account {
            lamports: 1,
            data: program_data,
            owner: bpf_loader_upgradeable::id(),
            executable: true,
            rent_epoch: 0,
        },
    );
    fixture.add_accounts_with_pubkeys([
        (
            rent::id(),
            Account {
                lamports: 1,
                data: bincode::serialize(&Rent::free()).unwrap(),
                owner: sysvar::id(),
                executable: false,
                rent_epoch: 0,
            },
        ),
        (
            clock::id(),
            Account {
                lamports: 1,
                data: bincode::serialize(&solana_clock::Clock {
                    slot: UPGRADE_SLOT,
                    epoch_start_timestamp: 0,
                    epoch: 0,
                    leader_schedule_epoch: 0,
                    unix_timestamp: 0,
                })
                .unwrap(),
                owner: sysvar::id(),
                executable: false,
                rent_epoch: 0,
            },
        ),
        keyed_account_for_bpf_loader_upgradeable_program(),
    ]);

    program_cache.set_slot_for_tests(UPGRADE_SLOT);
    let sysvar_cache = sysvar_cache_from_accounts(&fixture.accounts);
    let feature_set = fixture.feature_set.clone();

    let execute = |transaction_accounts: Vec<(Pubkey, Account)>,
                   instructions: Vec<Instruction>,
                   program_cache: &mut ProgramCacheForTxBatch,
                   sysvar_cache: &SysvarCache| {
        let sanitized_message = SanitizedMessage::try_from_legacy_message(
            Message::new(&instructions, Some(&mint_keypair.pubkey())),
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let context = TxnContext::new_with_default_budget(
            feature_set.clone(),
            transaction_accounts,
            sanitized_message,
            None,
        );
        execute_txn(&context, program_cache, sysvar_cache)
    };

    // Call the upgradeable program via CPI
    let instruction_accounts = vec![
        AccountMeta::new_readonly(program_id, false),
        AccountMeta::new_readonly(clock::id(), false),
    ];
    let instruction =
        Instruction::new_with_bytes(invoke_and_return, &[1], instruction_accounts.clone());

    let effects = execute(
        fixture.accounts.clone(),
        vec![instruction],
        &mut program_cache,
        &sysvar_cache,
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::Custom(42),
        )),
    );

    // Set authority via CPI
    let mut authority_instruction = loader_v3_instruction::set_upgrade_authority(
        &program_id,
        &authority_keypair.pubkey(),
        Some(&new_authority_keypair.pubkey()),
    );
    authority_instruction.program_id = invoke_and_return;
    authority_instruction
        .accounts
        .insert(0, AccountMeta::new(bpf_loader_upgradeable::id(), false));

    let effects = execute(
        fixture.accounts.clone(),
        vec![authority_instruction],
        &mut program_cache,
        &sysvar_cache,
    );
    fixture.preserve_accounts_state(effects);

    // Upgrade program via CPI
    let upgraded_program_elf = load_program_elf("solana_sbf_rust_upgraded");
    let mut buffer_data = bincode::serialize(&UpgradeableLoaderState::Buffer {
        authority_address: Some(new_authority_keypair.pubkey()),
    })
    .unwrap();
    buffer_data.extend_from_slice(&upgraded_program_elf);
    let buffer_keypair = fixture.add_account(Some(Account {
        lamports: 1,
        data: buffer_data,
        owner: bpf_loader_upgradeable::id(),
        executable: false,
        rent_epoch: 0,
    }));

    let mut upgrade_instruction = loader_v3_instruction::upgrade(
        &program_id,
        &buffer_keypair.pubkey(),
        &new_authority_keypair.pubkey(),
        &mint_keypair.pubkey(),
    );
    upgrade_instruction.program_id = invoke_and_return;
    upgrade_instruction
        .accounts
        .insert(0, AccountMeta::new(bpf_loader_upgradeable::id(), false));

    let effects = execute(
        fixture.accounts.clone(),
        vec![upgrade_instruction],
        &mut program_cache,
        &sysvar_cache,
    );
    fixture.preserve_accounts_state(effects);

    let modified_programs = program_cache.drain_modified_entries();
    assert!(modified_programs.contains_key(&program_id));
    program_cache.merge(&modified_programs);
    program_cache.set_slot_for_tests(UPGRADE_EFFECTIVE_SLOT);

    fixture.replace_account(
        clock::id(),
        Account {
            lamports: 1,
            data: bincode::serialize(&solana_clock::Clock {
                slot: UPGRADE_EFFECTIVE_SLOT,
                epoch_start_timestamp: 0,
                epoch: 0,
                leader_schedule_epoch: 0,
                unix_timestamp: 0,
            })
            .unwrap(),
            owner: sysvar::id(),
            executable: false,
            rent_epoch: 0,
        },
    );
    let sysvar_cache = sysvar_cache_from_accounts(&fixture.accounts);

    // Call the upgraded program via CPI
    let effects = execute(
        fixture.accounts.clone(),
        vec![Instruction::new_with_bytes(
            invoke_and_return,
            &[2],
            instruction_accounts,
        )],
        &mut program_cache,
        &sysvar_cache,
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::Custom(43),
        )),
    );
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_ro_account_modify() {
    agave_logger::setup();

    let program_elf = load_program_elf("solana_sbf_rust_ro_account_modify");
    let program_id = Pubkey::new_unique();

    let feature_set = SVMFeatureSet::all_enabled();

    let mut program_cache =
        default_program_cache_with_program(&program_id, &program_elf, &feature_set);

    let argument_pubkey = Pubkey::new_unique();
    let mut accounts = upgradeable_program_accounts(&program_id, &program_elf);
    accounts.push((argument_pubkey, Account::new(42, 100, &program_id)));

    let account_metas = vec![
        AccountMeta::new_readonly(argument_pubkey, false),
        AccountMeta::new_readonly(program_id, false),
    ];

    for case in [0, 1, 2] {
        let instruction = Instruction::new_with_bytes(program_id, &[case], account_metas.clone());

        let context =
            InstrContext::new_with_default_budget(feature_set, accounts.clone(), instruction);

        let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());

        assert_eq!(effects.result, Some(InstructionError::ReadonlyDataModified));
    }
}

#[test_case(false; "without_virtual_address_space_adjustments")]
#[test_case(true; "with_virtual_address_space_adjustments")]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_realloc(virtual_address_space_adjustments: bool) {
    const START_BALANCE: u64 = 100_000_000_000;

    let configured_feature_set =
        feature_set_with_account_mapping_features(virtual_address_space_adjustments);
    let mut fixture = ProgramSbfTxnFixture::new_with_feature_set(
        vec![ProgramSpec::upgradeable("solana_sbf_rust_realloc")],
        configured_feature_set,
    );
    let mint_keypair = fixture.add_account(None);
    let account_keypair = fixture.add_account(Some(Account::new(
        START_BALANCE,
        5,
        &fixture.program_ids[0],
    )));
    let program_id = fixture.program_ids[0];

    fixture.add_accounts_with_pubkeys([
        keyed_account_for_compute_budget_program(),
        keyed_account_for_system_program(),
    ]);

    // Loaded-account data size limit is not enforced by the transaction conformance harness,
    // but keep the compute-budget instruction to preserve the original transaction.
    let execute = |fixture: &mut ProgramSbfTxnFixture, instruction| {
        let sanitized_message = SanitizedMessage::try_from_legacy_message(
            Message::new(
                &[
                    instruction,
                    ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                        LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                    ),
                ],
                Some(&mint_keypair.pubkey()),
            ),
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();
        let context = TxnContext::new_with_default_budget(
            fixture.feature_set.clone(),
            fixture.accounts.clone(),
            sanitized_message,
            None,
        );
        execute_txn(
            &context,
            &mut fixture.program_cache,
            &default_sysvar_cache(),
        )
    };

    let mut bump = 0;

    // Realloc RO account
    let mut instruction = realloc(&program_id, &account_keypair.pubkey(), 0, &mut bump);
    instruction.accounts[0].is_writable = false;

    let effects = execute(&mut fixture, instruction);
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::ReadonlyDataModified,
        )),
    );

    // Realloc account to overflow
    let effects = execute(
        &mut fixture,
        realloc(
            &program_id,
            &account_keypair.pubkey(),
            usize::MAX,
            &mut bump,
        ),
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidRealloc,
        )),
    );

    // Realloc account to 0
    let effects = execute(
        &mut fixture,
        realloc(&program_id, &account_keypair.pubkey(), 0, &mut bump),
    );
    let data_len = effects
        .get_account(&account_keypair.pubkey())
        .unwrap()
        .data
        .len();
    assert_eq!(data_len, 0,);
    fixture.preserve_accounts_state(effects);

    // Realloc account to max then undo
    let effects = execute(
        &mut fixture,
        realloc_extend_and_undo(
            &program_id,
            &account_keypair.pubkey(),
            MAX_PERMITTED_DATA_INCREASE,
            &mut bump,
        ),
    );
    let data_len = effects
        .get_account(&account_keypair.pubkey())
        .unwrap()
        .data
        .len();
    assert_eq!(data_len, 0,);
    fixture.preserve_accounts_state(effects);

    // Realloc account to max + 1 then undo
    let effects = execute(
        &mut fixture,
        realloc_extend_and_undo(
            &program_id,
            &account_keypair.pubkey(),
            MAX_PERMITTED_DATA_INCREASE + 1,
            &mut bump,
        ),
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidRealloc,
        )),
    );

    // Realloc to max + 1
    let effects = execute(
        &mut fixture,
        realloc(
            &program_id,
            &account_keypair.pubkey(),
            MAX_PERMITTED_DATA_INCREASE + 1,
            &mut bump,
        ),
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidRealloc,
        )),
    );

    // Realloc to max length in max increase increments
    for i in 0..MAX_PERMITTED_DATA_LENGTH as usize / MAX_PERMITTED_DATA_INCREASE {
        let mut bump = i as u64;

        let effects = execute(
            &mut fixture,
            realloc_extend_and_fill(
                &program_id,
                &account_keypair.pubkey(),
                MAX_PERMITTED_DATA_INCREASE,
                1,
                &mut bump,
            ),
        );
        let data_len = effects
            .get_account(&account_keypair.pubkey())
            .unwrap()
            .data
            .len();
        assert_eq!(
            data_len,
            i.saturating_add(1)
                .saturating_mul(MAX_PERMITTED_DATA_INCREASE),
        );
        fixture.preserve_accounts_state(effects);
    }

    // and one more time should fail
    let effects = execute(
        &mut fixture,
        realloc_extend(
            &program_id,
            &account_keypair.pubkey(),
            MAX_PERMITTED_DATA_INCREASE,
            &mut bump,
        ),
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidRealloc,
        )),
    );

    // Realloc to 6 bytes
    let effects = execute(
        &mut fixture,
        realloc(&program_id, &account_keypair.pubkey(), 6, &mut bump),
    );
    let data_len = effects
        .get_account(&account_keypair.pubkey())
        .unwrap()
        .data
        .len();
    assert_eq!(data_len, 6,);
    fixture.preserve_accounts_state(effects);

    // Extend by 2 bytes and write a u64. This ensures that we can do writes that span the original
    // account length (6 bytes) and the realloc data (2 bytes).
    let val = 0x1122334455667788;

    let effects = execute(
        &mut fixture,
        extend_and_write_u64(&program_id, &account_keypair.pubkey(), val),
    );

    let data = &effects.get_account(&account_keypair.pubkey()).unwrap().data;
    assert_eq!(8, data.len());
    assert_eq!(val, unsafe { *data.as_ptr().cast::<u64>() });
    fixture.preserve_accounts_state(effects);

    // Realloc to 0
    let effects = execute(
        &mut fixture,
        realloc(&program_id, &account_keypair.pubkey(), 0, &mut bump),
    );

    let data_len = effects
        .get_account(&account_keypair.pubkey())
        .unwrap()
        .data
        .len();
    assert_eq!(data_len, 0,);
    fixture.preserve_accounts_state(effects);

    // Realloc and assign
    let effects = execute(
        &mut fixture,
        Instruction::new_with_bytes(
            program_id,
            &[REALLOC_AND_ASSIGN],
            vec![AccountMeta::new(account_keypair.pubkey(), false)],
        ),
    );

    let acc = effects.get_account(&account_keypair.pubkey()).unwrap();
    assert_eq!(&system_program::id(), acc.owner());
    assert_eq!(MAX_PERMITTED_DATA_INCREASE, acc.data.len());
    fixture.preserve_accounts_state(effects);

    // Realloc to 0 with wrong owner
    let effects = execute(
        &mut fixture,
        realloc(&program_id, &account_keypair.pubkey(), 0, &mut bump),
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::ExternalAccountDataModified,
        )),
    );

    // realloc and assign to self via cpi
    let effects = execute(
        &mut fixture,
        Instruction::new_with_bytes(
            program_id,
            &[REALLOC_AND_ASSIGN_TO_SELF_VIA_SYSTEM_PROGRAM],
            vec![
                AccountMeta::new(account_keypair.pubkey(), true),
                AccountMeta::new(system_program::id(), false),
            ],
        ),
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::ExternalAccountDataModified,
        )),
    );

    // Assign to self and realloc via cpi
    let effects = execute(
        &mut fixture,
        Instruction::new_with_bytes(
            program_id,
            &[ASSIGN_TO_SELF_VIA_SYSTEM_PROGRAM_AND_REALLOC],
            vec![
                AccountMeta::new(account_keypair.pubkey(), true),
                AccountMeta::new(system_program::id(), false),
            ],
        ),
    );

    let acc = effects.get_account(&account_keypair.pubkey()).unwrap();
    assert_eq!(&program_id, acc.owner());
    assert_eq!(2 * MAX_PERMITTED_DATA_INCREASE, acc.data.len());
    fixture.preserve_accounts_state(effects);

    // Realloc to 0
    let effects = execute(
        &mut fixture,
        realloc(&program_id, &account_keypair.pubkey(), 0, &mut bump),
    );

    let data_len = effects
        .get_account(&account_keypair.pubkey())
        .unwrap()
        .data
        .len();
    assert_eq!(data_len, 0);
    fixture.preserve_accounts_state(effects);

    // zero-init
    let effects = execute(
        &mut fixture,
        Instruction::new_with_bytes(
            program_id,
            &[ZERO_INIT],
            vec![AccountMeta::new(account_keypair.pubkey(), true)],
        ),
    );
    assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_realloc_invoke() {
    const START_BALANCE: u64 = 100_000_000_000;

    let mut fixture = ProgramSbfTxnFixture::new(vec![
        ProgramSpec::upgradeable("solana_sbf_rust_realloc"),
        ProgramSpec::upgradeable("solana_sbf_rust_realloc_invoke"),
    ]);
    let mint_keypair = fixture.add_account(Some(Account::new(
        1_000_000_000_000,
        0,
        &system_program::id(),
    )));
    let account_keypair = fixture.add_account(Some(Account::new(
        START_BALANCE,
        5,
        &fixture.program_ids[0],
    )));

    let realloc_program_id = fixture.program_ids[0];
    let realloc_invoke_program_id = fixture.program_ids[1];
    let feature_set = fixture.feature_set.clone();
    let mut program_cache = fixture.program_cache.clone();
    fixture.add_accounts_with_pubkeys([
        keyed_account_for_compute_budget_program(),
        keyed_account_for_system_program(),
    ]);

    // Loaded-account data size limit is not enforced by the transaction conformance harness,
    // but keep the compute-budget instructions to preserve the original transactions.
    let mut execute = |transaction_accounts, instruction, with_loaded_accounts_data_size_limit| {
        let mut instructions = vec![instruction];
        if with_loaded_accounts_data_size_limit {
            instructions.push(
                ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                    LOADED_ACCOUNTS_DATA_SIZE_LIMIT_FOR_TEST,
                ),
            );
        }
        let sanitized_message = SanitizedMessage::try_from_legacy_message(
            Message::new(&instructions, Some(&mint_keypair.pubkey())),
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let context = TxnContext::new_with_default_budget(
            feature_set.clone(),
            transaction_accounts,
            sanitized_message,
            None,
        );

        execute_txn(&context, &mut program_cache, &default_sysvar_cache())
    };

    let mut bump = 0;

    // Realloc RO account
    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &[INVOKE_REALLOC_ZERO_RO],
            vec![
                AccountMeta::new_readonly(account_keypair.pubkey(), false),
                AccountMeta::new_readonly(realloc_program_id, false),
            ],
        ),
        true,
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::ReadonlyDataModified,
        )),
    );
    assert_eq!(
        effects
            .get_account(&account_keypair.pubkey())
            .unwrap()
            .lamports(),
        START_BALANCE,
    );

    // Realloc account to 0
    let effects = execute(
        fixture.accounts.clone(),
        realloc(&realloc_program_id, &account_keypair.pubkey(), 0, &mut bump),
        true,
    );

    let account = effects.get_account(&account_keypair.pubkey()).unwrap();
    assert_eq!(account.lamports(), START_BALANCE);
    assert_eq!(account.data.len(), 0);
    fixture.preserve_accounts_state(effects);

    // Realloc to max + 1
    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &[INVOKE_REALLOC_MAX_PLUS_ONE],
            vec![
                AccountMeta::new(account_keypair.pubkey(), false),
                AccountMeta::new_readonly(realloc_program_id, false),
            ],
        ),
        true,
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidRealloc,
        )),
    );

    // Realloc to max twice
    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &[INVOKE_REALLOC_MAX_TWICE],
            vec![
                AccountMeta::new(account_keypair.pubkey(), false),
                AccountMeta::new_readonly(realloc_program_id, false),
            ],
        ),
        true,
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidRealloc,
        )),
    );

    // Realloc account to 0
    let effects = execute(
        fixture.accounts.clone(),
        realloc(&realloc_program_id, &account_keypair.pubkey(), 0, &mut bump),
        true,
    );

    let account = effects.get_account(&account_keypair.pubkey()).unwrap();
    assert_eq!(account.lamports(), START_BALANCE);
    assert_eq!(account.data.len(), 0);
    fixture.preserve_accounts_state(effects);

    // Realloc and assign
    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &[INVOKE_REALLOC_AND_ASSIGN],
            vec![
                AccountMeta::new(account_keypair.pubkey(), false),
                AccountMeta::new_readonly(realloc_program_id, false),
            ],
        ),
        true,
    );

    let account = effects.get_account(&account_keypair.pubkey()).unwrap();
    assert_eq!(&system_program::id(), account.owner());
    assert_eq!(MAX_PERMITTED_DATA_INCREASE, account.data.len());
    fixture.preserve_accounts_state(effects);

    // Realloc to 0 with wrong owner
    let effects = execute(
        fixture.accounts.clone(),
        realloc(&realloc_program_id, &account_keypair.pubkey(), 0, &mut bump),
        true,
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::ExternalAccountDataModified,
        )),
    );

    // realloc and assign to self via system program
    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &[INVOKE_REALLOC_AND_ASSIGN_TO_SELF_VIA_SYSTEM_PROGRAM],
            vec![
                AccountMeta::new(account_keypair.pubkey(), true),
                AccountMeta::new_readonly(realloc_program_id, false),
                AccountMeta::new_readonly(system_program::id(), false),
            ],
        ),
        true,
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::ExternalAccountDataModified,
        )),
    );

    // Assign to self and realloc via system program
    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &[INVOKE_ASSIGN_TO_SELF_VIA_SYSTEM_PROGRAM_AND_REALLOC],
            vec![
                AccountMeta::new(account_keypair.pubkey(), true),
                AccountMeta::new_readonly(realloc_program_id, false),
                AccountMeta::new_readonly(system_program::id(), false),
            ],
        ),
        true,
    );

    let account = effects.get_account(&account_keypair.pubkey()).unwrap();
    assert_eq!(&realloc_program_id, account.owner());
    assert_eq!(
        MAX_PERMITTED_DATA_INCREASE.saturating_mul(2),
        account.data.len(),
    );
    fixture.preserve_accounts_state(effects);

    // Realloc to 0
    let effects = execute(
        fixture.accounts.clone(),
        realloc(&realloc_program_id, &account_keypair.pubkey(), 0, &mut bump),
        true,
    );
    assert_eq!(
        effects
            .get_account(&account_keypair.pubkey())
            .unwrap()
            .data
            .len(),
        0,
    );
    fixture.preserve_accounts_state(effects);

    // Realloc to 100 and check via CPI
    let invoke_keypair = fixture.add_account(Some(Account::new(
        START_BALANCE,
        5,
        &realloc_invoke_program_id,
    )));

    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &[INVOKE_REALLOC_INVOKE_CHECK],
            vec![
                AccountMeta::new(invoke_keypair.pubkey(), false),
                AccountMeta::new_readonly(realloc_program_id, false),
            ],
        ),
        true,
    );

    let data = &effects.get_account(&invoke_keypair.pubkey()).unwrap().data;
    assert_eq!(100, data.len());
    for i in 0..5 {
        assert_eq!(data[i], 0);
    }
    for i in 5..data.len() {
        assert_eq!(data[i], 2);
    }
    fixture.preserve_accounts_state(effects);

    // Create account, realloc, check
    let new_keypair = fixture.add_account(None);

    let mut instruction_data = vec![];
    instruction_data.extend_from_slice(&[INVOKE_CREATE_ACCOUNT_REALLOC_CHECK, 1]);
    instruction_data.extend_from_slice(&100_usize.to_le_bytes());

    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &instruction_data,
            vec![
                AccountMeta::new(mint_keypair.pubkey(), true),
                AccountMeta::new(new_keypair.pubkey(), true),
                AccountMeta::new(system_program::id(), false),
                AccountMeta::new_readonly(realloc_invoke_program_id, false),
            ],
        ),
        true,
    );

    let account = effects.get_account(&new_keypair.pubkey()).unwrap();
    assert_eq!(200, account.data.len());
    assert_eq!(&realloc_invoke_program_id, account.owner());
    fixture.preserve_accounts_state(effects);

    // Invoke, dealloc, and assign
    let pre_len: usize = 100;
    let new_len = pre_len.saturating_mul(2);

    let mut invoke_account = Account::new(START_BALANCE, pre_len, &realloc_program_id);
    invoke_account.data.fill(1);
    fixture.replace_account(invoke_keypair.pubkey(), invoke_account);

    let mut instruction_data = vec![];
    instruction_data.extend_from_slice(&[INVOKE_DEALLOC_AND_ASSIGN, 1]);
    instruction_data.extend_from_slice(&pre_len.to_le_bytes());

    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &instruction_data,
            vec![
                AccountMeta::new(invoke_keypair.pubkey(), false),
                AccountMeta::new_readonly(realloc_invoke_program_id, false),
                AccountMeta::new_readonly(realloc_program_id, false),
            ],
        ),
        true,
    );

    let data = &effects.get_account(&invoke_keypair.pubkey()).unwrap().data;
    assert_eq!(new_len, data.len());
    for i in 0..new_len {
        assert_eq!(data[i], 0);
    }
    fixture.preserve_accounts_state(effects);

    // Realloc to max invoke max
    fixture.replace_account(
        invoke_keypair.pubkey(),
        Account::new(42, 0, &realloc_invoke_program_id),
    );

    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &[INVOKE_REALLOC_MAX_INVOKE_MAX],
            vec![
                AccountMeta::new(invoke_keypair.pubkey(), false),
                AccountMeta::new_readonly(realloc_program_id, false),
            ],
        ),
        true,
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidRealloc,
        )),
    );

    // CPI realloc extend then local realloc extend
    for (cpi_extend_bytes, local_extend_bytes, should_succeed) in [
        (0, 0, true),
        (MAX_PERMITTED_DATA_INCREASE, 0, true),
        (0, MAX_PERMITTED_DATA_INCREASE, true),
        (MAX_PERMITTED_DATA_INCREASE, 1, false),
        (1, MAX_PERMITTED_DATA_INCREASE, false),
    ] {
        fixture.replace_account(
            invoke_keypair.pubkey(),
            Account::new(100_000_000, 0, &realloc_invoke_program_id),
        );

        let mut instruction_data = vec![];
        instruction_data.extend_from_slice(&[INVOKE_REALLOC_TO_THEN_LOCAL_REALLOC_EXTEND, 1]);
        instruction_data.extend_from_slice(&cpi_extend_bytes.to_le_bytes());
        instruction_data.extend_from_slice(&local_extend_bytes.to_le_bytes());

        let effects = execute(
            fixture.accounts.clone(),
            Instruction::new_with_bytes(
                realloc_invoke_program_id,
                &instruction_data,
                vec![
                    AccountMeta::new(invoke_keypair.pubkey(), false),
                    AccountMeta::new_readonly(realloc_invoke_program_id, false),
                ],
            ),
            true,
        );

        if should_succeed {
            assert_eq!(
                effects.status,
                Ok(()),
                "cpi: {cpi_extend_bytes} local: {local_extend_bytes}, logs: {:?}",
                effects.logs,
            );
            fixture.preserve_accounts_state(effects);
        } else {
            assert_eq!(
                effects.status,
                Err(TransactionError::InstructionError(
                    0,
                    InstructionError::InvalidRealloc,
                )),
                "cpi: {cpi_extend_bytes} local: {local_extend_bytes}",
            );
        }
    }

    // Realloc shrink, then CPI, then realloc extend
    let mut invoke_account = Account::new(100_000_000, 10, &realloc_invoke_program_id);
    invoke_account.data = vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
    fixture.replace_account(invoke_keypair.pubkey(), invoke_account);

    let mut instruction_data = vec![];
    instruction_data.extend_from_slice(&[INVOKE_REALLOC_SHRINK_THEN_CPI_THEN_REALLOC_EXTEND, 1]);
    instruction_data.extend_from_slice(&5_u64.to_le_bytes());
    instruction_data.extend_from_slice(&10_u64.to_le_bytes());

    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &instruction_data,
            vec![
                AccountMeta::new(invoke_keypair.pubkey(), false),
                AccountMeta::new_readonly(realloc_invoke_program_id, false),
            ],
        ),
        true,
    );

    let data = &effects.get_account(&invoke_keypair.pubkey()).unwrap().data;
    assert_eq!(data, &[0, 1, 2, 3, 4, 0, 0, 0, 0, 0]);
    fixture.preserve_accounts_state(effects);

    // Realloc invoke max twice
    fixture.replace_account(
        invoke_keypair.pubkey(),
        Account::new(42, 0, &realloc_program_id),
    );

    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &[INVOKE_INVOKE_MAX_TWICE],
            vec![
                AccountMeta::new(invoke_keypair.pubkey(), false),
                AccountMeta::new_readonly(realloc_invoke_program_id, false),
                AccountMeta::new_readonly(realloc_program_id, false),
            ],
        ),
        true,
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidRealloc,
        )),
    );

    // Realloc to 0
    let effects = execute(
        fixture.accounts.clone(),
        realloc(&realloc_program_id, &account_keypair.pubkey(), 0, &mut bump),
        false,
    );
    assert_eq!(
        effects
            .get_account(&account_keypair.pubkey())
            .unwrap()
            .data
            .len(),
        0,
    );
    fixture.preserve_accounts_state(effects);

    // Realloc to max length in max increase increments
    for i in 0..MAX_PERMITTED_DATA_LENGTH as usize / MAX_PERMITTED_DATA_INCREASE {
        let effects = execute(
            fixture.accounts.clone(),
            Instruction::new_with_bytes(
                realloc_invoke_program_id,
                &[INVOKE_REALLOC_EXTEND_MAX, 1, i as u8, (i / 255) as u8],
                vec![
                    AccountMeta::new(account_keypair.pubkey(), false),
                    AccountMeta::new_readonly(realloc_program_id, false),
                ],
            ),
            true,
        );
        assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);

        assert_eq!(
            i.saturating_add(1)
                .saturating_mul(MAX_PERMITTED_DATA_INCREASE),
            effects
                .get_account(&account_keypair.pubkey())
                .unwrap()
                .data
                .len(),
        );

        fixture.preserve_accounts_state(effects);
    }

    // and one more time should fail
    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &[INVOKE_REALLOC_EXTEND_MAX, 2, 1, 1],
            vec![
                AccountMeta::new(account_keypair.pubkey(), false),
                AccountMeta::new_readonly(realloc_program_id, false),
            ],
        ),
        true,
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::InvalidRealloc,
        )),
    );

    // Realloc recursively and fill data
    let recursive_invoke_keypair = fixture.add_account(Some(Account::new(
        START_BALANCE,
        0,
        &realloc_invoke_program_id,
    )));

    let mut instruction_data = vec![];
    instruction_data.extend_from_slice(&[INVOKE_REALLOC_RECURSIVE, 1]);
    instruction_data.extend_from_slice(&100_usize.to_le_bytes());

    let effects = execute(
        fixture.accounts.clone(),
        Instruction::new_with_bytes(
            realloc_invoke_program_id,
            &instruction_data,
            vec![
                AccountMeta::new(recursive_invoke_keypair.pubkey(), false),
                AccountMeta::new_readonly(realloc_invoke_program_id, false),
            ],
        ),
        true,
    );
    assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);

    let data = &effects
        .get_account(&recursive_invoke_keypair.pubkey())
        .unwrap()
        .data;
    assert_eq!(200, data.len());
    for i in 0..100 {
        assert_eq!(data[i], 1);
    }
    for i in 100..200 {
        assert_eq!(data[i], 2);
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_processed_inner_instruction() {
    let mut fixture = ProgramSbfTxnFixture::new(vec![
        ProgramSpec::upgradeable("solana_sbf_rust_sibling_instructions"),
        ProgramSpec::upgradeable("solana_sbf_rust_sibling_inner_instructions"),
        ProgramSpec::upgradeable("solana_sbf_rust_noop"),
        ProgramSpec::upgradeable("solana_sbf_rust_invoke_and_return"),
    ]);
    let payer = fixture
        .add_account(Some(Account::new(50_000, 0, &system_program::id())))
        .pubkey();
    let mint_keypair = fixture.add_account(None);

    let sibling_program_id = fixture.program_ids[0];
    let sibling_inner_program_id = fixture.program_ids[1];
    let noop_program_id = fixture.program_ids[2];
    let invoke_and_return_program_id = fixture.program_ids[3];

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[
                Instruction::new_with_bytes(
                    noop_program_id,
                    &[43],
                    vec![
                        AccountMeta::new_readonly(noop_program_id, false),
                        AccountMeta::new(mint_keypair.pubkey(), true),
                    ],
                ),
                Instruction::new_with_bytes(
                    noop_program_id,
                    &[42],
                    vec![
                        AccountMeta::new(mint_keypair.pubkey(), true),
                        AccountMeta::new_readonly(noop_program_id, false),
                    ],
                ),
                Instruction::new_with_bytes(
                    sibling_program_id,
                    &[1, 2, 3, 0, 4, 5, 6],
                    vec![
                        AccountMeta::new(mint_keypair.pubkey(), true),
                        AccountMeta::new_readonly(noop_program_id, false),
                        AccountMeta::new_readonly(invoke_and_return_program_id, false),
                        AccountMeta::new_readonly(sibling_inner_program_id, false),
                    ],
                ),
            ],
            Some(&payer),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set,
        fixture.accounts,
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    assert_eq!(effects.status, Ok(()));
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_inner_instruction_alignment_checks() {
    let mut fixture = ProgramSbfTxnFixture::new(vec![
        ProgramSpec::deprecated("solana_sbf_rust_noop"),
        ProgramSpec::deprecated("solana_sbf_rust_inner_instruction_alignment_check"),
    ]);
    let mint_keypair = fixture.add_account(Some(Account::new(50, 0, &system_program::id())));
    let noop = fixture.program_ids[0];
    let inner_instruction_alignment_check = fixture.program_ids[1];

    // Invoke an unaligned program, which will call another unaligned program twice.
    // Unaligned pointer access should remain allowed once an invoke completes.
    let instruction = Instruction::new_with_bytes(
        inner_instruction_alignment_check,
        &[1],
        vec![
            AccountMeta::new_readonly(noop, false),
            AccountMeta::new_readonly(mint_keypair.pubkey(), false),
        ],
    );

    // Match the original Bank test, where the account passed to both CPIs was also the payer.
    // Message-wide privilege promotion therefore makes it a signer and writable even though its
    // instruction meta is readonly and non-signer.
    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(&[instruction], Some(&mint_keypair.pubkey())),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set,
        fixture.accounts,
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    assert_eq!(effects.status, Ok(()));
}

#[test_case(false; "without_virtual_address_space_adjustments")]
#[test_case(true; "with_virtual_address_space_adjustments")]
#[cfg(feature = "sbf_rust")]
fn test_cpi_account_ownership_writability(virtual_address_space_adjustments: bool) {
    let configured_feature_set =
        feature_set_with_account_mapping_features(virtual_address_space_adjustments);

    let mut fixture = ProgramSbfTxnFixture::new_with_feature_set(
        vec![
            ProgramSpec::upgradeable("solana_sbf_rust_invoke"),
            ProgramSpec::upgradeable("solana_sbf_rust_invoked"),
            ProgramSpec::upgradeable("solana_sbf_rust_realloc"),
        ],
        configured_feature_set,
    );
    let mint_keypair = fixture.add_account(None);
    let account_keypair = fixture.add_account(None);

    let invoke_program_id = fixture.program_ids[0];
    let invoked_program_id = fixture.program_ids[1];
    let realloc_program_id = fixture.program_ids[2];

    let account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new(account_keypair.pubkey(), false),
        AccountMeta::new_readonly(invoked_program_id, false),
        AccountMeta::new_readonly(invoke_program_id, false),
        AccountMeta::new_readonly(realloc_program_id, false),
    ];

    for (account_size, byte_index) in [
        (0, 0),                                   // first realloc byte
        (0, MAX_PERMITTED_DATA_INCREASE - 1),     // last realloc byte
        (2, 0),                                   // first data byte
        (2, 1),                                   // last data byte
        (2, 3),                                   // first realloc byte
        (2, 2 + MAX_PERMITTED_DATA_INCREASE - 1), // last realloc byte
    ] {
        for instruction_id in [
            TEST_FORBID_WRITE_AFTER_OWNERSHIP_CHANGE_IN_CALLEE,
            TEST_FORBID_WRITE_AFTER_OWNERSHIP_CHANGE_IN_CALLER,
        ] {
            fixture.replace_account(
                account_keypair.pubkey(),
                Account::new(42, account_size, &invoke_program_id),
            );

            let mut instruction_data = vec![instruction_id];
            instruction_data.extend_from_slice(byte_index.to_le_bytes().as_ref());

            let sanitized_message = SanitizedMessage::try_from_legacy_message(
                Message::new(
                    &[Instruction::new_with_bytes(
                        invoke_program_id,
                        &instruction_data,
                        account_metas.clone(),
                    )],
                    Some(&mint_keypair.pubkey()),
                ),
                &ReservedAccountKeys::empty_key_set(),
            )
            .unwrap();

            let context = TxnContext::new_with_default_budget(
                fixture.feature_set.clone(),
                fixture.accounts.clone(),
                sanitized_message,
                None,
            );

            let effects = execute_txn(
                &context,
                &mut fixture.program_cache,
                &default_sysvar_cache(),
            );
            if (byte_index as usize) < account_size || virtual_address_space_adjustments {
                assert_eq!(
                    effects.status,
                    Err(TransactionError::InstructionError(
                        0,
                        InstructionError::ExternalAccountDataModified,
                    )),
                );
            } else {
                // without virtual_address_space_adjustments, changes to the realloc padding
                // outside the account length are ignored
                assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
            }
        }
    }
    // Test that the CPI code that updates `ref_to_len_in_vm` fails if we
    // make it write to an invalid location. This is the first variant which
    // correctly triggers ExternalAccountDataModified when virtual_address_space_adjustments is
    // disabled. When virtual_address_space_adjustments is enabled this tests fails early
    // because we move the account data pointer.
    // TEST_FORBID_LEN_UPDATE_AFTER_OWNERSHIP_CHANGE is able to make more
    // progress when virtual_address_space_adjustments is on.
    fixture.replace_account(
        account_keypair.pubkey(),
        Account::new(42, 0, &invoke_program_id),
    );
    let instruction_data = vec![
        TEST_FORBID_LEN_UPDATE_AFTER_OWNERSHIP_CHANGE_MOVING_DATA_POINTER,
        42,
        42,
        42,
    ];

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas.clone(),
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    assert_eq!(
        effects.status,
        Err(if virtual_address_space_adjustments {
            // We move the data pointer, virtual_address_space_adjustments doesn't allow it
            // anymore so it errors out earlier. See
            // test_cpi_invalid_account_info_pointers.
            TransactionError::InstructionError(0, InstructionError::ProgramFailedToComplete)
        } else {
            // We managed to make CPI write into the account data, but the
            // usual checks still apply and we get an error.
            TransactionError::InstructionError(0, InstructionError::ExternalAccountDataModified)
        }),
    );

    // We're going to try and make CPI write ref_to_len_in_vm into a 2nd
    // account, so we add an extra one here.
    let account2_keypair = fixture.add_account(Some(Account::new(42, 0, &invoke_program_id)));
    let mut account_metas = account_metas.clone();
    account_metas.push(AccountMeta::new(account2_keypair.pubkey(), false));

    for target_account in [1, (account_metas.len() as u8).checked_sub(1).unwrap()] {
        // Similar to the test above where we try to make CPI write into account
        // data. This variant is for when virtual_address_space_adjustments is enabled.
        fixture.replace_account(
            account_keypair.pubkey(),
            Account::new(42, 0, &invoke_program_id),
        );
        fixture.replace_account(
            account2_keypair.pubkey(),
            Account::new(42, 0, &invoke_program_id),
        );

        let instruction_data = vec![
            TEST_FORBID_LEN_UPDATE_AFTER_OWNERSHIP_CHANGE,
            target_account,
            42,
            42,
        ];

        let sanitized_message = SanitizedMessage::try_from_legacy_message(
            Message::new(
                &[Instruction::new_with_bytes(
                    invoke_program_id,
                    &instruction_data,
                    account_metas.clone(),
                )],
                Some(&mint_keypair.pubkey()),
            ),
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let context = TxnContext::new_with_default_budget(
            fixture.feature_set.clone(),
            fixture.accounts.clone(),
            sanitized_message,
            None,
        );

        let effects = execute_txn(
            &context,
            &mut fixture.program_cache,
            &default_sysvar_cache(),
        );
        if virtual_address_space_adjustments {
            assert_eq!(
                effects.status,
                Err(TransactionError::InstructionError(
                    0,
                    InstructionError::ProgramFailedToComplete
                )),
            );
            // We haven't moved the data pointer, but ref_to_len_vm _is_ in
            // the account data vm range and that's not allowed either.
            assert!(
                effects
                    .logs
                    .iter()
                    .any(|log| log.contains("Invalid pointer")),
                "{:?}",
                effects.logs,
            );
        } else {
            // we expect this to succeed as after updating `ref_to_len_in_vm`,
            // CPI will sync the actual account data between the callee and the
            // caller, _always_ writing over the location pointed by
            // `ref_to_len_in_vm`. To verify this, we check that the account
            // data is in fact all zeroes like it is in the callee.
            assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
            assert_eq!(
                effects.get_account(&account_keypair.pubkey()).unwrap().data,
                vec![0; 40],
            );
        }
    }

    // Test that the caller can write to an account which it received from the callee
    fixture.replace_account(
        account_keypair.pubkey(),
        Account::new(42, 0, &invoked_program_id),
    );

    let instruction_data = vec![TEST_ALLOW_WRITE_AFTER_OWNERSHIP_CHANGE_TO_CALLER, 1, 42, 42];

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas,
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
}

#[cfg(feature = "sbf_rust")]
fn cpi_account_data_updates_fixture(
    deprecated_callee: bool,
    deprecated_caller: bool,
    virtual_address_space_adjustments: bool,
) -> (ProgramSbfTxnFixture, Keypair, Keypair, Vec<AccountMeta>) {
    let configured_feature_set =
        feature_set_with_account_mapping_features(virtual_address_space_adjustments);

    let mut fixture = ProgramSbfTxnFixture::new_with_feature_set(
        vec![
            ProgramSpec::upgradeable("solana_sbf_rust_invoke"),
            ProgramSpec::upgradeable("solana_sbf_rust_realloc"),
            ProgramSpec::deprecated("solana_sbf_rust_deprecated_loader"),
        ],
        configured_feature_set,
    );
    let mint_keypair = fixture.add_account(None);
    let account_keypair = fixture.add_account(None);

    let invoke_program_id = fixture.program_ids[0];
    let realloc_program_id = fixture.program_ids[1];
    let deprecated_program_id = fixture.program_ids[2];

    let account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new(account_keypair.pubkey(), false),
        AccountMeta::new_readonly(
            if deprecated_callee {
                deprecated_program_id
            } else {
                realloc_program_id
            },
            false,
        ),
        AccountMeta::new_readonly(
            if deprecated_caller {
                deprecated_program_id
            } else {
                invoke_program_id
            },
            false,
        ),
    ];

    (fixture, mint_keypair, account_keypair, account_metas)
}

// This tests the case where a caller extends an account beyond the original
// data length. The callee should see the extended data (asserted in the
// callee program, not here).
#[test_matrix([false, true], [false, true], [false, true])]
#[cfg(feature = "sbf_rust")]
fn test_cpi_account_data_updates_caller_grows(
    deprecated_callee: bool,
    deprecated_caller: bool,
    virtual_address_space_adjustments: bool,
) {
    let (mut fixture, mint_keypair, account_keypair, account_metas) =
        cpi_account_data_updates_fixture(
            deprecated_callee,
            deprecated_caller,
            virtual_address_space_adjustments,
        );

    let mut account = Account::new(42, 0, &account_metas[3].pubkey);
    account.data = b"foo".to_vec();
    fixture.replace_account(account_keypair.pubkey(), account);

    let mut instruction_data = vec![TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS];
    instruction_data.extend_from_slice(b"bar");

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                account_metas[3].pubkey,
                &instruction_data,
                account_metas,
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );

    if deprecated_caller {
        assert_eq!(
            effects.status,
            Err(TransactionError::InstructionError(
                0,
                if virtual_address_space_adjustments {
                    InstructionError::ProgramFailedToComplete
                } else {
                    InstructionError::ModifiedProgramId
                },
            )),
        );
    } else {
        assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
        // "bar" here was copied from the realloc region
        assert_eq!(
            effects.get_account(&account_keypair.pubkey()).unwrap().data,
            b"foobar",
        );
    }
}

// This tests the case where a callee extends an account beyond the original
// data length. The caller should see the extended data where the realloc
// region contains the new data. In this test the callee owns the account,
// the caller can't write but the CPI glue still updates correctly.
#[test_matrix([false, true], [false, true], [false, true])]
#[cfg(feature = "sbf_rust")]
fn test_cpi_account_data_updates_callee_grows(
    deprecated_callee: bool,
    deprecated_caller: bool,
    virtual_address_space_adjustments: bool,
) {
    let (mut fixture, mint_keypair, account_keypair, account_metas) =
        cpi_account_data_updates_fixture(
            deprecated_callee,
            deprecated_caller,
            virtual_address_space_adjustments,
        );

    let mut account = Account::new(42, 0, &account_metas[2].pubkey);
    account.data = b"foo".to_vec();
    fixture.replace_account(account_keypair.pubkey(), account);

    let mut instruction_data = vec![TEST_CPI_ACCOUNT_UPDATE_CALLEE_GROWS];
    instruction_data.extend_from_slice(b"bar");

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                account_metas[3].pubkey,
                &instruction_data,
                account_metas,
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    if deprecated_callee {
        assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
        // deprecated_callee is incapable of resizing accounts
        assert_eq!(
            effects.get_account(&account_keypair.pubkey()).unwrap().data,
            b"foo",
        );
    } else if deprecated_caller {
        assert_eq!(
            effects.status,
            Err(TransactionError::InstructionError(
                0,
                if virtual_address_space_adjustments {
                    InstructionError::InvalidRealloc
                } else {
                    InstructionError::ExternalAccountDataModified
                },
            )),
        );
    } else {
        assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
        // "bar" here was copied from the realloc region
        assert_eq!(
            effects.get_account(&account_keypair.pubkey()).unwrap().data,
            b"foobar",
        );
    }
}

// This tests the case where a callee shrinks an account, the caller data
// slice must be truncated accordingly and post_len..original_data_len must
// be zeroed (zeroing is checked in the invoked program not here). Same as
// above, the callee owns the account but the changes are still reflected in
// the caller even if things are readonly from the caller's POV.
#[test_matrix([false, true], [false, true], [false, true])]
#[cfg(feature = "sbf_rust")]
fn test_cpi_account_data_updates_callee_shrinks_smaller_than_original_len(
    deprecated_callee: bool,
    deprecated_caller: bool,
    virtual_address_space_adjustments: bool,
) {
    let (mut fixture, mint_keypair, account_keypair, account_metas) =
        cpi_account_data_updates_fixture(
            deprecated_callee,
            deprecated_caller,
            virtual_address_space_adjustments,
        );

    let mut account = Account::new(42, 0, &account_metas[2].pubkey);
    account.data = b"foobar".to_vec();
    fixture.replace_account(account_keypair.pubkey(), account);

    let mut instruction_data = vec![
        TEST_CPI_ACCOUNT_UPDATE_CALLEE_SHRINKS_SMALLER_THAN_ORIGINAL_LEN,
        virtual_address_space_adjustments as u8,
    ];
    instruction_data.extend_from_slice(4usize.to_le_bytes().as_ref());

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                account_metas[3].pubkey,
                &instruction_data,
                account_metas,
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    if deprecated_callee {
        assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
        // deprecated_callee is incapable of resizing accounts
        assert_eq!(
            effects.get_account(&account_keypair.pubkey()).unwrap().data,
            b"foobar",
        );
    } else if deprecated_caller {
        assert_eq!(
            effects.status,
            Err(TransactionError::InstructionError(
                0,
                if virtual_address_space_adjustments && deprecated_callee {
                    InstructionError::InvalidRealloc
                } else {
                    InstructionError::ExternalAccountDataModified
                },
            )),
        );
    } else {
        assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
        assert_eq!(
            effects.get_account(&account_keypair.pubkey()).unwrap().data,
            b"foob",
        );
    }
}

// This tests the case where the program extends an account, then calls
// itself and in the inner call it shrinks the account to a size that is
// still larger than the original size. The account data must be set to the
// correct value in the caller frame, and the realloc region must be zeroed
// (again tested in the invoked program).
#[test_matrix([false, true], [false, true], [false, true])]
#[cfg(feature = "sbf_rust")]
fn test_cpi_account_data_updates_caller_grows_callee_shrinks_larger_than_original_len(
    deprecated_callee: bool,
    deprecated_caller: bool,
    virtual_address_space_adjustments: bool,
) {
    let (mut fixture, mint_keypair, account_keypair, account_metas) =
        cpi_account_data_updates_fixture(
            deprecated_callee,
            deprecated_caller,
            virtual_address_space_adjustments,
        );

    let mut account = Account::new(42, 0, &account_metas[3].pubkey);
    account.data = b"foo".to_vec();
    fixture.replace_account(account_keypair.pubkey(), account);

    let mut instruction_data = vec![
        TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS_CALLEE_SHRINKS,
        virtual_address_space_adjustments as u8,
    ];
    // realloc to "foobazbad" then shrink to "foobazb"
    instruction_data.extend_from_slice(7usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(b"bazbad");

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                account_metas[3].pubkey,
                &instruction_data,
                account_metas,
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    if deprecated_caller {
        assert_eq!(
            effects.status,
            Err(TransactionError::InstructionError(
                0,
                if virtual_address_space_adjustments {
                    InstructionError::ProgramFailedToComplete
                } else {
                    InstructionError::ModifiedProgramId
                },
            )),
        );
    } else {
        assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
        assert_eq!(
            effects.get_account(&account_keypair.pubkey()).unwrap().data,
            b"foobazb",
        );
    }
}

// This tests the case where the program extends an account and the nested
// invocation then shrinks it below the original data length. Both the spare
// capacity at the end of the account data and the realloc region must be
// zeroed.
#[test_matrix([false, true], [false, true], [false, true])]
#[cfg(feature = "sbf_rust")]
fn test_cpi_account_data_updates_caller_grows_callee_shrinks_smaller_than_original_len(
    deprecated_callee: bool,
    deprecated_caller: bool,
    virtual_address_space_adjustments: bool,
) {
    let (mut fixture, mint_keypair, account_keypair, account_metas) =
        cpi_account_data_updates_fixture(
            deprecated_callee,
            deprecated_caller,
            virtual_address_space_adjustments,
        );

    let mut account = Account::new(42, 0, &account_metas[3].pubkey);
    account.data = b"foo".to_vec();
    fixture.replace_account(account_keypair.pubkey(), account);

    let mut instruction_data = vec![
        TEST_CPI_ACCOUNT_UPDATE_CALLER_GROWS_CALLEE_SHRINKS,
        virtual_address_space_adjustments as u8,
    ];
    // realloc to "foobazbad" then shrink to "f"
    instruction_data.extend_from_slice(1usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(b"bazbad");

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                account_metas[3].pubkey,
                &instruction_data,
                account_metas,
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    if deprecated_caller {
        assert_eq!(
            effects.status,
            Err(TransactionError::InstructionError(
                0,
                if virtual_address_space_adjustments {
                    InstructionError::ProgramFailedToComplete
                } else {
                    InstructionError::ModifiedProgramId
                },
            )),
        );
    } else {
        assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
        assert_eq!(
            effects.get_account(&account_keypair.pubkey()).unwrap().data,
            b"f",
        );
    }
}

#[test]
#[cfg(any(feature = "sbf_c", feature = "sbf_rust"))]
fn test_cpi_invalid_account_info_pointers() {
    let mut fixture = ProgramSbfTxnFixture::new(vec![
        #[cfg(feature = "sbf_rust")]
        ProgramSpec::upgradeable("solana_sbf_rust_invoke"),
        #[cfg(feature = "sbf_c")]
        ProgramSpec::upgradeable("invoke"),
    ]);
    let mint_keypair = fixture.add_account(None);
    let account_keypair = fixture.add_account(None);
    let program_ids = fixture.program_ids.clone();

    let mut account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new(account_keypair.pubkey(), false),
    ];
    account_metas.extend(
        program_ids
            .iter()
            .map(|program_id| AccountMeta::new_readonly(*program_id, false)),
    );

    for invoke_program_id in program_ids {
        for ix in [
            TEST_CPI_INVALID_KEY_POINTER,
            TEST_CPI_INVALID_LAMPORTS_POINTER,
            TEST_CPI_INVALID_OWNER_POINTER,
            TEST_CPI_INVALID_DATA_POINTER,
        ] {
            fixture.replace_account(
                account_keypair.pubkey(),
                Account::new(42, 5, &invoke_program_id),
            );

            let sanitized_message = SanitizedMessage::try_from_legacy_message(
                Message::new(
                    &[Instruction::new_with_bytes(
                        invoke_program_id,
                        &[ix, 42, 42, 42],
                        account_metas.clone(),
                    )],
                    Some(&mint_keypair.pubkey()),
                ),
                &ReservedAccountKeys::empty_key_set(),
            )
            .unwrap();

            let context = TxnContext::new_with_default_budget(
                fixture.feature_set.clone(),
                fixture.accounts.clone(),
                sanitized_message,
                None,
            );

            let effects = execute_txn(
                &context,
                &mut fixture.program_cache,
                &default_sysvar_cache(),
            );
            assert!(effects.status.is_err(), "{:?}", effects.status);
            assert!(
                effects
                    .logs
                    .iter()
                    .any(|log| log.contains("Invalid pointer")),
                "{:?}",
                effects.logs,
            );
        }
    }
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_deplete_cost_meter_with_access_violation() {
    const COMPUTE_UNIT_LIMIT: u32 = 10_000;

    let mut fixture =
        ProgramSbfTxnFixture::new(vec![ProgramSpec::upgradeable("solana_sbf_rust_invoke")]);
    let payer = fixture
        .add_account(Some(Account::new(50_000, 0, &system_program::id())))
        .pubkey();
    let mint_keypair = fixture.add_account(None);
    let account_keypair = fixture.add_account(None);
    let invoke_program_id = fixture.program_ids[0];
    fixture.add_accounts_with_pubkeys([keyed_account_for_compute_budget_program()]);

    let account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new(account_keypair.pubkey(), false),
        AccountMeta::new_readonly(invoke_program_id, false),
    ];

    let mut instruction_data = vec![TEST_WRITE_ACCOUNT, 2];
    instruction_data.extend_from_slice(3usize.to_le_bytes().as_ref());
    instruction_data.push(42);

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(COMPUTE_UNIT_LIMIT),
                Instruction::new_with_bytes(
                    invoke_program_id,
                    &instruction_data,
                    account_metas.clone(),
                ),
            ],
            Some(&payer),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set,
        fixture.accounts,
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            1,
            InstructionError::ReadonlyDataModified,
        )),
    );
    // all compute unit limit should be consumed due to SBF VM error
    assert_eq!(effects.executed_units, (COMPUTE_UNIT_LIMIT as u64));
}

#[test]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_deplete_cost_meter_with_divide_by_zero() {
    agave_logger::setup();

    let program_elf = load_program_elf("solana_sbf_rust_divide_by_zero");
    let program_id = Pubkey::new_unique();

    let feature_set = SVMFeatureSet::all_enabled();

    let mut program_cache =
        default_program_cache_with_program(&program_id, &program_elf, &feature_set);

    let instruction = Instruction::new_with_bytes(program_id, &[], vec![]);

    let context = InstrContext {
        feature_set,
        accounts: upgradeable_program_accounts(&program_id, &program_elf),
        instruction,
        cu_avail: 10_000,
    };

    let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());

    assert_eq!(
        effects.result,
        Some(InstructionError::ProgramFailedToComplete)
    );

    // all compute unit limit should be consumed due to SBF VM error
    assert_eq!(effects.cu_avail, 0);
}

#[test_matrix([false, true], [1, 2])]
#[cfg(feature = "sbf_rust")]
fn test_deny_access_beyond_current_length(
    virtual_address_space_adjustments: bool,
    instruction_account_index: u8,
) {
    let configured_feature_set =
        feature_set_with_account_mapping_features(virtual_address_space_adjustments);

    let mut fixture = ProgramSbfTxnFixture::new_with_feature_set(
        vec![ProgramSpec::upgradeable("solana_sbf_rust_invoke")],
        configured_feature_set,
    );
    let payer = fixture
        .add_account(Some(Account::new(50_000, 0, &system_program::id())))
        .pubkey();
    let mint_keypair = fixture.add_account(None);
    let account = Account::new(42, 0, &fixture.program_ids[0]);
    let readonly_account_keypair = fixture.add_account(Some(account.clone()));
    let writable_account_keypair = fixture.add_account(Some(account));
    let invoke_program_id = fixture.program_ids[0];

    let account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new_readonly(readonly_account_keypair.pubkey(), false),
        AccountMeta::new(writable_account_keypair.pubkey(), false),
        AccountMeta::new_readonly(invoke_program_id, false),
    ];

    let mut instruction_data = vec![TEST_READ_ACCOUNT, instruction_account_index];
    instruction_data.extend_from_slice(3usize.to_le_bytes().as_ref());

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas,
            )],
            Some(&payer),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set,
        fixture.accounts,
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    assert_eq!(
        effects.status,
        if virtual_address_space_adjustments {
            let expected_error = match instruction_account_index {
                1 => InstructionError::AccountDataTooSmall,
                2 => InstructionError::InvalidRealloc,
                _ => unreachable!(),
            };
            Err(TransactionError::InstructionError(0, expected_error))
        } else {
            Ok(())
        }
    );
}

#[test_case(false; "without_virtual_address_space_adjustments")]
#[test_case(true; "with_virtual_address_space_adjustments")]
#[cfg(feature = "sbf_rust")]
fn test_deny_executable_write(virtual_address_space_adjustments: bool) {
    let configured_feature_set =
        feature_set_with_account_mapping_features(virtual_address_space_adjustments);

    let mut fixture = ProgramSbfTxnFixture::new_with_feature_set(
        vec![ProgramSpec::upgradeable("solana_sbf_rust_invoke")],
        configured_feature_set,
    );
    let payer = fixture
        .add_account(Some(Account::new(50_000, 0, &system_program::id())))
        .pubkey();
    let mint_keypair = fixture.add_account(None);
    let account_keypair = fixture.add_account(None);
    let invoke_program_id = fixture.program_ids[0];

    let account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new(account_keypair.pubkey(), false),
        AccountMeta::new_readonly(invoke_program_id, false),
    ];

    let mut instruction_data = vec![TEST_WRITE_ACCOUNT, 2];
    instruction_data.extend_from_slice(3usize.to_le_bytes().as_ref());
    instruction_data.push(42);

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas,
            )],
            Some(&payer),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set,
        fixture.accounts,
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    assert_eq!(
        effects.status,
        Err(TransactionError::InstructionError(
            0,
            InstructionError::ReadonlyDataModified,
        )),
    );
}

// Test that fn update_callee_account() works and we are updating the callee account on CPI.
#[test_case(false; "without_virtual_address_space_adjustments")]
#[test_case(true; "with_virtual_address_space_adjustments")]
#[cfg(feature = "sbf_rust")]
fn test_update_callee_account(virtual_address_space_adjustments: bool) {
    let configured_feature_set =
        feature_set_with_account_mapping_features(virtual_address_space_adjustments);

    let mut fixture = ProgramSbfTxnFixture::new_with_feature_set(
        vec![ProgramSpec::upgradeable("solana_sbf_rust_invoke")],
        configured_feature_set,
    );
    let mint_keypair = fixture.add_account(None);
    let account_keypair = fixture.add_account(None);
    let invoke_program_id = fixture.program_ids[0];

    let account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new(account_keypair.pubkey(), false),
        AccountMeta::new_readonly(invoke_program_id, false),
    ];

    // I. do CPI with account in read only (separate code path with virtual_address_space_adjustments)
    let mut account = Account::new(42, 10240, &invoke_program_id);
    let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
    account.data = data;
    fixture.replace_account(account_keypair.pubkey(), account);

    let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 0, 0];
    instruction_data.extend_from_slice(20480usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(16384usize.to_le_bytes().as_ref());
    // instruction data for inner CPI (2x)
    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());

    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas.clone(),
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );

    let data = &effects.get_account(&account_keypair.pubkey()).unwrap().data;
    assert_eq!(data.len(), 20480);
    data.iter().enumerate().for_each(|(i, v)| {
        let expected = match i {
            ..=10240 => i as u8,
            16384 => 0xe5,
            _ => 0,
        };

        assert_eq!(*v, expected, "offset:{i} {v:#x} != {expected:#x}");
    });
    fixture.preserve_accounts_state(effects);

    // II. do CPI with account with resize to smaller and write
    let mut account = Account::new(42, 10240, &invoke_program_id);
    let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
    account.data = data;
    fixture.replace_account(account_keypair.pubkey(), account);

    let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 0];
    instruction_data.extend_from_slice(20480usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(16384usize.to_le_bytes().as_ref());
    // instruction data for inner CPI
    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
    instruction_data.extend_from_slice(19480usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(8129usize.to_le_bytes().as_ref());

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas.clone(),
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );

    let data = &effects.get_account(&account_keypair.pubkey()).unwrap().data;
    assert_eq!(data.len(), 19480);
    data.iter().enumerate().for_each(|(i, v)| {
        let expected = match i {
            8129 => (i as u8) ^ 0xe5,
            ..=10240 => i as u8,
            16384 => 0xe5,
            _ => 0,
        };

        assert_eq!(*v, expected, "offset:{i} {v:#x} != {expected:#x}");
    });
    fixture.preserve_accounts_state(effects);

    // III. do CPI with account with resize to larger and write
    let mut account = Account::new(42, 10240, &invoke_program_id);
    let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
    account.data = data;
    fixture.replace_account(account_keypair.pubkey(), account);

    let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 0];
    instruction_data.extend_from_slice(16384usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(16384usize.to_le_bytes().as_ref());
    // instruction data for inner CPI
    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
    instruction_data.extend_from_slice(20480usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(16385usize.to_le_bytes().as_ref());

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas.clone(),
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );

    let data = &effects.get_account(&account_keypair.pubkey()).unwrap().data;
    assert_eq!(data.len(), 20480);
    data.iter().enumerate().for_each(|(i, v)| {
        let expected = match i {
            ..=10240 => i as u8,
            16384 | 16385 => 0xe5,
            _ => 0,
        };

        assert_eq!(*v, expected, "offset:{i} {v:#x} != {expected:#x}");
    });
    fixture.preserve_accounts_state(effects);

    // IV. do CPI with account with resize to larger and write
    let mut account = Account::new(42, 10240, &invoke_program_id);
    let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
    account.data = data;
    fixture.replace_account(account_keypair.pubkey(), account);

    let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 0];
    instruction_data.extend_from_slice(16384usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(16384usize.to_le_bytes().as_ref());
    // instruction data for inner CPI (2x)
    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 1, 0]);
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());

    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 1, 0]);
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    // instruction data for inner CPI
    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
    instruction_data.extend_from_slice(20480usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(16385usize.to_le_bytes().as_ref());

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas.clone(),
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );

    let data = &effects.get_account(&account_keypair.pubkey()).unwrap().data;
    assert_eq!(data.len(), 20480);
    data.iter().enumerate().for_each(|(i, v)| {
        let expected = match i {
            ..=10240 => i as u8,
            16384 | 16385 => 0xe5,
            _ => 0,
        };

        assert_eq!(*v, expected, "offset:{i} {v:#x} != {expected:#x}");
    });
    fixture.preserve_accounts_state(effects);

    // V. clone data, modify and CPI
    let mut account = Account::new(42, 10240, &invoke_program_id);
    let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
    account.data = data;
    fixture.replace_account(account_keypair.pubkey(), account);

    let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 1];
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(8190usize.to_le_bytes().as_ref());

    // instruction data for inner CPI
    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 1, 0]);
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(8191usize.to_le_bytes().as_ref());

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas.clone(),
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );

    if virtual_address_space_adjustments {
        // changing the data pointer is not permitted
        assert!(effects.status.is_err(), "{:?}", effects.logs);
    } else {
        assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);

        let data = &effects.get_account(&account_keypair.pubkey()).unwrap().data;
        assert_eq!(data.len(), 10240);
        data.iter().enumerate().for_each(|(i, v)| {
            let expected = match i {
                // since the data is was cloned, the write to 8191 was lost
                8190 => (i as u8) ^ 0xe5,
                ..=10240 => i as u8,
                _ => 0,
            };
            assert_eq!(*v, expected, "offset:{i} {v:#x} != {expected:#x}");
        });
    }
}

#[test_case(false; "without_syscall_parameter_address_restrictions")]
#[test_case(true; "with_syscall_parameter_address_restrictions")]
fn test_account_info_in_account(syscall_parameter_address_restrictions: bool) {
    let mut programs = Vec::new();
    #[cfg(feature = "sbf_c")]
    {
        programs.push("invoke");
    }
    #[cfg(feature = "sbf_rust")]
    {
        programs.push("solana_sbf_rust_invoke");
    }

    for program in programs {
        let configured_feature_set =
            feature_set_with_account_mapping_features(syscall_parameter_address_restrictions);

        let mut fixture = ProgramSbfTxnFixture::new_with_feature_set(
            vec![ProgramSpec::upgradeable(program)],
            configured_feature_set,
        );
        let payer = fixture
            .add_account(Some(Account::new(50_000, 0, &system_program::id())))
            .pubkey();
        let mint_keypair = fixture.add_account(None);
        let account_owner = fixture.program_ids[0];
        let account_keypair = fixture.add_account(Some(Account::new(42, 10240, &account_owner)));
        let invoke_program_id = fixture.program_ids[0];

        let account_metas = vec![
            AccountMeta::new(mint_keypair.pubkey(), true),
            AccountMeta::new(account_keypair.pubkey(), false),
            AccountMeta::new_readonly(invoke_program_id, false),
        ];

        let mut instruction_data = vec![TEST_ACCOUNT_INFO_IN_ACCOUNT];
        instruction_data.extend_from_slice(32usize.to_le_bytes().as_ref());

        let sanitized_message = SanitizedMessage::try_from_legacy_message(
            Message::new(
                &[Instruction::new_with_bytes(
                    invoke_program_id,
                    &instruction_data,
                    account_metas,
                )],
                Some(&payer),
            ),
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let context = TxnContext::new_with_default_budget(
            fixture.feature_set,
            fixture.accounts,
            sanitized_message,
            None,
        );

        let effects = execute_txn(
            &context,
            &mut fixture.program_cache,
            &default_sysvar_cache(),
        );
        if syscall_parameter_address_restrictions {
            assert!(effects.status.is_err(), "{program}: {:?}", effects.status);
        } else {
            assert_eq!(effects.status, Ok(()), "{program}");
        }
    }
}

#[test_matrix(
    [false, true],
    [TEST_ACCOUNT_INFO_LAMPORTS_RC, TEST_ACCOUNT_INFO_DATA_RC]
)]
fn test_account_info_rc_in_account(syscall_parameter_address_restrictions: bool, instruction: u8) {
    let configured_feature_set =
        feature_set_with_account_mapping_features(syscall_parameter_address_restrictions);

    let mut fixture = ProgramSbfTxnFixture::new_with_feature_set(
        vec![ProgramSpec::upgradeable("solana_sbf_rust_invoke")],
        configured_feature_set,
    );
    let payer = fixture
        .add_account(Some(Account::new(50_000, 0, &system_program::id())))
        .pubkey();
    let mint_keypair = fixture.add_account(None);
    let account_owner = fixture.program_ids[0];
    let account_keypair = fixture.add_account(Some(Account::new(42, 10240, &account_owner)));
    let invoke_program_id = fixture.program_ids[0];

    let account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new(account_keypair.pubkey(), false),
        AccountMeta::new_readonly(invoke_program_id, false),
    ];

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                invoke_program_id,
                &[instruction, 0, 0, 0],
                account_metas,
            )],
            Some(&payer),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set,
        fixture.accounts,
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    if syscall_parameter_address_restrictions {
        assert!(
            effects
                .logs
                .last()
                .unwrap()
                .ends_with(" failed: Invalid pointer"),
            "{:?}",
            effects.logs,
        );
        assert!(effects.status.is_err());
    } else {
        assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
    }
}

#[test]
fn test_clone_account_data() {
    let configured_feature_set = feature_set_with_account_mapping_features(false);
    let mut fixture = ProgramSbfTxnFixture::new_with_feature_set(
        vec![
            ProgramSpec::upgradeable("solana_sbf_rust_invoke"),
            ProgramSpec::upgradeable("solana_sbf_rust_invoke"),
        ],
        configured_feature_set,
    );
    let mint_keypair = fixture.add_account(None);
    let account_keypair = fixture.add_account(None);
    let invoke_program_id = fixture.program_ids[0];
    let invoke_program_id2 = fixture.program_ids[1];

    let account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new(account_keypair.pubkey(), false),
        AccountMeta::new_readonly(invoke_program_id2, false),
        AccountMeta::new_readonly(invoke_program_id, false),
    ];

    // I. clone data and CPI; modify data in callee.
    // Now the original data in the caller is unmodified, and we get a "instruction modified data of an account it does not own"
    // error in the caller
    let mut account = Account::new(42, 10240, &invoke_program_id2);
    let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
    account.data = data;
    fixture.replace_account(account_keypair.pubkey(), account);

    let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 1];
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());

    // instruction data for inner CPI: modify account
    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(8190usize.to_le_bytes().as_ref());

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas.clone(),
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    assert!(effects.status.is_err(), "{:?}", effects.status);
    let error = format!(
        "Program {invoke_program_id} failed: instruction modified data of an account it does not \
         own"
    );
    assert!(
        effects.logs.iter().any(|log| log.contains(&error)),
        "{:?}",
        effects.logs,
    );

    // II. clone data, modify and then CPI
    // The deserialize checks should verify that we're not allowed to modify an account we don't own, even though
    // we have only modified a copy of the data. Fails in caller
    let mut account = Account::new(42, 10240, &invoke_program_id2);
    let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
    account.data = data;
    fixture.replace_account(account_keypair.pubkey(), account);

    let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 1];
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(8190usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());

    // instruction data for inner CPI
    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas.clone(),
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    assert!(effects.status.is_err(), "{:?}", effects.status);
    let error = format!(
        "Program {invoke_program_id} failed: instruction modified data of an account it does not \
         own"
    );
    assert!(
        effects.logs.iter().any(|log| log.contains(&error)),
        "{:?}",
        effects.logs,
    );

    // III. Clone data, call, modifiy in callee and then make the same change in the caller - transaction succeeds
    // Note the caller needs to modify the original account data, not the copy
    let mut account = Account::new(42, 10240, &invoke_program_id2);
    let data: Vec<u8> = (0..10240).map(|n| n as u8).collect();
    account.data = data;
    fixture.replace_account(account_keypair.pubkey(), account);

    let mut instruction_data = vec![TEST_CALLEE_ACCOUNT_UPDATES, 1, 1];
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(8190usize.to_le_bytes().as_ref());

    // instruction data for inner CPI
    instruction_data.extend_from_slice(&[TEST_CALLEE_ACCOUNT_UPDATES, 0, 0]);
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(0usize.to_le_bytes().as_ref());
    instruction_data.extend_from_slice(8190usize.to_le_bytes().as_ref());

    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                invoke_program_id,
                &instruction_data,
                account_metas.clone(),
            )],
            Some(&mint_keypair.pubkey()),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set.clone(),
        fixture.accounts.clone(),
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );
    // works because the account is exactly the same in caller as callee
    assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
}

#[test]
fn test_stack_heap_zeroed() {
    const COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

    let mut fixture =
        ProgramSbfTxnFixture::new(vec![ProgramSpec::upgradeable("solana_sbf_rust_invoke")]);
    let payer = fixture
        .add_account(Some(Account::new(50_000, 0, &system_program::id())))
        .pubkey();
    let mint_keypair = fixture.add_account(None);
    let account_keypair = fixture.add_account(None);
    let invoke_program_id = fixture.program_ids[0];
    fixture.add_accounts_with_pubkeys([keyed_account_for_compute_budget_program()]);

    let account_metas = vec![
        AccountMeta::new(mint_keypair.pubkey(), true),
        AccountMeta::new(account_keypair.pubkey(), false),
        AccountMeta::new_readonly(invoke_program_id, false),
    ];

    // Check multiple heap sizes. It's generally a good idea, and also it's needed to ensure that
    // pooled heap and stack values are reused - and therefore zeroed - across executions.
    for heap_len in [32usize * 1024, 64 * 1024, 128 * 1024, 256 * 1024] {
        // TEST_STACK_HEAP_ZEROED will recursively check that stack and heap are zeroed until it
        // reaches max CPI invoke depth. We make it fail at max depth so we're sure that there's no
        // legit way to access non-zeroed stack and heap regions.
        let mut instruction_data = vec![TEST_STACK_HEAP_ZEROED];
        instruction_data.extend_from_slice(&heap_len.to_le_bytes());

        let sanitized_message = SanitizedMessage::try_from_legacy_message(
            Message::new(
                &[
                    ComputeBudgetInstruction::set_compute_unit_limit(COMPUTE_UNIT_LIMIT),
                    ComputeBudgetInstruction::request_heap_frame(heap_len as u32),
                    Instruction::new_with_bytes(
                        invoke_program_id,
                        &instruction_data,
                        account_metas.clone(),
                    ),
                ],
                Some(&payer),
            ),
            &ReservedAccountKeys::empty_key_set(),
        )
        .unwrap();

        let context = TxnContext {
            feature_set: fixture.feature_set.clone(),
            accounts: fixture.accounts.clone(),
            message: sanitized_message,
            nonce_fields: None,
            cu_avail: u64::from(COMPUTE_UNIT_LIMIT),
        };

        let effects = execute_txn(
            &context,
            &mut fixture.program_cache,
            &default_sysvar_cache(),
        );
        assert!(effects.status.is_err(), "{:?}", effects.status);
        assert!(
            effects
                .logs
                .iter()
                .any(|log| log.contains("Cross-program invocation call depth too deep")),
            "{:?}",
            effects.logs,
        );
    }
}

#[test]
// This function tests edge compiler edge cases when calling functions with more than five
// arguments and passing by value arguments with more than 16 bytes.
fn test_function_call_args() {
    let mut fixture =
        ProgramSbfTxnFixture::new(vec![ProgramSpec::upgradeable("solana_sbf_rust_call_args")]);
    let payer = fixture
        .add_account(Some(Account::new(50_000, 0, &system_program::id())))
        .pubkey();
    let mint_keypair = fixture.add_account(None);
    let account_keypair = fixture.add_account(None);
    let program_id = fixture.program_ids[0];

    #[derive(BorshSerialize, BorshDeserialize, PartialEq, Eq, Debug)]
    struct Test128 {
        a: u128,
        b: u128,
    }

    #[derive(BorshSerialize)]
    struct InputData {
        test_128: Test128,
        arg1: i64,
        arg2: i64,
        arg3: i64,
        arg4: i64,
        arg5: i64,
        arg6: i64,
        arg7: i64,
        arg8: i64,
    }

    #[derive(BorshDeserialize)]
    struct OutputData {
        res_128: u128,
        res_256: Test128,
        many_args_1: i64,
        many_args_2: i64,
    }

    let input_data = InputData {
        test_128: Test128 {
            a: rand::random::<u128>(),
            b: rand::random::<u128>(),
        },
        arg1: rand::random::<i64>(),
        arg2: rand::random::<i64>(),
        arg3: rand::random::<i64>(),
        arg4: rand::random::<i64>(),
        arg5: rand::random::<i64>(),
        arg6: rand::random::<i64>(),
        arg7: rand::random::<i64>(),
        arg8: rand::random::<i64>(),
    };

    let instruction_data = to_vec(&input_data).unwrap();
    let sanitized_message = SanitizedMessage::try_from_legacy_message(
        Message::new(
            &[Instruction::new_with_bytes(
                program_id,
                &instruction_data,
                vec![
                    AccountMeta::new(mint_keypair.pubkey(), true),
                    AccountMeta::new(account_keypair.pubkey(), false),
                ],
            )],
            Some(&payer),
        ),
        &ReservedAccountKeys::empty_key_set(),
    )
    .unwrap();

    let context = TxnContext::new_with_default_budget(
        fixture.feature_set,
        fixture.accounts,
        sanitized_message,
        None,
    );

    let effects = execute_txn(
        &context,
        &mut fixture.program_cache,
        &default_sysvar_cache(),
    );

    let decoded: OutputData = from_slice::<OutputData>(&effects.return_data).unwrap();
    assert_eq!(
        decoded.res_128,
        input_data.test_128.a % input_data.test_128.b
    );
    assert_eq!(
        decoded.res_256,
        Test128 {
            a: input_data
                .test_128
                .a
                .overflowing_add(input_data.test_128.b)
                .0,
            b: input_data
                .test_128
                .a
                .overflowing_sub(input_data.test_128.b)
                .0
        }
    );

    fn verify_many_args(input: &InputData) -> i64 {
        let a = input
            .arg1
            .overflowing_add(input.arg2)
            .0
            .overflowing_sub(input.arg3)
            .0
            .overflowing_add(input.arg4)
            .0
            .overflowing_sub(input.arg5)
            .0;
        (a % input.arg6)
            .overflowing_sub(input.arg7)
            .0
            .overflowing_add(input.arg8)
            .0
    }

    assert_eq!(decoded.many_args_1, verify_many_args(&input_data));
    assert_eq!(decoded.many_args_2, verify_many_args(&input_data));
}

#[test_case(false; "without_virtual_address_space_adjustments")]
#[test_case(true; "with_virtual_address_space_adjustments")]
#[cfg(feature = "sbf_rust")]
fn test_mem_syscalls_overlap_account_begin_or_end(virtual_address_space_adjustments: bool) {
    let configured_feature_set =
        feature_set_with_account_mapping_features(virtual_address_space_adjustments);

    let mut fixture = ProgramSbfTxnFixture::new_with_feature_set(
        vec![
            ProgramSpec::upgradeable("solana_sbf_rust_account_mem"),
            ProgramSpec::deprecated("solana_sbf_rust_account_mem_deprecated"),
        ],
        configured_feature_set,
    );
    let mint_keypair = fixture.add_account(None);
    let account_keypair = fixture.add_account(None);
    let upgradeable_program_id = fixture.program_ids[0];
    let deprecated_program_id = fixture.program_ids[1];

    for deprecated in [false, true] {
        let program_id = if deprecated {
            deprecated_program_id
        } else {
            upgradeable_program_id
        };
        let account_metas = vec![
            AccountMeta::new(mint_keypair.pubkey(), true),
            AccountMeta::new_readonly(program_id, false),
            AccountMeta::new(account_keypair.pubkey(), false),
        ];

        for account_data_len in [1024, 0] {
            fixture.replace_account(
                account_keypair.pubkey(),
                Account::new(42, account_data_len, &program_id),
            );

            for instr in 0..=15 {
                let instruction_data = if account_data_len == 0 {
                    vec![instr, 0]
                } else {
                    vec![instr]
                };
                let sanitized_message = SanitizedMessage::try_from_legacy_message(
                    Message::new(
                        &[Instruction::new_with_bytes(
                            program_id,
                            &instruction_data,
                            account_metas.clone(),
                        )],
                        Some(&mint_keypair.pubkey()),
                    ),
                    &ReservedAccountKeys::empty_key_set(),
                )
                .unwrap();

                let context = TxnContext::new_with_default_budget(
                    fixture.feature_set.clone(),
                    fixture.accounts.clone(),
                    sanitized_message,
                    None,
                );
                let effects = execute_txn(
                    &context,
                    &mut fixture.program_cache,
                    &default_sysvar_cache(),
                );

                if virtual_address_space_adjustments && account_data_len != 0 {
                    assert!(
                        effects
                            .logs
                            .last()
                            .unwrap()
                            .contains(" failed: Access violation"),
                        "{:?}",
                        effects.logs,
                    );
                } else if virtual_address_space_adjustments && (!deprecated || instr < 8) {
                    let last_line = effects.logs.last().unwrap();
                    assert!(
                        last_line.contains(" failed: account data too small")
                            || last_line.contains(" failed: Failed to reallocate account data")
                            || last_line.contains(" failed: Access violation"),
                        "{:?}",
                        effects.logs,
                    );
                } else {
                    // virtual_address_space_adjustments && deprecated && instr >= 8 succeeds with zero-length accounts
                    // because there is no MemoryRegion for the account,
                    // so there can be no error when leaving that non-existent region.
                    assert_eq!(effects.status, Ok(()), "{:?}", effects.logs);
                }

                // Preserve Bank behavior: commit successful transaction effects for the next
                // instruction, while leaving account state unchanged after failed transactions.
                if effects.status.is_ok() {
                    fixture.preserve_accounts_state(effects);
                }
            }
        }
    }
}

#[test_matrix(
    [0, 1, 2, 5, 10, 15, 20, 32],
    [1, 10, 50, 100, 255, 500, 1000, 1024]
)]
#[allow(clippy::arithmetic_side_effects)]
#[cfg(feature = "sbf_rust")]
fn test_program_sbf_rust_direct_account_pointers(num_accounts: usize, input_data_len: usize) {
    agave_logger::setup();

    let program_elf = load_program_elf("solana_sbf_rust_direct_account_pointers");
    let program_id = Pubkey::new_unique();

    let feature_set = SVMFeatureSet::all_enabled();

    let mut program_cache =
        default_program_cache_with_program(&program_id, &program_elf, &feature_set);

    let mut accounts = Vec::new();
    let mut account_metas = Vec::new();

    for i in 0..num_accounts {
        let pubkey = Pubkey::new_unique();

        // Mixed account sizes.
        accounts.push((pubkey, Account::new(0, 100 + (i * 50), &program_id)));

        // Mixed account roles.
        if i % 2 == 0 {
            account_metas.push(AccountMeta::new(pubkey, false));
        } else {
            account_metas.push(AccountMeta::new_readonly(pubkey, false));
        }
    }

    // Add `num_accounts` duplicated accounts.
    for i in 0..num_accounts {
        let pubkey = accounts[i].0;
        account_metas.push(AccountMeta::new(pubkey, false));
    }

    accounts.extend(upgradeable_program_accounts(&program_id, &program_elf));

    let input_data: Vec<u8> = (0..input_data_len).map(|i| (i % 256) as u8).collect();

    let instruction = Instruction::new_with_bytes(program_id, &input_data, account_metas);

    let context = InstrContext::new_with_default_budget(feature_set, accounts, instruction);

    let effects = execute_instr(&context, &mut program_cache, &default_sysvar_cache());

    assert!(effects.result.is_none());
    // `num_accounts * 2` will be added as return data.
    assert_eq!(
        (num_accounts * 2).to_le_bytes().to_vec(),
        effects.return_data
    );
}
