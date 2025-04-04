#![cfg(test)]
use {
    agave_feature_set as feature_set,
    solana_compute_budget::compute_budget_limits::{
        DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT, MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT,
    },
    solana_cost_model::cost_model::CostModel,
    solana_runtime::{bank::Bank, bank_forks::BankForks},
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_sdk::{
        account::Account,
        bpf_loader,
        bpf_loader_upgradeable::UpgradeableLoaderState,
        clock::{Slot, MAX_PROCESSING_AGE},
        compute_budget::ComputeBudgetInstruction,
        genesis_config::{create_genesis_config, GenesisConfig},
        instruction::{AccountMeta, Instruction, InstructionError},
        message::Message,
        native_token::sol_to_lamports,
        pubkey::Pubkey,
        rent::Rent,
        signature::{Keypair, Signer},
        system_instruction::{self},
        transaction::{Result, Transaction, TransactionError},
    },
    solana_svm::transaction_processor::ExecutionRecordingConfig,
    solana_timings::ExecuteTimings,
    std::sync::{Arc, RwLock},
};

const MEMO_PROGRAM_ELF: &[u8] = include_bytes!("../../program-test/src/programs/spl_memo-3.0.0.so");

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
        let (mut genesis_config, mint_keypair) = create_genesis_config(sol_to_lamports(1.));
        genesis_config.rent = Rent::default();
        Self {
            genesis_config,
            mint_keypair,
        }
    }

    fn install_memo_program_account(&mut self) {
        self.genesis_config.accounts.insert(
            spl_memo::id(),
            Account {
                lamports: u64::MAX,
                // borrows memo elf for executing memo ix in order to set up test condition
                data: MEMO_PROGRAM_ELF.to_vec(),
                owner: bpf_loader::id(),
                executable: true,
                rent_epoch: 0,
            },
        );
    }

    fn execute_test_transaction(
        &mut self,
        ixs: &[Instruction],
        is_simd_170_enabled: bool,
    ) -> TestResult {
        let mut root_bank = Bank::new_for_tests(&self.genesis_config);

        if is_simd_170_enabled {
            root_bank
                .activate_feature(&feature_set::reserve_minimal_cus_for_builtin_instructions::id());
        } else {
            root_bank.deactivate_feature(
                &feature_set::reserve_minimal_cus_for_builtin_instructions::id(),
            );
        }

        let (bank, bank_forks) = root_bank.wrap_with_bank_forks_for_tests();
        let bank = new_bank_from_parent_with_bank_forks(
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
                false,
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
                    "All test Transactions should be well-formatted for execution and commit, err: '{}'", err
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
        let memo_ix = spl_memo::build_memo(memo.as_bytes(), &[]);
        let memo_ix_cost = 356_963;

        (memo_ix, memo_ix_cost)
    }

    #[allow(deprecated)]
    fn deploy_with_max_data_len_ix(&mut self) -> Instruction {
        let buffer_address = Pubkey::new_unique();
        let program_address = Pubkey::new_unique();
        let payer_address = self.mint_keypair.pubkey();
        let upgrade_authority_address = payer_address;

        // Stash a valid buffer account before attempting a deployment.
        {
            let metadata_offset = UpgradeableLoaderState::size_of_buffer_metadata();
            let space = UpgradeableLoaderState::size_of_buffer(MEMO_PROGRAM_ELF.len());
            let lamports = self.genesis_config.rent.minimum_balance(space);

            let mut data = vec![0; space];
            bincode::serialize_into(
                &mut data[..metadata_offset],
                &UpgradeableLoaderState::Buffer {
                    authority_address: Some(upgrade_authority_address),
                },
            )
            .unwrap();
            data[metadata_offset..].copy_from_slice(MEMO_PROGRAM_ELF);

            self.genesis_config.accounts.insert(
                buffer_address,
                Account {
                    lamports,
                    data,
                    owner: solana_sdk::bpf_loader_upgradeable::id(),
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
                    owner: solana_sdk::bpf_loader_upgradeable::id(),
                    ..Default::default()
                },
            );
        }

        solana_sdk::bpf_loader_upgradeable::deploy_with_max_program_len(
            &payer_address,
            &program_address,
            &buffer_address,
            &upgrade_authority_address,
            /* program_lamports */ 0, // Doesn't matter here.
            /* max_data_len */ MEMO_PROGRAM_ELF.len().saturating_mul(2),
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
    for (is_simd_170_enabled, expected) in [
        // post #3799:
        // Cost model & Compute budget: reserve/allocate requested CU Limit `1`
        // VM Execution: consume `1` CU, then fail
        // Result: 0 adjustment
        (
            true,
            TestResult {
                cost_adjustment: 0,
                execution_status: Err(TransactionError::InstructionError(
                    0,
                    InstructionError::ComputationalBudgetExceeded,
                )),
            },
        ),
        // pre #3799:
        // Cost model: ignores requested CU Limit due to without bpf ixs, reserve CUs for `system` and
        //   `compute-budget` programs: (150 + 150) = 300;
        // Compute budget: allocate CU Meter to requested CU Limit `1`.
        // VM execution: consumed `1` CU, then fail
        // Result: adjustment = 300 - 1 = 299;
        (
            false,
            TestResult {
                cost_adjustment: 299,
                execution_status: Err(TransactionError::InstructionError(
                    0,
                    InstructionError::ComputationalBudgetExceeded,
                )),
            },
        ),
    ] {
        assert_eq!(
            expected,
            test_setup.execute_test_transaction(
                &[
                    test_setup.transfer_ix(),
                    test_setup.set_cu_limit_ix(cu_limit),
                ],
                is_simd_170_enabled
            )
        );
    }
}

#[test]
fn test_builtin_ix_cost_adjustment_with_cu_limit_high() {
    let mut test_setup = TestSetup::new();
    let cu_limit: u32 = 500_000;

    // A simple transfer ix, and request cu-limit to more than needed
    for (is_simd_170_enabled, expected) in [
        // post #3799:
        // Cost model & Compute budget: reserve/allocate requested CU Limit `500_000`
        // VM Execution: consume CUs for `system` and `compute-budget` programs, then success
        // Result: adjustment = 500_000 - 150 -150
        (
            true,
            TestResult {
                cost_adjustment: cu_limit as i64
                    - solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS as i64
                    - solana_compute_budget_program::DEFAULT_COMPUTE_UNITS as i64,
                execution_status: Ok(()),
            },
        ),
        // pre #3799:
        // Cost model: ignores requested CU Limit due to without bpf ixs, reserve CUs for `system` and
        //   `compute-budget` programs: (150 + 150) = 300;
        // Compute budget: allocate CU Meter to requested CU Limit `500_000`.
        // VM Execution: consume CUs for `system` and `compute-budget` programs, then success
        // Result: adjustment = 300 - 150 -150 = 0;
        (
            false,
            TestResult {
                cost_adjustment: 0,
                execution_status: Ok(()),
            },
        ),
    ] {
        assert_eq!(
            expected,
            test_setup.execute_test_transaction(
                &[
                    test_setup.transfer_ix(),
                    test_setup.set_cu_limit_ix(cu_limit),
                ],
                is_simd_170_enabled
            )
        );
    }
}

#[test]
fn test_builtin_ix_cost_adjustment_with_memo_no_cu_limit() {
    let mut test_setup = TestSetup::new();
    test_setup.install_memo_program_account();
    let (memo_ix, memo_ix_cost) = test_setup.memo_ix();

    // A simple transfer ix, and a bpf ix (memo_ix) that needs 356_963 CUs
    for (is_simd_170_enabled, expected) in [
        // post #3799:
        // Cost model & Compute budget: reserve/allocate CU for 1 builtin and 1 non-builtin
        //   (3_000 + 200_000) = 203_000 CUs (note: less than memo_ix needs)
        // VM Execution: consume all allocated CUs, then fail
        // Result: no adjustment
        (
            true,
            TestResult {
                cost_adjustment: 0,
                execution_status: Err(TransactionError::InstructionError(
                    1,
                    InstructionError::ProgramFailedToComplete,
                )),
            },
        ),
        // pre #3799:
        // Cost model: reserve CUs for `system` and default 200K for bpf ix
        //   (150 + 200_000) = 200_150 CUs
        // Compute budget: allocate default 200K CUs for non-compute-budget ixs:
        //   (2 * 200_000) = 400_000 (note: more than memo_ix needs)
        // VM Execution: consume CUs for `system` and bpf programs, then success
        // Result: adjustment = 200_150 - (150 + 356_963) = -156_963 CUs
        //   Note: negative adjustment means adjust-up.
        (
            false,
            TestResult {
                cost_adjustment: DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as i64
                    - memo_ix_cost as i64,
                execution_status: Ok(()),
            },
        ),
    ] {
        assert_eq!(
            expected,
            test_setup.execute_test_transaction(
                &[test_setup.transfer_ix(), memo_ix.clone()],
                is_simd_170_enabled
            )
        );
    }
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
    for (is_simd_170_enabled, expected) in [
        // post #3799:
        // Cost model & Compute budget: reserve/allocate requested CUs
        // VM Execution: consume all allocated CUs, then succeed
        // Result: no adjustment
        (
            true,
            TestResult {
                cost_adjustment: 0,
                execution_status: Ok(()),
            },
        ),
        // pre #3799:
        // Cost model: reserve requested CUs because there are bpf ix
        // Compute budget: allocate requested CUs,
        // VM Execution: consume all allocated CUs, then succeed,
        // Result: no adjustment
        (
            false,
            TestResult {
                cost_adjustment: 0,
                execution_status: Ok(()),
            },
        ),
    ] {
        assert_eq!(
            expected,
            test_setup.execute_test_transaction(
                &[
                    test_setup.transfer_ix(),
                    memo_ix.clone(),
                    test_setup.set_cu_limit_ix(cu_limit)
                ],
                is_simd_170_enabled
            )
        );
    }
}

#[test]
fn test_builtin_ix_cost_adjustment_with_bpf_v3_no_cu_limit() {
    // A System & BPF Loader v3 ix. The latter CPIs into System.
    for (is_simd_170_enabled, expected) in [
        // post #3799:
        // Cost model & Compute budget: reserve/allocate default CU for 1 builtin
        // VM Execution: consume CUs for 1 BPF_L and 1 System (CPI-ed 1 time), then succeed
        // Result: adjustment = 3_000 - 2_370 - 150 = 480
        (
            true,
            TestResult {
                cost_adjustment: MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT as i64
                    - solana_bpf_loader_program::UPGRADEABLE_LOADER_COMPUTE_UNITS as i64
                    - solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS as i64,
                execution_status: Ok(()),
            },
        ),
        // pre #3799:
        // Cost model: reserve 2_370 CU for 1 BPF_L instruction,
        // Compute budget: allocate 200K CU for one non-compute-budget instruction
        // VM Execution: consumeed CU for 1 BPF_L and 1 System, then succeed,
        // Result: adjustment = 2370 - (2370 + 150) = -150
        //   Note negative adjustment means adjust-up
        (
            false,
            TestResult {
                cost_adjustment: -(solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS
                    as i64),
                execution_status: Ok(()),
            },
        ),
    ] {
        let mut test_setup = TestSetup::new();
        let ix = test_setup.deploy_with_max_data_len_ix();
        assert_eq!(
            expected,
            test_setup.execute_test_transaction(&[ix], is_simd_170_enabled)
        );
    }
}

#[test]
fn test_builtin_ix_cost_adjustment_with_bpf_v3_and_cu_limit_high() {
    let cu_limit = 500_000;
    let tx_execution_cost = solana_bpf_loader_program::UPGRADEABLE_LOADER_COMPUTE_UNITS
        + solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS
        + solana_compute_budget_program::DEFAULT_COMPUTE_UNITS;

    // A BPF Loader v3 ix only, that CPIs into System instructions; and a compute-budget
    // instruction requests enough CU Limit
    for (is_simd_170_enabled, expected) in [
        // post #3799:
        // Cost model & Compute budget: reserve/allocate requested CUs
        // VM Execution: consume CUs for 1 BPF_L and 1 System (CPI-ed 1 time) and 1 Compute Budget, then succeed
        // Result: adjustment = 500_000 - 2_370 - 150 - 150 = 497_330
        (
            true,
            TestResult {
                cost_adjustment: cu_limit as i64 - tx_execution_cost as i64,
                execution_status: Ok(()),
            },
        ),
        // pre #3799:
        // Cost model: ignores requested CU limit because no bpf instruction, instead
        //   reserve CUs for 1 BPF_L instruction and 1 Compute Budget instruction,
        // Compute budget: allocate reuested CUs,
        // VM Execution: consume CUs for 1 BPF_L and 1 System (CPI-ed 1 time) and 1 Compute Budget, then succeed
        // Result: adjustment = (BPF_L + CB) - (BPF_L + System + CB) = -150
        //   Note negative adjustment means adjust-up
        (
            false,
            TestResult {
                cost_adjustment: -(solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS
                    as i64),
                execution_status: Ok(()),
            },
        ),
    ] {
        let mut test_setup = TestSetup::new();
        let ixs = [
            test_setup.deploy_with_max_data_len_ix(),
            test_setup.set_cu_limit_ix(cu_limit),
        ];
        assert_eq!(
            expected,
            test_setup.execute_test_transaction(&ixs, is_simd_170_enabled)
        );
    }
}

#[test]
fn test_builtin_ix_set_cu_price_only() {
    let mut test_setup = TestSetup::new();
    let mut cu_price = 1;

    // single Compute Budget instruction to set CU Price
    for (is_simd_170_enabled, expected) in [
        // post #3799:
        // Cost model & Compute budget: reserve/allocate default CU for one builtin ix
        // VM Execution: consume CUs for 1 Compute Budget, then succeed
        // Result: adjustment = 3_000 - 150 = 2_850
        (
            true,
            TestResult {
                cost_adjustment: MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT as i64
                    - solana_compute_budget_program::DEFAULT_COMPUTE_UNITS as i64,
                execution_status: Ok(()),
            },
        ),
        // pre #3799:
        // Cost model: reserved CU for 1 compute budget instruction,
        // Compute budget: allocate zero CU because there is zero non-compute-budget instructions,
        // VM Execution: no budget to execute, fail.
        // Result: adjustment = 150 - 0 = 150
        (
            false,
            TestResult {
                cost_adjustment: solana_compute_budget_program::DEFAULT_COMPUTE_UNITS as i64,
                execution_status: Err(TransactionError::InstructionError(
                    0,
                    InstructionError::ComputationalBudgetExceeded,
                )),
            },
        ),
    ] {
        assert_eq!(
            expected,
            test_setup.execute_test_transaction(
                &[test_setup.set_cu_price_ix(cu_price)],
                is_simd_170_enabled
            )
        );
        cu_price += 1;
    }
}

#[test]
fn test_builtin_ix_precompiled() {
    let mut test_setup = TestSetup::new();

    // single precompiled instruction
    for (is_simd_170_enabled, expected) in [
        // post #3799:
        // Cost model & Compute budget: reserve/allocate default CU for one builtin ix
        // VM Execution: consume 0 from CU-meter
        // Result: adjustment = 3_000
        (
            true,
            TestResult {
                cost_adjustment: MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT as i64,
                execution_status: Ok(()),
            },
        ),
        // pre #3799:
        // Cost model: reserved 0 CU because precompiles are builtin with native_cost set to `0`.
        // Compute budget: allocate zero CU because there is no non-compute-budget instructions,
        // VM Execution: nothing to execute;
        // Result: no adjustment
        (
            false,
            TestResult {
                cost_adjustment: 0,
                execution_status: Ok(()),
            },
        ),
    ] {
        assert_eq!(
            expected,
            test_setup.execute_test_transaction(
                &[Instruction::new_with_bincode(
                    solana_sdk::secp256k1_program::id(),
                    &[0u8],
                    // Add a dummy account to generate a unique transaction
                    vec![AccountMeta::new_readonly(Pubkey::new_unique(), false)]
                )],
                is_simd_170_enabled
            )
        );
    }
}
