//! 'cost_model` provides service to estimate a transaction's cost
//! following proposed fee schedule #16984; Relevant cluster cost
//! measuring is described by #19627
//!
//! The main function is `calculate_cost` which returns &TransactionCost.
//!

use {
    crate::{block_cost_limits::*, transaction_cost::*},
    solana_compute_budget::compute_budget_limits::DEFAULT_HEAP_COST,
    solana_feature_set::{self as feature_set, FeatureSet},
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_sdk::{fee::FeeStructure, pubkey::Pubkey},
    solana_svm_transaction::svm_message::SVMMessage,
};

pub struct CostModel;

impl CostModel {
    pub fn calculate_cost<'a, Tx: TransactionWithMeta>(
        transaction: &'a Tx,
        feature_set: &FeatureSet,
    ) -> TransactionCost<'a, Tx> {
        if transaction.is_simple_vote_transaction() {
            TransactionCost::SimpleVote { transaction }
        } else {
            let signature_cost = Self::get_signature_cost(transaction, feature_set);
            let write_lock_cost = Self::get_write_lock_cost(transaction, feature_set);
            let (programs_execution_cost, loaded_accounts_data_size_cost, data_bytes_cost) =
                Self::get_transaction_cost(transaction, feature_set);
            let allocated_accounts_data_size =
                transaction.allocation_instruction_details().total_space;

            let usage_cost_details = UsageCostDetails {
                transaction,
                signature_cost,
                write_lock_cost,
                data_bytes_cost,
                programs_execution_cost,
                loaded_accounts_data_size_cost,
                allocated_accounts_data_size,
            };

            TransactionCost::Transaction(usage_cost_details)
        }
    }

    // Calculate executed transaction CU cost, with actual execution and loaded accounts size
    // costs.
    pub fn calculate_cost_for_executed_transaction<'a, Tx: TransactionWithMeta>(
        transaction: &'a Tx,
        actual_programs_execution_cost: u64,
        actual_loaded_accounts_data_size_bytes: u32,
        feature_set: &FeatureSet,
    ) -> TransactionCost<'a, Tx> {
        if transaction.is_simple_vote_transaction() {
            TransactionCost::SimpleVote { transaction }
        } else {
            let signature_cost = Self::get_signature_cost(transaction, feature_set);
            let write_lock_cost = Self::get_write_lock_cost(transaction, feature_set);

            let instructions_data_cost = Self::get_instructions_data_cost(transaction);
            let allocated_accounts_data_size =
                transaction.allocation_instruction_details().total_space;

            let programs_execution_cost = actual_programs_execution_cost;
            let loaded_accounts_data_size_cost = Self::calculate_loaded_accounts_data_size_cost(
                actual_loaded_accounts_data_size_bytes,
                feature_set,
            );

            let usage_cost_details = UsageCostDetails {
                transaction,
                signature_cost,
                write_lock_cost,
                data_bytes_cost: instructions_data_cost,
                programs_execution_cost,
                loaded_accounts_data_size_cost,
                allocated_accounts_data_size,
            };

            TransactionCost::Transaction(usage_cost_details)
        }
    }

    /// Returns signature details and the total signature cost
    fn get_signature_cost(transaction: &impl TransactionWithMeta, feature_set: &FeatureSet) -> u64 {
        let signatures_count_detail = transaction.signature_details();

        let ed25519_verify_cost =
            if feature_set.is_active(&feature_set::ed25519_precompile_verify_strict::id()) {
                ED25519_VERIFY_STRICT_COST
            } else {
                ED25519_VERIFY_COST
            };

        signatures_count_detail
            .num_transaction_signatures()
            .saturating_mul(SIGNATURE_COST)
            .saturating_add(
                signatures_count_detail
                    .num_secp256k1_instruction_signatures()
                    .saturating_mul(SECP256K1_VERIFY_COST),
            )
            .saturating_add(
                signatures_count_detail
                    .num_ed25519_instruction_signatures()
                    .saturating_mul(ed25519_verify_cost),
            )
    }

    fn get_writable_accounts(message: &impl SVMMessage) -> impl Iterator<Item = &Pubkey> {
        message
            .account_keys()
            .iter()
            .enumerate()
            .filter_map(|(i, k)| message.is_writable(i).then_some(k))
    }

    /// Returns the total write-lock cost.
    fn get_write_lock_cost(transaction: &impl SVMMessage, feature_set: &FeatureSet) -> u64 {
        let num_write_locks =
            if feature_set.is_active(&feature_set::cost_model_requested_write_lock_cost::id()) {
                transaction.num_write_locks()
            } else {
                Self::get_writable_accounts(transaction).count() as u64
            };
        WRITE_LOCK_UNITS.saturating_mul(num_write_locks)
    }

    /// Return (programs_execution_cost, loaded_accounts_data_size_cost, data_bytes_cost)
    fn get_transaction_cost(
        transaction: &impl TransactionWithMeta,
        feature_set: &FeatureSet,
    ) -> (u64, u64, u64) {
        let builtin_instruction_details = transaction.builtin_instruction_details();

        // if failed to process compute budget instructions, the transaction
        // will not be executed by `bank`, therefore it should be considered
        // as no execution cost by cost model.
        let (programs_execution_costs, loaded_accounts_data_size_cost) = match transaction
            .compute_budget_limits(feature_set)
        {
            Ok(compute_budget_limits) => {
                // if tx contained user-space instructions and a more accurate
                // estimate available correct it, where
                // "user-space instructions" must be specifically checked by
                // 'compute_unit_limit_is_set' flag, because compute_budget
                // does not distinguish builtin and bpf instructions when
                // calculating default compute-unit-limit.
                //
                // (see compute_budget.rs test
                // `test_process_mixed_instructions_without_compute_budget`)
                let programs_execution_costs = if transaction.is_compute_budget_limit_set()
                    && builtin_instruction_details.has_user_space_instructions
                {
                    u64::from(compute_budget_limits.compute_unit_limit)
                } else {
                    builtin_instruction_details.instruction_execution_costs
                };
                let loaded_accounts_data_size_cost = Self::calculate_loaded_accounts_data_size_cost(
                    compute_budget_limits.loaded_accounts_bytes.get(),
                    feature_set,
                );

                (programs_execution_costs, loaded_accounts_data_size_cost)
            }
            Err(_) => (0, 0),
        };

        (
            programs_execution_costs,
            loaded_accounts_data_size_cost,
            builtin_instruction_details.data_bytes_len_total / INSTRUCTION_DATA_BYTES_COST,
        )
    }

    /// Return the instruction data bytes cost.
    fn get_instructions_data_cost(transaction: &impl SVMMessage) -> u64 {
        let ix_data_bytes_len_total: u64 = transaction
            .instructions_iter()
            .map(|instruction| instruction.data.len() as u64)
            .sum();

        ix_data_bytes_len_total / INSTRUCTION_DATA_BYTES_COST
    }

    pub fn calculate_loaded_accounts_data_size_cost(
        loaded_accounts_data_size: u32,
        _feature_set: &FeatureSet,
    ) -> u64 {
        FeeStructure::calculate_memory_usage_cost(loaded_accounts_data_size, DEFAULT_HEAP_COST)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        itertools::Itertools,
        log::debug,
        solana_builtins_default_costs::BUILTIN_INSTRUCTION_COSTS,
        solana_compute_budget::compute_budget_limits::DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_sdk::{
            compute_budget::{self, ComputeBudgetInstruction},
            fee::ACCOUNT_DATA_COST_PAGE_SIZE,
            hash::Hash,
            instruction::{CompiledInstruction, Instruction},
            message::Message,
            signature::{Keypair, Signer},
            system_instruction::{self},
            system_program, system_transaction,
            transaction::Transaction,
        },
    };

    fn test_setup() -> (Keypair, Hash) {
        solana_logger::setup();
        (Keypair::new(), Hash::new_unique())
    }

    #[test]
    fn test_cost_model_simple_transaction() {
        let (mint_keypair, start_hash) = test_setup();

        let keypair = Keypair::new();
        let simple_transaction = RuntimeTransaction::from_transaction_for_tests(
            system_transaction::transfer(&mint_keypair, &keypair.pubkey(), 2, start_hash),
        );
        debug!(
            "system_transaction simple_transaction {:?}",
            simple_transaction
        );

        // expected cost for one system transfer instructions
        let expected_execution_cost = BUILTIN_INSTRUCTION_COSTS
            .get(&system_program::id())
            .unwrap();

        let (program_execution_cost, _loaded_accounts_data_size_cost, data_bytes_cost) =
            CostModel::get_transaction_cost(&simple_transaction, &FeatureSet::all_enabled());

        assert_eq!(*expected_execution_cost, program_execution_cost);
        assert_eq!(3, data_bytes_cost);
    }

    #[test]
    fn test_cost_model_token_transaction() {
        let (mint_keypair, start_hash) = test_setup();

        let instructions = vec![CompiledInstruction::new(3, &(), vec![1, 2, 0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&mint_keypair],
            &[
                solana_sdk::pubkey::new_rand(),
                solana_sdk::pubkey::new_rand(),
            ],
            start_hash,
            vec![Pubkey::new_unique()],
            instructions,
        );
        let token_transaction = RuntimeTransaction::from_transaction_for_tests(tx);
        debug!("token_transaction {:?}", token_transaction);

        let (program_execution_cost, _loaded_accounts_data_size_cost, data_bytes_cost) =
            CostModel::get_transaction_cost(&token_transaction, &FeatureSet::all_enabled());
        assert_eq!(
            DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
            program_execution_cost
        );
        assert_eq!(0, data_bytes_cost);
    }

    #[test]
    fn test_cost_model_demoted_write_lock() {
        let (mint_keypair, start_hash) = test_setup();

        // Cannot write-lock the system program, it will be demoted when taking locks.
        // However, the cost should be calculated as if it were taken.
        let simple_transaction = RuntimeTransaction::from_transaction_for_tests(
            system_transaction::transfer(&mint_keypair, &system_program::id(), 2, start_hash),
        );

        // Feature not enabled - write lock is demoted and does not count towards cost
        {
            let tx_cost = CostModel::calculate_cost(&simple_transaction, &FeatureSet::default());
            assert_eq!(WRITE_LOCK_UNITS, tx_cost.write_lock_cost());
            assert_eq!(1, tx_cost.writable_accounts().count());
        }

        // Feature enabled - write lock is demoted but still counts towards cost
        {
            let tx_cost =
                CostModel::calculate_cost(&simple_transaction, &FeatureSet::all_enabled());
            assert_eq!(2 * WRITE_LOCK_UNITS, tx_cost.write_lock_cost());
            assert_eq!(1, tx_cost.writable_accounts().count());
        }
    }

    #[test]
    fn test_cost_model_compute_budget_transaction() {
        let (mint_keypair, start_hash) = test_setup();

        let instructions = vec![
            CompiledInstruction::new(3, &(), vec![1, 2, 0]),
            CompiledInstruction::new_from_raw_parts(
                4,
                ComputeBudgetInstruction::SetComputeUnitLimit(12_345)
                    .pack()
                    .unwrap(),
                vec![],
            ),
        ];
        let tx = Transaction::new_with_compiled_instructions(
            &[&mint_keypair],
            &[
                solana_sdk::pubkey::new_rand(),
                solana_sdk::pubkey::new_rand(),
            ],
            start_hash,
            vec![Pubkey::new_unique(), compute_budget::id()],
            instructions,
        );
        let token_transaction = RuntimeTransaction::from_transaction_for_tests(tx);

        let (program_execution_cost, _loaded_accounts_data_size_cost, data_bytes_cost) =
            CostModel::get_transaction_cost(&token_transaction, &FeatureSet::all_enabled());

        // If cu-limit is specified, that would the cost for all programs
        assert_eq!(12_345, program_execution_cost);
        assert_eq!(1, data_bytes_cost);
    }

    #[test]
    fn test_cost_model_with_failed_compute_budget_transaction() {
        let (mint_keypair, start_hash) = test_setup();

        let instructions = vec![
            CompiledInstruction::new(3, &(), vec![1, 2, 0]),
            CompiledInstruction::new_from_raw_parts(
                4,
                ComputeBudgetInstruction::SetComputeUnitLimit(12_345)
                    .pack()
                    .unwrap(),
                vec![],
            ),
            // to trigger failure in `sanitize_and_convert_to_compute_budget_limits`
            CompiledInstruction::new_from_raw_parts(
                4,
                ComputeBudgetInstruction::SetLoadedAccountsDataSizeLimit(0)
                    .pack()
                    .unwrap(),
                vec![],
            ),
        ];
        let tx = Transaction::new_with_compiled_instructions(
            &[&mint_keypair],
            &[
                solana_sdk::pubkey::new_rand(),
                solana_sdk::pubkey::new_rand(),
            ],
            start_hash,
            vec![Pubkey::new_unique(), compute_budget::id()],
            instructions,
        );
        let token_transaction = RuntimeTransaction::from_transaction_for_tests(tx);

        let (program_execution_cost, _loaded_accounts_data_size_cost, _data_bytes_cost) =
            CostModel::get_transaction_cost(&token_transaction, &FeatureSet::all_enabled());
        assert_eq!(0, program_execution_cost);
    }

    #[test]
    fn test_cost_model_transaction_many_transfer_instructions() {
        let (mint_keypair, start_hash) = test_setup();

        let key1 = solana_sdk::pubkey::new_rand();
        let key2 = solana_sdk::pubkey::new_rand();
        let instructions =
            system_instruction::transfer_many(&mint_keypair.pubkey(), &[(key1, 1), (key2, 1)]);
        let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
        let tx = RuntimeTransaction::from_transaction_for_tests(Transaction::new(
            &[&mint_keypair],
            message,
            start_hash,
        ));
        debug!("many transfer transaction {:?}", tx);

        // expected cost for two system transfer instructions
        let program_cost = BUILTIN_INSTRUCTION_COSTS
            .get(&system_program::id())
            .unwrap();
        let expected_cost = program_cost * 2;

        let (program_execution_cost, _loaded_accounts_data_size_cost, data_bytes_cost) =
            CostModel::get_transaction_cost(&tx, &FeatureSet::all_enabled());
        assert_eq!(expected_cost, program_execution_cost);
        assert_eq!(6, data_bytes_cost);
    }

    #[test]
    fn test_cost_model_message_many_different_instructions() {
        let (mint_keypair, start_hash) = test_setup();

        // construct a transaction with multiple random instructions
        let key1 = solana_sdk::pubkey::new_rand();
        let key2 = solana_sdk::pubkey::new_rand();
        let prog1 = solana_sdk::pubkey::new_rand();
        let prog2 = solana_sdk::pubkey::new_rand();
        let instructions = vec![
            CompiledInstruction::new(3, &(), vec![0, 1]),
            CompiledInstruction::new(4, &(), vec![0, 2]),
        ];
        let tx = RuntimeTransaction::from_transaction_for_tests(
            Transaction::new_with_compiled_instructions(
                &[&mint_keypair],
                &[key1, key2],
                start_hash,
                vec![prog1, prog2],
                instructions,
            ),
        );
        debug!("many random transaction {:?}", tx);

        let expected_cost = DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64 * 2;
        let (program_execution_cost, _loaded_accounts_data_size_cost, data_bytes_cost) =
            CostModel::get_transaction_cost(&tx, &FeatureSet::all_enabled());
        assert_eq!(expected_cost, program_execution_cost);
        assert_eq!(0, data_bytes_cost);
    }

    #[test]
    fn test_cost_model_sort_message_accounts_by_type() {
        // construct a transaction with two random instructions with same signer
        let signer1 = Keypair::new();
        let signer2 = Keypair::new();
        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();
        let prog1 = Pubkey::new_unique();
        let prog2 = Pubkey::new_unique();
        let instructions = vec![
            CompiledInstruction::new(4, &(), vec![0, 2]),
            CompiledInstruction::new(5, &(), vec![1, 3]),
        ];
        let tx = RuntimeTransaction::from_transaction_for_tests(
            Transaction::new_with_compiled_instructions(
                &[&signer1, &signer2],
                &[key1, key2],
                Hash::new_unique(),
                vec![prog1, prog2],
                instructions,
            ),
        );

        let tx_cost = CostModel::calculate_cost(&tx, &FeatureSet::all_enabled());
        let writable_accounts = tx_cost.writable_accounts().collect_vec();
        assert_eq!(2 + 2, writable_accounts.len());
        assert_eq!(signer1.pubkey(), *writable_accounts[0]);
        assert_eq!(signer2.pubkey(), *writable_accounts[1]);
        assert_eq!(key1, *writable_accounts[2]);
        assert_eq!(key2, *writable_accounts[3]);
    }

    #[test]
    fn test_cost_model_calculate_cost_all_default() {
        let (mint_keypair, start_hash) = test_setup();
        let tx = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &Keypair::new().pubkey(),
            2,
            start_hash,
        ));

        let expected_account_cost = WRITE_LOCK_UNITS * 2;
        let expected_execution_cost = BUILTIN_INSTRUCTION_COSTS
            .get(&system_program::id())
            .unwrap();
        const DEFAULT_PAGE_COST: u64 = 8;
        let expected_loaded_accounts_data_size_cost =
            solana_compute_budget::compute_budget_limits::MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get()
                as u64
                / ACCOUNT_DATA_COST_PAGE_SIZE
                * DEFAULT_PAGE_COST;

        let tx_cost = CostModel::calculate_cost(&tx, &FeatureSet::all_enabled());
        assert_eq!(expected_account_cost, tx_cost.write_lock_cost());
        assert_eq!(*expected_execution_cost, tx_cost.programs_execution_cost());
        assert_eq!(2, tx_cost.writable_accounts().count());
        assert_eq!(
            expected_loaded_accounts_data_size_cost,
            tx_cost.loaded_accounts_data_size_cost()
        );
    }

    #[test]
    fn test_cost_model_calculate_cost_with_limit() {
        let (mint_keypair, start_hash) = test_setup();
        let to_keypair = Keypair::new();
        let data_limit = 32 * 1024u32;
        let tx =
            RuntimeTransaction::from_transaction_for_tests(Transaction::new_signed_with_payer(
                &[
                    system_instruction::transfer(&mint_keypair.pubkey(), &to_keypair.pubkey(), 2),
                    ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(data_limit),
                ],
                Some(&mint_keypair.pubkey()),
                &[&mint_keypair],
                start_hash,
            ));

        let feature_set = FeatureSet::all_enabled();
        let expected_account_cost = WRITE_LOCK_UNITS * 2;
        let expected_execution_cost = BUILTIN_INSTRUCTION_COSTS
            .get(&system_program::id())
            .unwrap()
            + BUILTIN_INSTRUCTION_COSTS
                .get(&compute_budget::id())
                .unwrap();
        let expected_loaded_accounts_data_size_cost = (data_limit as u64) / (32 * 1024) * 8;

        let tx_cost = CostModel::calculate_cost(&tx, &feature_set);
        assert_eq!(expected_account_cost, tx_cost.write_lock_cost());
        assert_eq!(expected_execution_cost, tx_cost.programs_execution_cost());
        assert_eq!(2, tx_cost.writable_accounts().count());
        assert_eq!(
            expected_loaded_accounts_data_size_cost,
            tx_cost.loaded_accounts_data_size_cost()
        );
    }

    #[test]
    fn test_transaction_cost_with_mix_instruction_without_compute_budget() {
        let (mint_keypair, start_hash) = test_setup();

        let transaction =
            RuntimeTransaction::from_transaction_for_tests(Transaction::new_signed_with_payer(
                &[
                    Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                    system_instruction::transfer(&mint_keypair.pubkey(), &Pubkey::new_unique(), 2),
                ],
                Some(&mint_keypair.pubkey()),
                &[&mint_keypair],
                start_hash,
            ));
        // transaction has one builtin instruction, and one bpf instruction, no ComputeBudget::compute_unit_limit
        let expected_builtin_cost = *BUILTIN_INSTRUCTION_COSTS
            .get(&solana_system_program::id())
            .unwrap();
        let expected_bpf_cost = DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT;

        let (program_execution_cost, _loaded_accounts_data_size_cost, _data_bytes_cost) =
            CostModel::get_transaction_cost(&transaction, &FeatureSet::all_enabled());

        assert_eq!(
            expected_builtin_cost + expected_bpf_cost as u64,
            program_execution_cost
        );
    }

    #[test]
    fn test_transaction_cost_with_mix_instruction_with_cu_limit() {
        let (mint_keypair, start_hash) = test_setup();

        let transaction =
            RuntimeTransaction::from_transaction_for_tests(Transaction::new_signed_with_payer(
                &[
                    system_instruction::transfer(&mint_keypair.pubkey(), &Pubkey::new_unique(), 2),
                    ComputeBudgetInstruction::set_compute_unit_limit(12_345),
                ],
                Some(&mint_keypair.pubkey()),
                &[&mint_keypair],
                start_hash,
            ));
        // transaction has one builtin instruction, and one ComputeBudget::compute_unit_limit
        let expected_cost = *BUILTIN_INSTRUCTION_COSTS
            .get(&solana_system_program::id())
            .unwrap()
            + BUILTIN_INSTRUCTION_COSTS
                .get(&compute_budget::id())
                .unwrap();

        let (program_execution_cost, _loaded_accounts_data_size_cost, _data_bytes_cost) =
            CostModel::get_transaction_cost(&transaction, &FeatureSet::all_enabled());
        assert_eq!(expected_cost, program_execution_cost);
    }
}
