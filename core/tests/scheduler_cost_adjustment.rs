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
        account::{Account, AccountSharedData},
        bpf_loader,
        clock::MAX_PROCESSING_AGE,
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
    bank: Bank,
    bank_forks: Arc<RwLock<BankForks>>,
    amount: u64,
}

impl TestSetup {
    fn new() -> Self {
        let (mut genesis_config, mint_keypair) = create_genesis_config(sol_to_lamports(1.));
        genesis_config.rent = Rent::default();
        let (bank, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let bank = Bank::new_from_parent(
            bank,
            &Pubkey::new_unique(),
            genesis_config.epoch_schedule.get_first_slot_in_epoch(1),
        );

        let amount = genesis_config.rent.minimum_balance(0);

        Self {
            genesis_config,
            mint_keypair,
            bank,
            bank_forks,
            amount,
        }
    }

    fn install_memo_program_account(&mut self) {
        self.bank.store_account(
            &spl_memo::id(),
            &AccountSharedData::from(Account {
                lamports: u64::MAX,
                // borrows memo elf for executing memo ix in order to set up test condition
                data: include_bytes!("../../program-test/src/programs/spl_memo-3.0.0.so").to_vec(),
                owner: bpf_loader::id(),
                executable: true,
                rent_epoch: 0,
            }),
        );
    }

    fn execute_test_transaction(
        &mut self,
        ixs: &[Instruction],
        is_simd_170_enabled: bool,
    ) -> TestResult {
        if is_simd_170_enabled {
            self.bank
                .activate_feature(&feature_set::reserve_minimal_cus_for_builtin_instructions::id());
        } else {
            self.bank.deactivate_feature(
                &feature_set::reserve_minimal_cus_for_builtin_instructions::id(),
            );
        }

        let tx = Transaction::new(
            &[&self.mint_keypair],
            Message::new(ixs, Some(&self.mint_keypair.pubkey())),
            self.genesis_config.hash(),
        );

        let estimated_execution_cost = CostModel::calculate_cost(
            &RuntimeTransaction::from_transaction_for_tests(tx.clone()),
            &self.bank.feature_set,
        )
        .programs_execution_cost();

        let batch = self.bank.prepare_batch_for_tests(vec![tx]);
        let commit_result = self
            .bank
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
            self.amount,
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

    fn create_lookup_table_ix(&self) -> Instruction {
        let (create_lookup_table_ix, _lookup_table_address) =
            solana_sdk::address_lookup_table::instruction::create_lookup_table(
                Pubkey::new_unique(),
                self.mint_keypair.pubkey(),
                0,
            );

        create_lookup_table_ix
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
fn test_builtin_ix_cost_adjustment_with_alt_no_cu_limit() {
    let mut test_setup = TestSetup::new();

    // A address-lookup-table ix only, that CPIs into System instructions
    for (is_simd_170_enabled, expected) in [
        // post #3799:
        // Cost model & Compute budget: reserve/allocate default CU for 1 builtin
        // VM Execution: consume CUs for 1 ATL and 3 System (CPI-ed 3 times), then succeed
        // Result: adjustment = 3_000 - 750 - 3 * 150 = 1,800
        (
            true,
            TestResult {
                cost_adjustment: MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT as i64
                    - solana_address_lookup_table_program::processor::DEFAULT_COMPUTE_UNITS as i64
                    - 3 * solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS as i64,
                execution_status: Ok(()),
            },
        ),
        // pre #3799:
        // Cost model: reserve 750 CU for ATL instruction,
        // Compute budget: allocate 200K CU for one non-compute-budget instruction
        // VM Execution: consumeed CU for 1 ATL and 3 System, then succeed,
        // Result: adjustment = 750 - (750 + 3 * 150) = -450
        //   Note negative adjustment means adjust-up
        (
            false,
            TestResult {
                cost_adjustment: -3
                    * solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS as i64,
                execution_status: Ok(()),
            },
        ),
    ] {
        assert_eq!(
            expected,
            test_setup.execute_test_transaction(
                &[test_setup.create_lookup_table_ix(),],
                is_simd_170_enabled
            )
        );
    }
}

#[test]
fn test_builtin_ix_cost_adjustment_with_alt_and_cu_limit_high() {
    let mut test_setup = TestSetup::new();
    let cu_limit = 500_000;
    let tx_execution_cost = solana_address_lookup_table_program::processor::DEFAULT_COMPUTE_UNITS
        + 3 * solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS
        + solana_compute_budget_program::DEFAULT_COMPUTE_UNITS;

    // A address-lookup-table ix only, that CPIs into System instructions; and a compute-budget
    // instruction requests enough CU Limit
    for (is_simd_170_enabled, expected) in [
        // post #3799:
        // Cost model & Compute budget: reserve/allocate requested CUs
        // VM Execution: consume CUs for 1 ATL and 3 System (CPI-ed 3 times) and 1 Compute Budget, then succeed
        // Result: adjustment = 500_000 - 750 - 3 * 150 -150 = 498_650
        (
            true,
            TestResult {
                cost_adjustment: cu_limit as i64 - tx_execution_cost as i64,
                execution_status: Ok(()),
            },
        ),
        // pre #3799:
        // Cost model: ignores requested CU limit because no bpf instruction, instead
        //   reserve CUs for 1 ATL instruction and 1 Compute Budget instruction,
        // Compute budget: allocate reuested CUs,
        // VM Execution: consume CUs for 1 ATL and 3 System (CPI-ed 3 times) and 1 Compute Budget, then succeed
        // Result: adjustment = (ATL + CB) - (ATL + 3 * System + CB) = -3 * 150 = -450
        //   Note negative adjustment means adjust-up
        (
            false,
            TestResult {
                cost_adjustment: -3
                    * solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS as i64,
                execution_status: Ok(()),
            },
        ),
    ] {
        assert_eq!(
            expected,
            test_setup.execute_test_transaction(
                &[
                    test_setup.create_lookup_table_ix(),
                    test_setup.set_cu_limit_ix(cu_limit),
                ],
                is_simd_170_enabled
            )
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
