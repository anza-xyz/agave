#![cfg_attr(RUSTC_WITH_SPECIALIZATION, feature(min_specialization))]
use {
    solana_builtins_default_costs::BUILTIN_INSTRUCTION_COSTS,
    solana_compute_budget::compute_budget_limits::*,
    solana_sdk::{
        borsh1::try_from_slice_unchecked,
        compute_budget::{self, ComputeBudgetInstruction},
        feature_set::{self, FeatureSet},
        instruction::{CompiledInstruction, InstructionError},
        pubkey::Pubkey,
        saturating_add_assign,
        transaction::TransactionError,
    },
    std::num::NonZeroU32,
};

/// Information about instructions gathered after scan over transaction;
/// These are "raw" information that suitable for cache and reuse.
#[derive(Default, Debug)]
pub struct InstructionDetails {
    // compute-budget instruction details:
    // the first field in tuple is instruction index, second field is the unsanitized value set by user
    requested_compute_unit_limit: Option<(u8, u32)>,
    requested_compute_unit_price: Option<(u8, u64)>,
    requested_heap_size: Option<(u8, u32)>,
    requested_loaded_accounts_data_size_limit: Option<(u8, u32)>,
    // builtin instruction details
    sum_builtin_compute_units: u32,
    count_builtin_instructions: u32,
    count_non_builtin_instructions: u32,
    count_compute_budget_instructions: u32,
    // NOTE: additional instruction details goes here
    // for example: signature_details here (SanitizedMessage::get_signature_details())
}

impl InstructionDetails {
    pub fn sanitize_and_convert_to_compute_budget_limits(
        &self,
        feature_set: &FeatureSet,
    ) -> Result<ComputeBudgetLimits, TransactionError> {
        // Sanitize requested heap size
        let updated_heap_bytes = self
            .requested_heap_size
            .map_or(MIN_HEAP_FRAME_BYTES, |(_index, requested_heap_size)| {
                requested_heap_size
            })
            .min(MAX_HEAP_FRAME_BYTES);

        // Calculate compute unit limit
        let compute_unit_limit = if let Some((index, requested_compute_unit_limit)) =
            self.requested_compute_unit_limit
        {
            if feature_set.is_active(&feature_set::compute_budget_uses_builtin_default_costs::id())
                && !(self.sum_builtin_compute_units..=MAX_COMPUTE_UNIT_LIMIT)
                    .contains(&requested_compute_unit_limit)
            {
                return Err(TransactionError::InstructionError(
                    index,
                    InstructionError::InvalidInstructionData,
                ));
            }
            requested_compute_unit_limit
        } else if feature_set
            .is_active(&feature_set::compute_budget_uses_builtin_default_costs::id())
        {
            self.sum_builtin_compute_units.saturating_add(
                self.count_non_builtin_instructions
                    .saturating_mul(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT),
            )
        } else {
            // current behavior of:
            // num_non_compute_budget_instructions * DEFAULT
            self.count_builtin_instructions
                .saturating_add(self.count_non_builtin_instructions)
                .saturating_sub(self.count_compute_budget_instructions)
                .saturating_mul(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT)
        }
        .min(MAX_COMPUTE_UNIT_LIMIT);

        let compute_unit_price = self
            .requested_compute_unit_price
            .map_or(0, |(_index, requested_compute_unit_price)| {
                requested_compute_unit_price
            });

        let loaded_accounts_bytes =
            if let Some((_index, requested_loaded_accounts_data_size_limit)) =
                self.requested_loaded_accounts_data_size_limit
            {
                NonZeroU32::new(requested_loaded_accounts_data_size_limit)
                    .ok_or(TransactionError::InvalidLoadedAccountsDataSizeLimit)?
            } else {
                MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES
            }
            .min(MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES);

        Ok(ComputeBudgetLimits {
            updated_heap_bytes,
            compute_unit_limit,
            compute_unit_price,
            loaded_accounts_bytes,
        })
    }
}

/// Iterate instructions for unsanitized user inputs;
/// Returns TransactionError if instructions were invalid or syntax error, otherwise
/// returns `InstructionDetails` that is deterministic per transaction,
/// therefore is safe for cache and reuse. Cached `InstructionDetails`
/// can be sanitized and converted into `ComputeBudgetLimits`, for example.
pub fn get_instruction_details<'a>(
    instructions: impl Iterator<Item = (&'a Pubkey, &'a CompiledInstruction)>,
) -> Result<InstructionDetails, TransactionError> {
    let mut instruction_details = InstructionDetails::default();

    for (i, (program_id, instruction)) in instructions.enumerate() {
        parse_compute_budget_instructions(
            &mut instruction_details,
            i as u8,
            program_id,
            instruction,
        )?;
        parse_builtin_instructions(&mut instruction_details, i as u8, program_id, instruction)?;
    }

    Ok(instruction_details)
}

fn parse_builtin_instructions<'a>(
    instruction_details: &mut InstructionDetails,
    _index: u8,
    program_id: &'a Pubkey,
    _instruction: &'a CompiledInstruction,
) -> Result<(), TransactionError> {
    if let Some(builtin_ix_cost) = BUILTIN_INSTRUCTION_COSTS.get(program_id) {
        saturating_add_assign!(
            instruction_details.sum_builtin_compute_units,
            u32::try_from(*builtin_ix_cost).unwrap()
        );
        saturating_add_assign!(instruction_details.count_builtin_instructions, 1);
    } else {
        saturating_add_assign!(instruction_details.count_non_builtin_instructions, 1);
    }

    Ok(())
}

fn parse_compute_budget_instructions<'a>(
    instruction_details: &mut InstructionDetails,
    index: u8,
    program_id: &'a Pubkey,
    instruction: &'a CompiledInstruction,
) -> Result<(), TransactionError> {
    if compute_budget::check_id(program_id) {
        saturating_add_assign!(instruction_details.count_compute_budget_instructions, 1);

        let invalid_instruction_data_error =
            TransactionError::InstructionError(index, InstructionError::InvalidInstructionData);
        let duplicate_instruction_error = TransactionError::DuplicateInstruction(index);

        match try_from_slice_unchecked(&instruction.data) {
            Ok(ComputeBudgetInstruction::RequestHeapFrame(bytes)) => {
                if instruction_details.requested_heap_size.is_some() {
                    return Err(duplicate_instruction_error);
                }
                if sanitize_requested_heap_size(bytes) {
                    instruction_details.requested_heap_size = Some((index, bytes));
                } else {
                    return Err(invalid_instruction_data_error);
                }
            }
            Ok(ComputeBudgetInstruction::SetComputeUnitLimit(compute_unit_limit)) => {
                if instruction_details.requested_compute_unit_limit.is_some() {
                    return Err(duplicate_instruction_error);
                }
                instruction_details.requested_compute_unit_limit =
                    Some((index, compute_unit_limit));
            }
            Ok(ComputeBudgetInstruction::SetComputeUnitPrice(micro_lamports)) => {
                if instruction_details.requested_compute_unit_price.is_some() {
                    return Err(duplicate_instruction_error);
                }
                instruction_details.requested_compute_unit_price = Some((index, micro_lamports));
            }
            Ok(ComputeBudgetInstruction::SetLoadedAccountsDataSizeLimit(bytes)) => {
                if instruction_details
                    .requested_loaded_accounts_data_size_limit
                    .is_some()
                {
                    return Err(duplicate_instruction_error);
                }
                instruction_details.requested_loaded_accounts_data_size_limit =
                    Some((index, bytes));
            }
            _ => return Err(invalid_instruction_data_error),
        }
    }

    Ok(())
}

fn sanitize_requested_heap_size(bytes: u32) -> bool {
    (MIN_HEAP_FRAME_BYTES..=MAX_HEAP_FRAME_BYTES).contains(&bytes) && bytes % 1024 == 0
}

// NOTE - temp adaptor to keep compiler happy for the time being
// all call sites will be updated with this two-step calls, using actual feature-set
pub fn process_compute_budget_instructions<'a>(
    instructions: impl Iterator<Item = (&'a Pubkey, &'a CompiledInstruction)>,
) -> Result<ComputeBudgetLimits, TransactionError> {
    // TODO - need pass featureset around
    get_instruction_details(instructions)?
        .sanitize_and_convert_to_compute_budget_limits(&FeatureSet::default())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_sdk::{
            hash::Hash,
            instruction::Instruction,
            message::Message,
            pubkey::Pubkey,
            signature::Keypair,
            signer::Signer,
            system_instruction::{self},
            transaction::{SanitizedTransaction, Transaction},
        },
    };

    macro_rules! test {
        ( $instructions: expr, $expected_result: expr) => {
            let payer_keypair = Keypair::new();
            let tx = SanitizedTransaction::from_transaction_for_tests(Transaction::new(
                &[&payer_keypair],
                Message::new($instructions, Some(&payer_keypair.pubkey())),
                Hash::default(),
            ));
            let result =
                process_compute_budget_instructions(tx.message().program_instructions_iter());
            assert_eq!($expected_result, result);
        };
    }

    #[test]
    fn test_process_instructions() {
        // Units
        test!(
            &[],
            Ok(ComputeBudgetLimits {
                compute_unit_limit: 0,
                ..ComputeBudgetLimits::default()
            })
        );
        test!(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(1),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
            ],
            Ok(ComputeBudgetLimits {
                compute_unit_limit: 1,
                ..ComputeBudgetLimits::default()
            })
        );
        test!(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(MAX_COMPUTE_UNIT_LIMIT + 1),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
            ],
            Ok(ComputeBudgetLimits {
                compute_unit_limit: MAX_COMPUTE_UNIT_LIMIT,
                ..ComputeBudgetLimits::default()
            })
        );
        test!(
            &[
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                ComputeBudgetInstruction::set_compute_unit_limit(MAX_COMPUTE_UNIT_LIMIT),
            ],
            Ok(ComputeBudgetLimits {
                compute_unit_limit: MAX_COMPUTE_UNIT_LIMIT,
                ..ComputeBudgetLimits::default()
            })
        );
        test!(
            &[
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                ComputeBudgetInstruction::set_compute_unit_limit(1),
            ],
            Ok(ComputeBudgetLimits {
                compute_unit_limit: 1,
                ..ComputeBudgetLimits::default()
            })
        );
        test!(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(1),
                ComputeBudgetInstruction::set_compute_unit_price(42)
            ],
            Ok(ComputeBudgetLimits {
                compute_unit_limit: 1,
                compute_unit_price: 42,
                ..ComputeBudgetLimits::default()
            })
        );

        // HeapFrame
        test!(
            &[],
            Ok(ComputeBudgetLimits {
                compute_unit_limit: 0,
                ..ComputeBudgetLimits::default()
            })
        );
        test!(
            &[
                ComputeBudgetInstruction::request_heap_frame(40 * 1024),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
            ],
            Ok(ComputeBudgetLimits {
                compute_unit_limit: DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
                updated_heap_bytes: 40 * 1024,
                ..ComputeBudgetLimits::default()
            })
        );
        test!(
            &[
                ComputeBudgetInstruction::request_heap_frame(40 * 1024 + 1),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
            ],
            Err(TransactionError::InstructionError(
                0,
                InstructionError::InvalidInstructionData,
            ))
        );
        test!(
            &[
                ComputeBudgetInstruction::request_heap_frame(31 * 1024),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
            ],
            Err(TransactionError::InstructionError(
                0,
                InstructionError::InvalidInstructionData,
            ))
        );
        test!(
            &[
                ComputeBudgetInstruction::request_heap_frame(MAX_HEAP_FRAME_BYTES + 1),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
            ],
            Err(TransactionError::InstructionError(
                0,
                InstructionError::InvalidInstructionData,
            ))
        );
        test!(
            &[
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                ComputeBudgetInstruction::request_heap_frame(MAX_HEAP_FRAME_BYTES),
            ],
            Ok(ComputeBudgetLimits {
                compute_unit_limit: DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
                updated_heap_bytes: MAX_HEAP_FRAME_BYTES,
                ..ComputeBudgetLimits::default()
            })
        );
        test!(
            &[
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                ComputeBudgetInstruction::request_heap_frame(1),
            ],
            Err(TransactionError::InstructionError(
                3,
                InstructionError::InvalidInstructionData,
            ))
        );
        test!(
            &[
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
            ],
            Ok(ComputeBudgetLimits {
                compute_unit_limit: DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT * 7,
                ..ComputeBudgetLimits::default()
            })
        );

        // Combined
        test!(
            &[
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                ComputeBudgetInstruction::request_heap_frame(MAX_HEAP_FRAME_BYTES),
                ComputeBudgetInstruction::set_compute_unit_limit(MAX_COMPUTE_UNIT_LIMIT),
                ComputeBudgetInstruction::set_compute_unit_price(u64::MAX),
            ],
            Ok(ComputeBudgetLimits {
                compute_unit_price: u64::MAX,
                compute_unit_limit: MAX_COMPUTE_UNIT_LIMIT,
                updated_heap_bytes: MAX_HEAP_FRAME_BYTES,
                ..ComputeBudgetLimits::default()
            })
        );
        test!(
            &[
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                ComputeBudgetInstruction::set_compute_unit_limit(1),
                ComputeBudgetInstruction::request_heap_frame(MAX_HEAP_FRAME_BYTES),
                ComputeBudgetInstruction::set_compute_unit_price(u64::MAX),
            ],
            Ok(ComputeBudgetLimits {
                compute_unit_price: u64::MAX,
                compute_unit_limit: 1,
                updated_heap_bytes: MAX_HEAP_FRAME_BYTES,
                ..ComputeBudgetLimits::default()
            })
        );

        // Duplicates
        test!(
            &[
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                ComputeBudgetInstruction::set_compute_unit_limit(MAX_COMPUTE_UNIT_LIMIT),
                ComputeBudgetInstruction::set_compute_unit_limit(MAX_COMPUTE_UNIT_LIMIT - 1),
            ],
            Err(TransactionError::DuplicateInstruction(2))
        );

        test!(
            &[
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                ComputeBudgetInstruction::request_heap_frame(MIN_HEAP_FRAME_BYTES),
                ComputeBudgetInstruction::request_heap_frame(MAX_HEAP_FRAME_BYTES),
            ],
            Err(TransactionError::DuplicateInstruction(2))
        );
        test!(
            &[
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                ComputeBudgetInstruction::set_compute_unit_price(0),
                ComputeBudgetInstruction::set_compute_unit_price(u64::MAX),
            ],
            Err(TransactionError::DuplicateInstruction(2))
        );
    }

    #[test]
    fn test_process_loaded_accounts_data_size_limit_instruction() {
        test!(
            &[],
            Ok(ComputeBudgetLimits {
                compute_unit_limit: 0,
                ..ComputeBudgetLimits::default()
            })
        );

        // Assert when set_loaded_accounts_data_size_limit presents,
        // budget is set with data_size
        let data_size = 1;
        let expected_result = Ok(ComputeBudgetLimits {
            compute_unit_limit: DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
            loaded_accounts_bytes: NonZeroU32::new(data_size).unwrap(),
            ..ComputeBudgetLimits::default()
        });

        test!(
            &[
                ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(data_size),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
            ],
            expected_result
        );

        // Assert when set_loaded_accounts_data_size_limit presents, with greater than max value
        // budget is set to max data size
        let data_size = MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get() + 1;
        let expected_result = Ok(ComputeBudgetLimits {
            compute_unit_limit: DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
            loaded_accounts_bytes: MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            ..ComputeBudgetLimits::default()
        });

        test!(
            &[
                ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(data_size),
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
            ],
            expected_result
        );

        // Assert when set_loaded_accounts_data_size_limit is not presented
        // budget is set to default data size
        let expected_result = Ok(ComputeBudgetLimits {
            compute_unit_limit: DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
            loaded_accounts_bytes: MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            ..ComputeBudgetLimits::default()
        });

        test!(
            &[Instruction::new_with_bincode(
                Pubkey::new_unique(),
                &0_u8,
                vec![]
            ),],
            expected_result
        );

        // Assert when set_loaded_accounts_data_size_limit presents more than once,
        // return DuplicateInstruction
        let data_size = MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.get();
        let expected_result = Err(TransactionError::DuplicateInstruction(2));

        test!(
            &[
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(data_size),
                ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(data_size),
            ],
            expected_result
        );
    }

    #[test]
    fn test_process_mixed_instructions_without_compute_budget() {
        let payer_keypair = Keypair::new();

        let transaction =
            SanitizedTransaction::from_transaction_for_tests(Transaction::new_signed_with_payer(
                &[
                    Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                    system_instruction::transfer(&payer_keypair.pubkey(), &Pubkey::new_unique(), 2),
                ],
                Some(&payer_keypair.pubkey()),
                &[&payer_keypair],
                Hash::default(),
            ));

        let result =
            process_compute_budget_instructions(transaction.message().program_instructions_iter());

        // assert process_instructions will be successful with default,
        // and the default compute_unit_limit is 2 times default: one for bpf ix, one for
        // builtin ix.
        assert_eq!(
            result,
            Ok(ComputeBudgetLimits {
                compute_unit_limit: 2 * DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT,
                ..ComputeBudgetLimits::default()
            })
        );
    }
}
