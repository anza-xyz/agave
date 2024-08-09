use {
    solana_builtins_default_costs::MAYBE_BUILTIN_KEY,
    solana_compute_budget::compute_budget_limits::*,
    solana_sdk::{
        instruction::{CompiledInstruction, InstructionError},
        pubkey::Pubkey,
        saturating_add_assign,
        transaction::{Result, TransactionError},
    },
    std::num::NonZeroU32,
};

/// Information about instructions gathered after scan over transaction;
/// These are "raw" information that suitable for cache and reuse.
#[cfg_attr(test, derive(Eq, PartialEq))]
#[derive(Default, Debug)]
pub struct InstructionDetails {
    // compute-budget instruction details:
    // the first field in tuple is instruction index, second field is the unsanitized value set by user
    pub requested_compute_unit_limit: Option<(u8, u32)>,
    pub requested_compute_unit_price: Option<(u8, u64)>,
    pub requested_heap_size: Option<(u8, u32)>,
    pub requested_loaded_accounts_data_size_limit: Option<(u8, u32)>,
    // builtin instruction details
    pub sum_builtin_compute_units: u32,
    pub count_builtin_instructions: u32,
    pub count_non_builtin_instructions: u32,
    pub count_compute_budget_instructions: u32,
}

impl InstructionDetails {
    pub fn try_from<'a>(
        instructions: impl Iterator<Item = (&'a Pubkey, &'a CompiledInstruction)>,
    ) -> Result<Self> {
        let mut instruction_details = InstructionDetails::default();

        for (i, (program_id, instruction)) in instructions.enumerate() {
            if MAYBE_BUILTIN_KEY[program_id.as_ref()[0] as usize] {
                instruction_details.process_compute_budget_instruction(
                    i as u8,
                    program_id,
                    instruction,
                )?;
                instruction_details.process_builtin_instruction(program_id, instruction)?;
            } else {
                saturating_add_assign!(instruction_details.count_non_builtin_instructions, 1);
            }
        }

        Ok(instruction_details)
    }

    pub fn sanitize_and_convert_to_compute_budget_limits(&self) -> Result<ComputeBudgetLimits> {
        // Sanitize requested heap size
        let updated_heap_bytes =
            if let Some((index, requested_heap_size)) = self.requested_heap_size {
                if Self::sanitize_requested_heap_size(requested_heap_size) {
                    requested_heap_size
                } else {
                    return Err(TransactionError::InstructionError(
                        index,
                        InstructionError::InvalidInstructionData,
                    ));
                }
            } else {
                MIN_HEAP_FRAME_BYTES
            }
            .min(MAX_HEAP_FRAME_BYTES);

        // Calculate compute unit limit
        let compute_unit_limit = self
            .requested_compute_unit_limit
            .map_or_else(
                || {
                    // NOTE: to match current behavior of:
                    // num_non_compute_budget_instructions * DEFAULT
                    self.num_non_compute_budget_instructions()
                        .saturating_mul(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT)
                },
                |(_index, requested_compute_unit_limit)| requested_compute_unit_limit,
            )
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

    fn sanitize_requested_heap_size(bytes: u32) -> bool {
        (MIN_HEAP_FRAME_BYTES..=MAX_HEAP_FRAME_BYTES).contains(&bytes) && bytes % 1024 == 0
    }

    fn num_non_compute_budget_instructions(&self) -> u32 {
        self.count_builtin_instructions
            .saturating_add(self.count_non_builtin_instructions)
            .saturating_sub(self.count_compute_budget_instructions)
    }
}

#[cfg(test)]
mod test {
    use {
        super::*,
        solana_sdk::{
            compute_budget::ComputeBudgetInstruction,
            instruction::{Instruction, InstructionError},
            message::Message,
            pubkey::Pubkey,
            signature::Keypair,
            signer::Signer,
            system_instruction::{self},
            transaction::{SanitizedTransaction, Transaction},
        },
    };

    macro_rules! test {
        ( $payer_keypair: expr, $instructions: expr, $expected_result: expr) => {
            let tx = SanitizedTransaction::from_transaction_for_tests(Transaction::new_unsigned(
                Message::new($instructions, Some(&$payer_keypair.pubkey())),
            ));

            let result = InstructionDetails::try_from(tx.message().program_instructions_iter());
            assert_eq!($expected_result, result);
        };
    }

    #[test]
    fn test_try_from() {
        let payer_keypair = Keypair::new();
        let dummy_program_id: Pubkey = "dummmy1111111111111111111111111111111111111"
            .parse()
            .unwrap();
        test!(payer_keypair, &[], Ok(InstructionDetails::default()));

        test!(
            payer_keypair,
            &[
                Instruction::new_with_bincode(dummy_program_id, &0_u8, vec![]),
                ComputeBudgetInstruction::request_heap_frame(40 * 1024),
                ComputeBudgetInstruction::set_compute_unit_limit(u32::MAX),
                ComputeBudgetInstruction::set_compute_unit_price(u64::MAX),
                ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(u32::MAX),
                Instruction::new_with_bincode(dummy_program_id, &0_u8, vec![]),
                system_instruction::transfer(&payer_keypair.pubkey(), &Pubkey::new_unique(), 1),
            ],
            Ok(InstructionDetails {
                sum_builtin_compute_units: 4 * solana_compute_budget_program::DEFAULT_COMPUTE_UNITS
                    as u32
                    + solana_system_program::system_processor::DEFAULT_COMPUTE_UNITS as u32,
                count_builtin_instructions: 5,
                count_non_builtin_instructions: 2,
                requested_compute_unit_limit: Some((2, u32::MAX)),
                requested_compute_unit_price: Some((3, u64::MAX)),
                requested_heap_size: Some((1, 40 * 1024)),
                requested_loaded_accounts_data_size_limit: Some((4, u32::MAX)),
                count_compute_budget_instructions: 4,
            })
        );

        // any invalid instruction would error out
        test!(
            payer_keypair,
            &[
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
                Instruction::new_with_bincode(solana_sdk::compute_budget::id(), &0_u8, vec![]), // invalid
                Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
            ],
            Err(TransactionError::InstructionError(
                1,
                InstructionError::InvalidInstructionData,
            ))
        );
    }

    #[test]
    fn test_sanitize_and_convert_to_compute_budget_limits() {
        // empty details, default ComputeBudgetLimits with 0 compute_unit_limits
        let instruction_details = InstructionDetails::default();
        assert_eq!(
            instruction_details.sanitize_and_convert_to_compute_budget_limits(),
            Ok(ComputeBudgetLimits {
                compute_unit_limit: 0,
                ..ComputeBudgetLimits::default()
            })
        );

        let count_builtin_instructions = 5;
        let count_non_builtin_instructions = 2;
        let count_compute_budget_instructions = 4;

        // no compute-budget instructions, all default ComputeBudgetLimits except cu-limit
        let instruction_details = InstructionDetails {
            count_builtin_instructions,
            count_non_builtin_instructions,
            ..InstructionDetails::default()
        };
        let expected_compute_unit_limit = (count_builtin_instructions
            + count_non_builtin_instructions)
            * DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT;
        assert_eq!(
            instruction_details.sanitize_and_convert_to_compute_budget_limits(),
            Ok(ComputeBudgetLimits {
                compute_unit_limit: expected_compute_unit_limit,
                ..ComputeBudgetLimits::default()
            })
        );

        let expected_heap_size_err = Err(TransactionError::InstructionError(
            3,
            InstructionError::InvalidInstructionData,
        ));
        // invalid: requested_heap_size can't be zero
        let instruction_details = InstructionDetails {
            requested_compute_unit_limit: Some((1, 0)),
            requested_compute_unit_price: Some((2, 0)),
            requested_heap_size: Some((3, 0)),
            requested_loaded_accounts_data_size_limit: Some((4, 1024)),
            count_compute_budget_instructions,
            count_builtin_instructions,
            count_non_builtin_instructions,
            ..InstructionDetails::default()
        };
        assert_eq!(
            instruction_details.sanitize_and_convert_to_compute_budget_limits(),
            expected_heap_size_err
        );

        // invalid: requested_heap_size can't be less than MIN_HEAP_FRAME_BYTES
        let instruction_details = InstructionDetails {
            requested_compute_unit_limit: Some((1, 0)),
            requested_compute_unit_price: Some((2, 0)),
            requested_heap_size: Some((3, MIN_HEAP_FRAME_BYTES - 1)),
            requested_loaded_accounts_data_size_limit: Some((4, 1024)),
            count_compute_budget_instructions,
            count_builtin_instructions,
            count_non_builtin_instructions,
            ..InstructionDetails::default()
        };
        assert_eq!(
            instruction_details.sanitize_and_convert_to_compute_budget_limits(),
            expected_heap_size_err
        );

        // invalid: requested_heap_size can't be more than MAX_HEAP_FRAME_BYTES
        let instruction_details = InstructionDetails {
            requested_compute_unit_limit: Some((1, 0)),
            requested_compute_unit_price: Some((2, 0)),
            requested_heap_size: Some((3, MAX_HEAP_FRAME_BYTES + 1)),
            requested_loaded_accounts_data_size_limit: Some((4, 1024)),
            count_compute_budget_instructions,
            count_builtin_instructions,
            count_non_builtin_instructions,
            ..InstructionDetails::default()
        };
        assert_eq!(
            instruction_details.sanitize_and_convert_to_compute_budget_limits(),
            expected_heap_size_err
        );

        // invalid: requested_heap_size must be round by 1024
        let instruction_details = InstructionDetails {
            requested_compute_unit_limit: Some((1, 0)),
            requested_compute_unit_price: Some((2, 0)),
            requested_heap_size: Some((3, MIN_HEAP_FRAME_BYTES + 1024 + 1)),
            requested_loaded_accounts_data_size_limit: Some((4, 1024)),
            count_compute_budget_instructions,
            count_builtin_instructions,
            count_non_builtin_instructions,
            ..InstructionDetails::default()
        };
        assert_eq!(
            instruction_details.sanitize_and_convert_to_compute_budget_limits(),
            expected_heap_size_err
        );

        // invalid: loaded_account_data_size can't be zero
        let instruction_details = InstructionDetails {
            requested_compute_unit_limit: Some((1, 0)),
            requested_compute_unit_price: Some((2, 0)),
            requested_heap_size: Some((3, 40 * 1024)),
            requested_loaded_accounts_data_size_limit: Some((4, 0)),
            count_compute_budget_instructions,
            count_builtin_instructions,
            count_non_builtin_instructions,
            ..InstructionDetails::default()
        };
        assert_eq!(
            instruction_details.sanitize_and_convert_to_compute_budget_limits(),
            Err(TransactionError::InvalidLoadedAccountsDataSizeLimit)
        );

        // valid: acceptable MAX
        let instruction_details = InstructionDetails {
            requested_compute_unit_limit: Some((1, u32::MAX)),
            requested_compute_unit_price: Some((2, u64::MAX)),
            requested_heap_size: Some((3, MAX_HEAP_FRAME_BYTES)),
            requested_loaded_accounts_data_size_limit: Some((4, u32::MAX)),
            count_compute_budget_instructions,
            count_builtin_instructions,
            count_non_builtin_instructions,
            ..InstructionDetails::default()
        };
        assert_eq!(
            instruction_details.sanitize_and_convert_to_compute_budget_limits(),
            Ok(ComputeBudgetLimits {
                updated_heap_bytes: MAX_HEAP_FRAME_BYTES,
                compute_unit_limit: MAX_COMPUTE_UNIT_LIMIT,
                compute_unit_price: u64::MAX,
                loaded_accounts_bytes: MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
            })
        );

        // valid
        let val: u32 = 1024 * 40;
        let instruction_details = InstructionDetails {
            requested_compute_unit_limit: Some((1, val)),
            requested_compute_unit_price: Some((2, val as u64)),
            requested_heap_size: Some((3, val)),
            requested_loaded_accounts_data_size_limit: Some((4, val)),
            count_compute_budget_instructions,
            count_builtin_instructions,
            count_non_builtin_instructions,
            ..InstructionDetails::default()
        };
        assert_eq!(
            instruction_details.sanitize_and_convert_to_compute_budget_limits(),
            Ok(ComputeBudgetLimits {
                updated_heap_bytes: val,
                compute_unit_limit: val,
                compute_unit_price: val as u64,
                loaded_accounts_bytes: NonZeroU32::new(val).unwrap(),
            })
        );
    }
}
