//! 'cost_model` provides service to estimate a transaction's cost
//! following proposed fee schedule #16984; Relevant cluster cost
//! measuring is described by #19627
//!
//! The main function is `calculate_cost` which returns &TransactionCost.
//!

use {
    crate::{block_cost_limits::*, transaction_cost::*},
    agave_feature_set::{self as feature_set, FeatureSet},
    solana_bincode::limited_deserialize,
    solana_compute_budget::compute_budget_limits::DEFAULT_HEAP_COST,
    solana_fee_structure::FeeStructure,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::transaction_meta::StaticMeta,
    solana_sdk_ids::system_program,
    solana_svm_transaction::{instruction::SVMInstruction, svm_message::SVMStaticMessage},
    solana_system_interface::{
        instruction::SystemInstruction, MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION,
        MAX_PERMITTED_DATA_LENGTH,
    },
    std::num::Saturating,
};

pub struct CostModel;

#[derive(Debug, PartialEq)]
enum SystemProgramAccountAllocation {
    None,
    Some(u64),
    Failed,
}

impl CostModel {
    pub fn calculate_cost<'a, Tx: StaticMeta + SVMStaticMessage>(
        transaction: &'a Tx,
        feature_set: &FeatureSet,
    ) -> TransactionCost<'a, Tx> {
        if transaction.is_simple_vote_transaction() {
            TransactionCost::SimpleVote { transaction }
        } else {
            let (programs_execution_cost, loaded_accounts_data_size_cost) =
                Self::get_estimated_execution_cost(transaction, feature_set);
            let data_bytes_cost = Self::get_instructions_data_cost(transaction);
            Self::calculate_non_vote_transaction_cost(
                transaction,
                transaction.program_instructions_iter(),
                transaction.num_write_locks(),
                programs_execution_cost,
                loaded_accounts_data_size_cost,
                data_bytes_cost,
                feature_set,
            )
        }
    }

    // Calculate executed transaction CU cost, with actual execution and loaded accounts size
    // costs.
    pub fn calculate_cost_for_executed_transaction<'a, Tx: StaticMeta + SVMStaticMessage>(
        transaction: &'a Tx,
        actual_programs_execution_cost: u64,
        actual_loaded_accounts_data_size_bytes: u32,
        feature_set: &FeatureSet,
    ) -> TransactionCost<'a, Tx> {
        if transaction.is_simple_vote_transaction() {
            TransactionCost::SimpleVote { transaction }
        } else {
            let loaded_accounts_data_size_cost = Self::calculate_loaded_accounts_data_size_cost(
                actual_loaded_accounts_data_size_bytes,
                feature_set,
            );
            let instructions_data_cost = Self::get_instructions_data_cost(transaction);

            Self::calculate_non_vote_transaction_cost(
                transaction,
                transaction.program_instructions_iter(),
                transaction.num_write_locks(),
                actual_programs_execution_cost,
                loaded_accounts_data_size_cost,
                instructions_data_cost,
                feature_set,
            )
        }
    }

    /// Return an estimated total cost for a transaction given its':
    /// - `meta` - transaction meta
    /// - `instructions` - transaction instructions
    /// - `num_write_locks` - number of requested write locks
    pub fn estimate_cost<'a, Tx: StaticMeta>(
        transaction: &'a Tx,
        instructions: impl Iterator<Item = (&'a Pubkey, SVMInstruction<'a>)>,
        num_write_locks: u64,
        feature_set: &FeatureSet,
    ) -> TransactionCost<'a, Tx> {
        if transaction.is_simple_vote_transaction() {
            return TransactionCost::SimpleVote { transaction };
        }
        let (programs_execution_cost, loaded_accounts_data_size_cost) =
            Self::get_estimated_execution_cost(transaction, feature_set);
        let data_bytes_cost = Self::get_instructions_data_cost(transaction);
        Self::calculate_non_vote_transaction_cost(
            transaction,
            instructions,
            num_write_locks,
            programs_execution_cost,
            loaded_accounts_data_size_cost,
            data_bytes_cost,
            feature_set,
        )
    }

    fn calculate_non_vote_transaction_cost<'a, Tx: StaticMeta>(
        transaction: &'a Tx,
        instructions: impl Iterator<Item = (&'a Pubkey, SVMInstruction<'a>)>,
        num_write_locks: u64,
        programs_execution_cost: u64,
        loaded_accounts_data_size_cost: u64,
        data_bytes_cost: u16,
        feature_set: &FeatureSet,
    ) -> TransactionCost<'a, Tx> {
        let signature_cost = Self::get_signature_cost(transaction, feature_set);
        let write_lock_cost = Self::get_write_lock_cost(num_write_locks);

        let allocated_accounts_data_size =
            Self::calculate_allocated_accounts_data_size(instructions);

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

    /// Returns signature details and the total signature cost
    fn get_signature_cost(transaction: &impl StaticMeta, feature_set: &FeatureSet) -> u64 {
        let signatures_count_detail = transaction.signature_details();

        let ed25519_verify_cost =
            if feature_set.is_active(&feature_set::ed25519_precompile_verify_strict::id()) {
                ED25519_VERIFY_STRICT_COST
            } else {
                ED25519_VERIFY_COST
            };

        let secp256r1_verify_cost =
            if feature_set.is_active(&feature_set::enable_secp256r1_precompile::id()) {
                SECP256R1_VERIFY_COST
            } else {
                0
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
            .saturating_add(
                signatures_count_detail
                    .num_secp256r1_instruction_signatures()
                    .saturating_mul(secp256r1_verify_cost),
            )
    }

    /// Returns the total write-lock cost.
    fn get_write_lock_cost(num_write_locks: u64) -> u64 {
        WRITE_LOCK_UNITS.saturating_mul(num_write_locks)
    }

    /// Return (programs_execution_cost, loaded_accounts_data_size_cost)
    fn get_estimated_execution_cost(
        transaction: &impl StaticMeta,
        feature_set: &FeatureSet,
    ) -> (u64, u64) {
        // if failed to process compute_budget instructions, the transaction will not be executed
        // by `bank`, therefore it should be considered as no execution cost by cost model.
        let (programs_execution_costs, loaded_accounts_data_size_cost) = match transaction
            .compute_budget_instruction_details()
            .sanitize_and_convert_to_compute_budget_limits(feature_set)
        {
            Ok(compute_budget_limits) => (
                u64::from(compute_budget_limits.compute_unit_limit),
                Self::calculate_loaded_accounts_data_size_cost(
                    compute_budget_limits.loaded_accounts_bytes.get(),
                    feature_set,
                ),
            ),
            Err(_) => (0, 0),
        };

        (programs_execution_costs, loaded_accounts_data_size_cost)
    }

    /// Return the instruction data bytes cost.
    fn get_instructions_data_cost(transaction: &impl StaticMeta) -> u16 {
        transaction.instruction_data_len() / (INSTRUCTION_DATA_BYTES_COST as u16)
    }

    pub fn calculate_loaded_accounts_data_size_cost(
        loaded_accounts_data_size: u32,
        _feature_set: &FeatureSet,
    ) -> u64 {
        FeeStructure::calculate_memory_usage_cost(loaded_accounts_data_size, DEFAULT_HEAP_COST)
    }

    fn calculate_account_data_size_on_deserialized_system_instruction(
        instruction: SystemInstruction,
    ) -> SystemProgramAccountAllocation {
        match instruction {
            SystemInstruction::CreateAccount { space, .. }
            | SystemInstruction::CreateAccountWithSeed { space, .. }
            | SystemInstruction::Allocate { space }
            | SystemInstruction::AllocateWithSeed { space, .. } => {
                if space > MAX_PERMITTED_DATA_LENGTH {
                    SystemProgramAccountAllocation::Failed
                } else {
                    SystemProgramAccountAllocation::Some(space)
                }
            }
            _ => SystemProgramAccountAllocation::None,
        }
    }

    fn calculate_account_data_size_on_instruction(
        program_id: &Pubkey,
        instruction: SVMInstruction,
    ) -> SystemProgramAccountAllocation {
        if program_id == &system_program::id() {
            if let Ok(instruction) =
                limited_deserialize(instruction.data, solana_packet::PACKET_DATA_SIZE as u64)
            {
                Self::calculate_account_data_size_on_deserialized_system_instruction(instruction)
            } else {
                SystemProgramAccountAllocation::Failed
            }
        } else {
            SystemProgramAccountAllocation::None
        }
    }

    /// eventually, potentially determine account data size of all writable accounts
    /// at the moment, calculate account data size of account creation
    fn calculate_allocated_accounts_data_size<'a>(
        instructions: impl Iterator<Item = (&'a Pubkey, SVMInstruction<'a>)>,
    ) -> u64 {
        let mut tx_attempted_allocation_size = Saturating(0u64);
        for (program_id, instruction) in instructions {
            match Self::calculate_account_data_size_on_instruction(program_id, instruction) {
                SystemProgramAccountAllocation::Failed => {
                    // If any system program instructions can be statically
                    // determined to fail, no allocations will actually be
                    // persisted by the transaction. So return 0 here so that no
                    // account allocation budget is used for this failed
                    // transaction.
                    return 0;
                }
                SystemProgramAccountAllocation::None => continue,
                SystemProgramAccountAllocation::Some(ix_attempted_allocation_size) => {
                    tx_attempted_allocation_size += ix_attempted_allocation_size;
                }
            }
        }

        // The runtime prevents transactions from allocating too much account
        // data so clamp the attempted allocation size to the max amount.
        //
        // Note that if there are any custom bpf instructions in the transaction
        // it's tricky to know whether a newly allocated account will be freed
        // or not during an intermediate instruction in the transaction so we
        // shouldn't assume that a large sum of allocations will necessarily
        // lead to transaction failure.
        (MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION as u64)
            .min(tx_attempted_allocation_size.0)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        itertools::Itertools,
        solana_compute_budget::{
            self,
            compute_budget_limits::{
                DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT, MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT,
            },
        },
        solana_compute_budget_interface::ComputeBudgetInstruction,
        solana_fee_structure::ACCOUNT_DATA_COST_PAGE_SIZE,
        solana_hash::Hash,
        solana_instruction::Instruction,
        solana_keypair::Keypair,
        solana_message::{compiled_instruction::CompiledInstruction, Message},
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_sdk_ids::{compute_budget, system_program},
        solana_signer::Signer,
        solana_svm_transaction::svm_message::SVMStaticMessage,
        solana_system_interface::instruction::{self as system_instruction},
        solana_system_transaction as system_transaction,
        solana_transaction::Transaction,
    };

    fn test_setup() -> (Keypair, Hash) {
        agave_logger::setup();
        (Keypair::new(), Hash::new_unique())
    }

    #[test]
    fn test_calculate_allocated_accounts_data_size_no_allocation() {
        let transaction = Transaction::new_unsigned(Message::new(
            &[system_instruction::transfer(
                &Pubkey::new_unique(),
                &Pubkey::new_unique(),
                1,
            )],
            Some(&Pubkey::new_unique()),
        ));
        let sanitized_tx = RuntimeTransaction::from_transaction_for_tests(transaction);

        assert_eq!(
            CostModel::calculate_allocated_accounts_data_size(
                sanitized_tx.program_instructions_iter()
            ),
            0
        );
    }

    #[test]
    fn test_calculate_allocated_accounts_data_size_multiple_allocations() {
        let space1 = 100;
        let space2 = 200;
        let transaction = Transaction::new_unsigned(Message::new(
            &[
                system_instruction::create_account(
                    &Pubkey::new_unique(),
                    &Pubkey::new_unique(),
                    1,
                    space1,
                    &Pubkey::new_unique(),
                ),
                system_instruction::allocate(&Pubkey::new_unique(), space2),
            ],
            Some(&Pubkey::new_unique()),
        ));
        let sanitized_tx = RuntimeTransaction::from_transaction_for_tests(transaction);

        assert_eq!(
            CostModel::calculate_allocated_accounts_data_size(
                sanitized_tx.program_instructions_iter()
            ),
            space1 + space2
        );
    }

    #[test]
    fn test_calculate_allocated_accounts_data_size_max_limit() {
        let spaces = [MAX_PERMITTED_DATA_LENGTH, MAX_PERMITTED_DATA_LENGTH, 100];
        assert!(
            spaces.iter().copied().sum::<u64>()
                > MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION as u64
        );
        let transaction = Transaction::new_unsigned(Message::new(
            &[
                system_instruction::create_account(
                    &Pubkey::new_unique(),
                    &Pubkey::new_unique(),
                    1,
                    spaces[0],
                    &Pubkey::new_unique(),
                ),
                system_instruction::create_account(
                    &Pubkey::new_unique(),
                    &Pubkey::new_unique(),
                    1,
                    spaces[1],
                    &Pubkey::new_unique(),
                ),
                system_instruction::create_account(
                    &Pubkey::new_unique(),
                    &Pubkey::new_unique(),
                    1,
                    spaces[2],
                    &Pubkey::new_unique(),
                ),
            ],
            Some(&Pubkey::new_unique()),
        ));
        let sanitized_tx = RuntimeTransaction::from_transaction_for_tests(transaction);

        assert_eq!(
            CostModel::calculate_allocated_accounts_data_size(
                sanitized_tx.program_instructions_iter()
            ),
            MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION as u64,
        );
    }

    #[test]
    fn test_calculate_allocated_accounts_data_size_overflow() {
        let transaction = Transaction::new_unsigned(Message::new(
            &[
                system_instruction::create_account(
                    &Pubkey::new_unique(),
                    &Pubkey::new_unique(),
                    1,
                    100,
                    &Pubkey::new_unique(),
                ),
                system_instruction::allocate(&Pubkey::new_unique(), u64::MAX),
            ],
            Some(&Pubkey::new_unique()),
        ));
        let sanitized_tx = RuntimeTransaction::from_transaction_for_tests(transaction);

        assert_eq!(
            0, // SystemProgramAccountAllocation::Failed,
            CostModel::calculate_allocated_accounts_data_size(
                sanitized_tx.program_instructions_iter()
            ),
        );
    }

    #[test]
    fn test_calculate_allocated_accounts_data_size_invalid_ix() {
        let transaction = Transaction::new_unsigned(Message::new(
            &[
                system_instruction::allocate(&Pubkey::new_unique(), 100),
                Instruction::new_with_bincode(system_program::id(), &(), vec![]),
            ],
            Some(&Pubkey::new_unique()),
        ));
        let sanitized_tx = RuntimeTransaction::from_transaction_for_tests(transaction);

        assert_eq!(
            0, // SystemProgramAccountAllocation::Failed,
            CostModel::calculate_allocated_accounts_data_size(
                sanitized_tx.program_instructions_iter()
            ),
        );
    }

    #[test]
    fn test_cost_model_data_len_cost() {
        let lamports = 0;
        let owner = Pubkey::default();
        let seed = String::default();
        let space = 100;
        let base = Pubkey::default();
        for instruction in [
            SystemInstruction::CreateAccount {
                lamports,
                space,
                owner,
            },
            SystemInstruction::CreateAccountWithSeed {
                base,
                seed: seed.clone(),
                lamports,
                space,
                owner,
            },
            SystemInstruction::Allocate { space },
            SystemInstruction::AllocateWithSeed {
                base,
                seed,
                space,
                owner,
            },
        ] {
            assert_eq!(
                SystemProgramAccountAllocation::Some(space),
                CostModel::calculate_account_data_size_on_deserialized_system_instruction(
                    instruction
                )
            );
        }
        assert_eq!(
            SystemProgramAccountAllocation::None,
            CostModel::calculate_account_data_size_on_deserialized_system_instruction(
                SystemInstruction::TransferWithSeed {
                    lamports,
                    from_seed: String::default(),
                    from_owner: Pubkey::default(),
                }
            )
        );
    }

    #[test]
    fn test_cost_model_simple_transaction() {
        let (mint_keypair, start_hash) = test_setup();

        let keypair = Keypair::new();
        let simple_transaction = RuntimeTransaction::from_transaction_for_tests(
            system_transaction::transfer(&mint_keypair, &keypair.pubkey(), 2, start_hash),
        );

        let feature_set = FeatureSet::default();
        let expected_execution_cost = u64::from(MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT);
        let (programs_execution_cost, _loaded_accounts_data_size_cost) =
            CostModel::get_estimated_execution_cost(&simple_transaction, &feature_set);

        assert_eq!(expected_execution_cost, programs_execution_cost);
    }

    #[test]
    fn test_cost_model_token_transaction() {
        let (mint_keypair, start_hash) = test_setup();

        let instructions = vec![CompiledInstruction::new(3, &(), vec![1, 2, 0])];
        let tx = Transaction::new_with_compiled_instructions(
            &[&mint_keypair],
            &[solana_pubkey::new_rand(), solana_pubkey::new_rand()],
            start_hash,
            vec![Pubkey::new_unique()],
            instructions,
        );
        let token_transaction = RuntimeTransaction::from_transaction_for_tests(tx);

        for (feature_set, expected_execution_cost) in [
            (
                FeatureSet::default(),
                DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
            ),
            (
                FeatureSet::all_enabled(),
                DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64,
            ),
        ] {
            let (programs_execution_cost, _loaded_accounts_data_size_cost) =
                CostModel::get_estimated_execution_cost(&token_transaction, &feature_set);
            let data_bytes_cost = CostModel::get_instructions_data_cost(&token_transaction);

            assert_eq!(expected_execution_cost, programs_execution_cost);
            assert_eq!(0, data_bytes_cost);
        }
    }

    #[test]
    fn test_cost_model_demoted_write_lock() {
        let (mint_keypair, start_hash) = test_setup();

        // Cannot write-lock the system program, it will be demoted when taking locks.
        // However, the cost should be calculated as if it were taken.
        let simple_transaction = RuntimeTransaction::from_transaction_for_tests(
            system_transaction::transfer(&mint_keypair, &system_program::id(), 2, start_hash),
        );

        // write-lock counts towards cost, even if it is demoted.
        let tx_cost = CostModel::calculate_cost(&simple_transaction, &FeatureSet::default());
        assert_eq!(2 * WRITE_LOCK_UNITS, tx_cost.write_lock_cost());
        assert_eq!(1, tx_cost.writable_accounts().count());
    }

    #[test]
    fn test_cost_model_compute_budget_transaction() {
        let (mint_keypair, start_hash) = test_setup();
        let expected_cu_limit = 12_345;

        let instructions = vec![
            CompiledInstruction::new(3, &(), vec![1, 2, 0]),
            CompiledInstruction::new_from_raw_parts(
                4,
                ComputeBudgetInstruction::SetComputeUnitLimit(expected_cu_limit)
                    .pack()
                    .unwrap(),
                vec![],
            ),
        ];
        let tx = Transaction::new_with_compiled_instructions(
            &[&mint_keypair],
            &[solana_pubkey::new_rand(), solana_pubkey::new_rand()],
            start_hash,
            vec![Pubkey::new_unique(), compute_budget::id()],
            instructions,
        );
        let token_transaction = RuntimeTransaction::from_transaction_for_tests(tx);

        // If cu-limit is specified, that would the cost for all programs
        for (feature_set, expected_execution_cost) in [
            (FeatureSet::default(), expected_cu_limit as u64),
            (FeatureSet::all_enabled(), expected_cu_limit as u64),
        ] {
            let (programs_execution_cost, _loaded_accounts_data_size_cost) =
                CostModel::get_estimated_execution_cost(&token_transaction, &feature_set);
            let data_bytes_cost = CostModel::get_instructions_data_cost(&token_transaction);

            assert_eq!(expected_execution_cost, programs_execution_cost);
            assert_eq!(1, data_bytes_cost);
        }
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
            &[solana_pubkey::new_rand(), solana_pubkey::new_rand()],
            start_hash,
            vec![Pubkey::new_unique(), compute_budget::id()],
            instructions,
        );
        let token_transaction = RuntimeTransaction::from_transaction_for_tests(tx);

        for feature_set in [FeatureSet::default(), FeatureSet::all_enabled()] {
            let (programs_execution_cost, _loaded_accounts_data_size_cost) =
                CostModel::get_estimated_execution_cost(&token_transaction, &feature_set);
            assert_eq!(0, programs_execution_cost);
        }
    }

    #[test]
    fn test_cost_model_transaction_many_transfer_instructions() {
        let (mint_keypair, start_hash) = test_setup();

        let key1 = solana_pubkey::new_rand();
        let key2 = solana_pubkey::new_rand();
        let instructions =
            system_instruction::transfer_many(&mint_keypair.pubkey(), &[(key1, 1), (key2, 1)]);
        let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
        let tx = RuntimeTransaction::from_transaction_for_tests(Transaction::new(
            &[&mint_keypair],
            message,
            start_hash,
        ));

        // expected cost for two system transfer instructions
        let feature_set = FeatureSet::default();
        let expected_execution_cost = 2 * u64::from(MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT);
        let (programs_execution_cost, _loaded_accounts_data_size_cost) =
            CostModel::get_estimated_execution_cost(&tx, &feature_set);
        let data_bytes_cost = CostModel::get_instructions_data_cost(&tx);
        assert_eq!(expected_execution_cost, programs_execution_cost);
        assert_eq!(6, data_bytes_cost);
    }

    #[test]
    fn test_cost_model_message_many_different_instructions() {
        let (mint_keypair, start_hash) = test_setup();

        // construct a transaction with multiple random instructions
        let key1 = solana_pubkey::new_rand();
        let key2 = solana_pubkey::new_rand();
        let prog1 = solana_pubkey::new_rand();
        let prog2 = solana_pubkey::new_rand();
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

        for (feature_set, expected_cost) in [
            (
                FeatureSet::default(),
                DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64 * 2,
            ),
            (
                FeatureSet::all_enabled(),
                DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT as u64 * 2,
            ),
        ] {
            let (programs_execution_cost, _loaded_accounts_data_size_cost) =
                CostModel::get_estimated_execution_cost(&tx, &feature_set);
            let data_bytes_cost = CostModel::get_instructions_data_cost(&tx);
            assert_eq!(expected_cost, programs_execution_cost);
            assert_eq!(0, data_bytes_cost);
        }
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
        let feature_set = FeatureSet::default();
        let expected_execution_cost = u64::from(MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT);
        const DEFAULT_PAGE_COST: u64 = 8;
        let expected_loaded_accounts_data_size_cost =
            solana_compute_budget::compute_budget_limits::MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get()
                as u64
                / ACCOUNT_DATA_COST_PAGE_SIZE
                * DEFAULT_PAGE_COST;

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

        let expected_account_cost = WRITE_LOCK_UNITS * 2;
        let feature_set = FeatureSet::default();
        let expected_execution_cost = 2 * u64::from(MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT);
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
        let feature_set = FeatureSet::default();
        let expected_execution_cost = u64::from(MAX_BUILTIN_ALLOCATION_COMPUTE_UNIT_LIMIT)
            + u64::from(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT);
        let (programs_execution_cost, _loaded_accounts_data_size_cost) =
            CostModel::get_estimated_execution_cost(&transaction, &feature_set);

        assert_eq!(expected_execution_cost, programs_execution_cost);
    }

    #[test]
    fn test_transaction_cost_with_mix_instruction_with_cu_limit() {
        let (mint_keypair, start_hash) = test_setup();
        let cu_limit: u32 = 12_345;

        let transaction =
            RuntimeTransaction::from_transaction_for_tests(Transaction::new_signed_with_payer(
                &[
                    system_instruction::transfer(&mint_keypair.pubkey(), &Pubkey::new_unique(), 2),
                    ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                ],
                Some(&mint_keypair.pubkey()),
                &[&mint_keypair],
                start_hash,
            ));
        let feature_set = FeatureSet::default();
        let expected_execution_cost = cu_limit as u64;

        let (programs_execution_cost, _loaded_accounts_data_size_cost) =
            CostModel::get_estimated_execution_cost(&transaction, &feature_set);

        assert_eq!(expected_execution_cost, programs_execution_cost);
    }
}
