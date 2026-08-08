use {
    crate::{
        block_cost_limits::MAX_BLOCK_ACCOUNTS_DATA_SIZE_DELTA, cost_tracker::CostTrackerError,
        transaction_cost::TransactionCost,
    },
    agave_feature_set::FeatureSet,
    solana_bincode::limited_deserialize,
    solana_pubkey::Pubkey,
    solana_runtime_transaction::transaction_with_meta::TransactionWithMeta,
    solana_sdk_ids::system_program,
    solana_svm_transaction::instruction::SVMInstruction,
    solana_system_interface::{
        MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION, MAX_PERMITTED_DATA_LENGTH,
        instruction::SystemInstruction,
    },
    std::num::Saturating,
};

pub(crate) const DEFAULT_LIMIT: u64 = MAX_BLOCK_ACCOUNTS_DATA_SIZE_DELTA;

#[derive(Debug, PartialEq)]
enum SystemProgramAccountAllocation {
    None,
    Some(u64),
    Failed,
}

pub(crate) fn calculate<'a>(
    instructions: impl Iterator<Item = (&'a Pubkey, SVMInstruction<'a>)>,
    feature_set: &FeatureSet,
) -> u64 {
    // NOTE when bank feature gate of track with actual accounts_data_size_delta lands
    // and activated, return `0`

    calculate_impl(instructions, feature_set)
}

pub(crate) fn would_fit(
    current_allocated_accounts_data_size: Saturating<u64>,
    tx_cost: &TransactionCost<impl TransactionWithMeta>,
    allocated_data_size_limit: u64,
) -> Result<(), CostTrackerError> {
    let allocated_accounts_data_size =
        current_allocated_accounts_data_size + Saturating(tx_cost.allocated_accounts_data_size());

    if allocated_accounts_data_size.0 > allocated_data_size_limit {
        return Err(CostTrackerError::WouldExceedAccountDataBlockLimit);
    }

    Ok(())
}

pub(crate) fn add(
    allocated_accounts_data_size: &mut Saturating<u64>,
    tx_cost: &TransactionCost<impl TransactionWithMeta>,
) {
    *allocated_accounts_data_size += tx_cost.allocated_accounts_data_size();
}

pub(crate) fn subtract(
    allocated_accounts_data_size: &mut Saturating<u64>,
    tx_cost: &TransactionCost<impl TransactionWithMeta>,
) {
    *allocated_accounts_data_size -= tx_cost.allocated_accounts_data_size();
}

pub(crate) fn stats_value(allocated_accounts_data_size: Saturating<u64>) -> u64 {
    allocated_accounts_data_size.0
}

fn calculate_on_deserialized_system_instruction(
    instruction: SystemInstruction,
    feature_set: &FeatureSet,
) -> SystemProgramAccountAllocation {
    let validate_space = |space: u64| {
        if space > MAX_PERMITTED_DATA_LENGTH {
            SystemProgramAccountAllocation::Failed
        } else {
            SystemProgramAccountAllocation::Some(space)
        }
    };

    match instruction {
        SystemInstruction::CreateAccount { space, .. }
        | SystemInstruction::CreateAccountWithSeed { space, .. }
        | SystemInstruction::Allocate { space }
        | SystemInstruction::AllocateWithSeed { space, .. } => validate_space(space),
        SystemInstruction::CreateAccountAllowPrefund { space, .. } => {
            if !feature_set.snapshot().create_account_allow_prefund {
                return SystemProgramAccountAllocation::Failed;
            }
            validate_space(space)
        }
        // DEVELOPER WARNING: New allocating instructions MUST return `Failed`
        // until activated by a feature gate
        SystemInstruction::Assign { .. }
        | SystemInstruction::Transfer { .. }
        | SystemInstruction::AdvanceNonceAccount
        | SystemInstruction::WithdrawNonceAccount(..)
        | SystemInstruction::InitializeNonceAccount(..)
        | SystemInstruction::AuthorizeNonceAccount(..)
        | SystemInstruction::UpgradeNonceAccount
        | SystemInstruction::AssignWithSeed { .. }
        | SystemInstruction::TransferWithSeed { .. } => SystemProgramAccountAllocation::None,
        // DEVELOPER WARNING: New non-allocating instructions MUST return `Failed`
        // until activated by a feature gate
    } // Do not add wildcard pattern (_)
}

fn calculate_on_instruction(
    program_id: &Pubkey,
    instruction: SVMInstruction,
    feature_set: &FeatureSet,
) -> SystemProgramAccountAllocation {
    if program_id == &system_program::id() {
        if let Ok(instruction) =
            limited_deserialize(instruction.data, solana_packet::PACKET_DATA_SIZE as u64)
        {
            calculate_on_deserialized_system_instruction(instruction, feature_set)
        } else {
            SystemProgramAccountAllocation::Failed
        }
    } else {
        SystemProgramAccountAllocation::None
    }
}

/// Eventually, `account_data_size_delta` should replace this static estimate.
/// For now, calculate account data size from top-level system account creation
/// instructions only.
fn calculate_impl<'a>(
    instructions: impl Iterator<Item = (&'a Pubkey, SVMInstruction<'a>)>,
    feature_set: &FeatureSet,
) -> u64 {
    let mut tx_attempted_allocation_size = Saturating(0u64);
    for (program_id, instruction) in instructions {
        match calculate_on_instruction(program_id, instruction, feature_set) {
            SystemProgramAccountAllocation::Failed => {
                // Runtime execution stops at the first failing
                // instruction. Keep allocations from earlier instructions
                // because they may have already executed before rollback,
                // but ignore any later instructions because they will not
                // be reached.
                break;
            }
            SystemProgramAccountAllocation::None => continue,
            SystemProgramAccountAllocation::Some(ix_attempted_allocation_size) => {
                tx_attempted_allocation_size += ix_attempted_allocation_size;
            }
        }
    }

    // The runtime prevents transactions from allocating too much account data
    // so clamp the attempted allocation size to the max amount.
    //
    // Note that if there are any custom bpf instructions in the transaction
    // it's tricky to know whether a newly allocated account will be freed
    // or not during an intermediate instruction in the transaction so we
    // shouldn't assume that a large sum of allocations will necessarily
    // lead to transaction failure.
    (MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION as u64)
        .min(tx_attempted_allocation_size.0)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_feature_set::FeatureSet,
        solana_instruction::Instruction,
        solana_message::Message,
        solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
        solana_svm_transaction::svm_message::SVMStaticMessage,
        solana_system_interface::instruction::{self as system_instruction},
        solana_transaction::Transaction,
    };

    #[test]
    fn test_calculate_no_allocation() {
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
            calculate(
                sanitized_tx.program_instructions_iter(),
                &FeatureSet::all_enabled()
            ),
            0
        );
    }

    #[test]
    fn test_calculate_multiple_allocations() {
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
            calculate(
                sanitized_tx.program_instructions_iter(),
                &FeatureSet::all_enabled()
            ),
            space1 + space2
        );
    }

    #[test]
    fn test_calculate_max_limit() {
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
            calculate(
                sanitized_tx.program_instructions_iter(),
                &FeatureSet::all_enabled()
            ),
            MAX_PERMITTED_ACCOUNTS_DATA_ALLOCATIONS_PER_TRANSACTION as u64,
        );
    }

    #[test]
    fn test_calculate_overflow() {
        let space = 100;
        let transaction = Transaction::new_unsigned(Message::new(
            &[
                system_instruction::create_account(
                    &Pubkey::new_unique(),
                    &Pubkey::new_unique(),
                    1,
                    space,
                    &Pubkey::new_unique(),
                ),
                system_instruction::allocate(&Pubkey::new_unique(), u64::MAX),
            ],
            Some(&Pubkey::new_unique()),
        ));
        let sanitized_tx = RuntimeTransaction::from_transaction_for_tests(transaction);

        assert_eq!(
            space,
            calculate(
                sanitized_tx.program_instructions_iter(),
                &FeatureSet::all_enabled()
            ),
        );
    }

    #[test]
    fn test_calculate_invalid_ix() {
        let space = 100;
        let transaction = Transaction::new_unsigned(Message::new(
            &[
                system_instruction::allocate(&Pubkey::new_unique(), space),
                Instruction::new_with_bincode(system_program::id(), &(), vec![]),
            ],
            Some(&Pubkey::new_unique()),
        ));
        let sanitized_tx = RuntimeTransaction::from_transaction_for_tests(transaction);

        assert_eq!(
            space,
            calculate(
                sanitized_tx.program_instructions_iter(),
                &FeatureSet::all_enabled()
            ),
        );
    }

    #[test]
    fn test_calculate_on_deserialized_system_instruction() {
        let lamports = 0;
        let owner = Pubkey::default();
        let seed = String::default();
        let space = 100;
        let base = Pubkey::default();
        let feature_set = FeatureSet::all_enabled();

        for instruction in [
            SystemInstruction::CreateAccount {
                lamports,
                space,
                owner,
            },
            SystemInstruction::CreateAccountAllowPrefund {
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
                calculate_on_deserialized_system_instruction(instruction, &feature_set)
            );
        }
        assert_eq!(
            SystemProgramAccountAllocation::None,
            calculate_on_deserialized_system_instruction(
                SystemInstruction::TransferWithSeed {
                    lamports,
                    from_seed: String::default(),
                    from_owner: Pubkey::default(),
                },
                &feature_set
            )
        );
    }

    #[test]
    fn test_create_account_allow_prefund_feature_gate() {
        let lamports = 0;
        let owner = Pubkey::default();
        let space = 100;
        let instruction = SystemInstruction::CreateAccountAllowPrefund {
            lamports,
            space,
            owner,
        };

        let feature_set_enabled = FeatureSet::all_enabled();
        assert_eq!(
            SystemProgramAccountAllocation::Some(space),
            calculate_on_deserialized_system_instruction(instruction.clone(), &feature_set_enabled)
        );

        let feature_set_disabled = FeatureSet::default();
        assert_eq!(
            SystemProgramAccountAllocation::Failed,
            calculate_on_deserialized_system_instruction(instruction, &feature_set_disabled)
        );
    }
}
