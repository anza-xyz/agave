#![cfg(test)]
use {
    solana_account::{Account, ReadableAccount},
    solana_clock::MAX_PROCESSING_AGE,
    solana_compute_budget::compute_budget_limits::MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT,
    solana_compute_budget_interface::ComputeBudgetInstruction,
    solana_cost_model::cost_model::CostModel,
    solana_genesis_config::{create_genesis_config, GenesisConfig},
    solana_instruction::{error::InstructionError, AccountMeta, Instruction},
    solana_keypair::Keypair,
    solana_loader_v3_interface::state::UpgradeableLoaderState,
    solana_message::Message,
    solana_native_token::LAMPORTS_PER_SOL,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_runtime::bank::Bank,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sdk_ids::{bpf_loader_upgradeable, secp256k1_program},
    solana_signer::Signer,
    solana_svm::transaction_processor::ExecutionRecordingConfig,
    solana_svm_timings::ExecuteTimings,
    solana_system_interface::instruction as system_instruction,
    solana_transaction::Transaction,
    solana_transaction_error::{TransactionError, TransactionResult as Result},
};

#[derive(Debug, Eq, PartialEq)]
struct TestResult {
    // execution cost adjustment (eg estimated_execution_cost -
    // actual_execution_cost) if *committed* successfully; Which always the case for our tests
    cost_adjustment: i64,
    // Ok(()) if transaction executed successfully, otherwise error
    execution_status: Result<()>,
}

#[allow(dead_code)]
struct TestSetup {
    genesis_config: GenesisConfig,
    mint_keypair: Keypair,
}

impl TestSetup {
    fn new() -> Self {
        let (mut genesis_config, mint_keypair) = create_genesis_config(LAMPORTS_PER_SOL);
        genesis_config.rent = Rent::default();
        Self {
            genesis_config,
            mint_keypair,
        }
    }

    fn install_memo_program_account(&mut self) {
        let (pubkey, account) = solana_program_binaries::by_id(
            &spl_memo_interface::v3::id(),
            &self.genesis_config.rent,
        )
        .unwrap()
        .swap_remove(0);

        self.genesis_config.add_account(pubkey, account);
    }

    fn execute_test_transaction(&mut self, ixs: &[Instruction]) -> TestResult {
        let root_bank = Bank::new_for_tests(&self.genesis_config);
        let (bank, bank_forks) = root_bank.wrap_with_bank_forks_for_tests();
        let bank = Bank::new_from_parent_with_bank_forks(
            &bank_forks,
            bank,
            &Pubkey::default(),
            self.genesis_config
                .epoch_schedule
                .get_first_slot_in_epoch(1),
        );

        let tx = Transaction::new(
            &[&self.mint_keypair],
            Message::new(ixs, Some(&self.mint_keypair.pubkey())),
            self.genesis_config.hash(),
        );

        let estimated_execution_cost = CostModel::calculate_cost(
            &RuntimeTransaction::from_transaction_for_tests(tx.clone()),
            &bank.feature_set,
        )
        .programs_execution_cost();

        let batch = bank.prepare_batch_for_tests(vec![tx]);
        let commit_result = bank
            .load_execute_and_commit_transactions(
                &batch,
                MAX_PROCESSING_AGE,
                ExecutionRecordingConfig::new_single_setting(false),
                &mut ExecuteTimings::default(),
                None,
            )
            .0
            .remove(0);

        match commit_result {
            Ok(committed_tx) => TestResult {
                cost_adjustment: (estimated_execution_cost as i64)
                    .saturating_sub(committed_tx.executed_units as i64),
                execution_status: committed_tx.status,
            },
            Err(err) => {
                unreachable!(
                    "All test Transactions should be well-formatted for execution and commit, \
                     err: '{err}'",
                );
            }
        }
    }

    fn transfer_ix(&self) -> Instruction {
        system_instruction::transfer(
            &self.mint_keypair.pubkey(),
            &Pubkey::new_unique(),
            self.genesis_config.rent.minimum_balance(0),
        )
    }

    fn set_cu_limit_ix(&self, cu_limit: u32) -> Instruction {
        ComputeBudgetInstruction::set_compute_unit_limit(cu_limit)
    }

    fn set_cu_price_ix(&self, cu_price: u64) -> Instruction {
        ComputeBudgetInstruction::set_compute_unit_price(cu_price)
    }

    fn memo_ix(&self) -> (Instruction, u32) {
        // construct a memo instruction that would consume more CU than DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT
        let memo = "The quick brown fox jumped over the lazy dog. ".repeat(22) + "!";
        let memo_ix = spl_memo_interface::instruction::build_memo(
            &spl_memo_interface::v3::id(),
            memo.as_bytes(),
            &[],
        );
        let memo_ix_cost = 356_963;

        (memo_ix, memo_ix_cost)
    }

    #[allow(deprecated)]
    fn deploy_with_max_data_len_ix(&mut self) -> Instruction {
        let buffer_address = Pubkey::new_unique();
        let program_address = Pubkey::new_unique();
        let payer_address = self.mint_keypair.pubkey();
        let upgrade_authority_address = payer_address;

        let (_, memo) = solana_program_binaries::by_id(
            &spl_memo_interface::v3::id(),
            &self.genesis_config.rent,
        )
        .unwrap()
        .swap_remove(0);

        // Stash a valid buffer account before attempting a deployment.
        {
            let metadata_offset = UpgradeableLoaderState::size_of_buffer_metadata();
            let space = UpgradeableLoaderState::size_of_buffer(memo.data().len());
            let lamports = self.genesis_config.rent.minimum_balance(space);

            let mut data = vec![0; space];
            bincode::serialize_into(
                &mut data[..metadata_offset],
                &UpgradeableLoaderState::Buffer {
                    authority_address: Some(upgrade_authority_address),
                },
            )
            .unwrap();
            data[metadata_offset..].copy_from_slice(memo.data());

            self.genesis_config.accounts.insert(
                buffer_address,
                Account {
                    lamports,
                    data,
                    owner: bpf_loader_upgradeable::id(),
                    ..Default::default()
                },
            );
        }

        // Now stash the uninitialized program account. We're just going to
        // invoke the loader directly.
        {
            let space = UpgradeableLoaderState::size_of_program();
            let lamports = self.genesis_config.rent.minimum_balance(space);

            self.genesis_config.accounts.insert(
                program_address,
                Account {
                    lamports,
                    data: vec![0; space],
                    owner: bpf_loader_upgradeable::id(),
                    ..Default::default()
                },
            );
        }

        solana_loader_v3_interface::instruction::deploy_with_max_program_len(
            &payer_address,
            &program_address,
            &buffer_address,
            &upgrade_authority_address,
            /* program_lamports */ 0, // Doesn't matter here.
            /* max_data_len */ memo.data().len().saturating_mul(2),
        )
        .unwrap()
        .pop()
        .unwrap()
    }
}

#[test]
fn test_builtin_ix_cost_adjustment_with_cu_limit_too_low() {
    let mut test_setup = TestSetup::new();
    let cu_limit = 1;

    // A simple transfer ix, and request cu-limit to 1 cu
    // Cost model & Compute budget: reserve/allocate requested CU Limit `1`
    // VM Execution: consume `1` CU, then fail
    // Result: 0 adjustment
    let expected = TestResult {
        cost_adjustment: 0,
        execution_status: Err(TransactionError::InstructionError(
            0,
            InstructionError::ComputationalBudgetExceeded,
        )),
    };
    assert_eq!(
        expected,
        test_setup.execute_test_transaction(&[
            test_setup.transfer_ix(),
            test_setup.set_cu_limit_ix(cu_limit),
        ])
    );
}

#[test]
fn test_builtin_ix_cost_adjustment_with_cu_limit_high() {
    let mut test_setup = TestSetup::new();
    let cu_limit: u32 = 500_000;

    // A simple transfer ix, and request cu-limit to more than needed
    // Cost model & Compute budget: reserve/allocate requested CU Limit `500_000`
    // VM Execution: consume CUs for `system` and `compute-budget` programs, then success
    // Result: adjustment = 500_000 - 150 -150
    let expected = TestResult {
        cost_adjustment: cu_limit as i64
            - solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS as i64
            - solana_compute_budget_program::DEFAULT_COMPUTE_UNITS as i64,
        execution_status: Ok(()),
    };

    assert_eq!(
        expected,
        test_setup.execute_test_transaction(&[
            test_setup.transfer_ix(),
            test_setup.set_cu_limit_ix(cu_limit),
        ],)
    );
}

#[test]
fn test_builtin_ix_cost_adjustment_with_memo_no_cu_limit() {
    let mut test_setup = TestSetup::new();
    test_setup.install_memo_program_account();
    let (memo_ix, _memo_ix_cost) = test_setup.memo_ix();

    // A simple transfer ix, and a bpf ix (memo_ix) that needs 356_963 CUs
    // Cost model & Compute budget: reserve/allocate CU for 1 builtin and 1 non-builtin
    //   (3_000 + 200_000) = 203_000 CUs (note: less than memo_ix needs)
    // VM Execution: consume all allocated CUs, then fail
    // Result: no adjustment
    let expected = TestResult {
        cost_adjustment: 0,
        execution_status: Err(TransactionError::InstructionError(
            1,
            InstructionError::ProgramFailedToComplete,
        )),
    };
    assert_eq!(
        expected,
        test_setup.execute_test_transaction(&[test_setup.transfer_ix(), memo_ix.clone()],)
    );
}

#[test]
fn test_builtin_ix_cost_adjustment_with_memo_and_cu_limit() {
    let mut test_setup = TestSetup::new();
    test_setup.install_memo_program_account();
    let (memo_ix, memo_ix_cost) = test_setup.memo_ix();
    // request exact amount CUs needed to execute a transafer, compute_budget and a memo ix
    let cu_limit = memo_ix_cost
        + solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS as u32
        + solana_compute_budget_program::DEFAULT_COMPUTE_UNITS as u32;

    // A simple transfer ix, and a bpf ix (memo_ix) that needs 356_963 CUs,
    // and a compute-budget instruction that requests exact amount CUs.
    // Cost model & Compute budget: reserve/allocate requested CUs
    // VM Execution: consume all allocated CUs, then succeed
    // Result: no adjustment
    let expected = TestResult {
        cost_adjustment: 0,
        execution_status: Ok(()),
    };

    assert_eq!(
        expected,
        test_setup.execute_test_transaction(&[
            test_setup.transfer_ix(),
            memo_ix.clone(),
            test_setup.set_cu_limit_ix(cu_limit)
        ],)
    );
}

#[test]
fn test_builtin_ix_cost_adjustment_with_bpf_v3_no_cu_limit() {
    // A System & BPF Loader v3 ix. The latter CPIs into System.
    // Cost model & Compute budget: reserve/allocate default CU for 1 builtin
    // VM Execution: consume CUs for 1 BPF_L and 1 System (CPI-ed 1 time), then succeed
    // Result: adjustment = 3_000 - 2_370 - 150 = 480
    let expected = TestResult {
        cost_adjustment: MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT as i64
            - solana_bpf_loader_program::UPGRADEABLE_LOADER_COMPUTE_UNITS as i64
            - solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS as i64,
        execution_status: Ok(()),
    };

    let mut test_setup = TestSetup::new();
    let ix = test_setup.deploy_with_max_data_len_ix();
    assert_eq!(expected, test_setup.execute_test_transaction(&[ix]));
}

#[test]
fn test_builtin_ix_cost_adjustment_with_bpf_v3_and_cu_limit_high() {
    let cu_limit = 500_000;
    let tx_execution_cost = solana_bpf_loader_program::UPGRADEABLE_LOADER_COMPUTE_UNITS
        + solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS
        + solana_compute_budget_program::DEFAULT_COMPUTE_UNITS;

    // A BPF Loader v3 ix only, that CPIs into System instructions; and a compute-budget
    // instruction requests enough CU Limit
    // Cost model & Compute budget: reserve/allocate requested CUs
    // VM Execution: consume CUs for 1 BPF_L and 1 System (CPI-ed 1 time) and 1 Compute Budget, then succeed
    // Result: adjustment = 500_000 - 2_370 - 150 - 150 = 497_330

    let expected = TestResult {
        cost_adjustment: cu_limit as i64 - tx_execution_cost as i64,
        execution_status: Ok(()),
    };
    let mut test_setup = TestSetup::new();
    let ixs = [
        test_setup.deploy_with_max_data_len_ix(),
        test_setup.set_cu_limit_ix(cu_limit),
    ];
    assert_eq!(expected, test_setup.execute_test_transaction(&ixs));
}

#[test]
fn test_builtin_ix_set_cu_price_only() {
    let mut test_setup = TestSetup::new();
    let cu_price = 1;

    // single Compute Budget instruction to set CU Price
    // Cost model & Compute budget: reserve/allocate default CU for one builtin ix
    // VM Execution: consume CUs for 1 Compute Budget, then succeed
    // Result: adjustment = 3_000 - 150 = 2_850
    let expected = TestResult {
        cost_adjustment: MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT as i64
            - solana_compute_budget_program::DEFAULT_COMPUTE_UNITS as i64,
        execution_status: Ok(()),
    };
    assert_eq!(
        expected,
        test_setup.execute_test_transaction(&[test_setup.set_cu_price_ix(cu_price)],)
    );
}

#[test]
fn test_builtin_ix_precompiled() {
    let mut test_setup = TestSetup::new();

    // single precompiled instruction
    // Cost model & Compute budget: reserve/allocate default CU for one builtin ix
    // VM Execution: consume 0 from CU-meter
    // Result: adjustment = 3_000
    let expected = TestResult {
        cost_adjustment: MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT as i64,
        execution_status: Ok(()),
    };
    assert_eq!(
        expected,
        test_setup.execute_test_transaction(&[Instruction::new_with_bincode(
            secp256k1_program::id(),
            &[0u8],
            // Add a dummy account to generate a unique transaction
            vec![AccountMeta::new_readonly(Pubkey::new_unique(), false)]
        )],)
    );
}
