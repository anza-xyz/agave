use {
    solana_builtins_default_costs::BUILTIN_INSTRUCTION_COSTS,
    solana_compute_budget::compute_budget_limits::*,
    solana_sdk::{
        borsh1::try_from_slice_unchecked,
        compute_budget::{self, ComputeBudgetInstruction},
        instruction::InstructionError,
        pubkey::Pubkey,
        saturating_add_assign,
        transaction::{Result, TransactionError},
    },
    solana_svm_transaction::instruction::SVMInstruction,
    std::num::NonZeroU32,
};

/// Information about instructions gathered after scan over transaction;
/// These are "raw" information that suitable for cache and reuse.
#[derive(Default, Debug)]
pub struct InstructionDetails {
    compute_budget_instruction_details: ComputeBudgetInstructionDetails,
    builtin_instruction_details: BuiltinInstructionDetails,
}

#[derive(Default, Debug)]
struct ComputeBudgetInstructionDetails {
    // compute-budget instruction details:
    // the first field in tuple is instruction index, second field is the unsanitized value set by user
    requested_compute_unit_limit: Option<(u8, u32)>,
    requested_compute_unit_price: Option<(u8, u64)>,
    requested_heap_size: Option<(u8, u32)>,
    requested_loaded_accounts_data_size_limit: Option<(u8, u32)>,
    count_compute_budget_instructions: u32,
}

#[derive(Default, Debug)]
struct BuiltinInstructionDetails {
    // builtin instruction details
    sum_builtin_compute_units: u32,
    count_builtin_instructions: u32,
    count_non_builtin_instructions: u32,
}

impl ComputeBudgetInstructionDetails {
    pub fn process_instruction(
        &mut self,
        index: u8,
        program_id: &Pubkey,
        instruction: &SVMInstruction,
    ) -> Result<()> {
        if compute_budget::check_id(program_id) {
            saturating_add_assign!(self.count_compute_budget_instructions, 1);

            let invalid_instruction_data_error =
                TransactionError::InstructionError(index, InstructionError::InvalidInstructionData);
            let duplicate_instruction_error = TransactionError::DuplicateInstruction(index);

            match try_from_slice_unchecked(instruction.data) {
                Ok(ComputeBudgetInstruction::RequestHeapFrame(bytes)) => {
                    if self.requested_heap_size.is_some() {
                        return Err(duplicate_instruction_error);
                    }
                    if Self::sanitize_requested_heap_size(bytes) {
                        self.requested_heap_size = Some((index, bytes));
                    } else {
                        return Err(invalid_instruction_data_error);
                    }
                }
                Ok(ComputeBudgetInstruction::SetComputeUnitLimit(compute_unit_limit)) => {
                    if self.requested_compute_unit_limit.is_some() {
                        return Err(duplicate_instruction_error);
                    }
                    self.requested_compute_unit_limit = Some((index, compute_unit_limit));
                }
                Ok(ComputeBudgetInstruction::SetComputeUnitPrice(micro_lamports)) => {
                    if self.requested_compute_unit_price.is_some() {
                        return Err(duplicate_instruction_error);
                    }
                    self.requested_compute_unit_price = Some((index, micro_lamports));
                }
                Ok(ComputeBudgetInstruction::SetLoadedAccountsDataSizeLimit(bytes)) => {
                    if self.requested_loaded_accounts_data_size_limit.is_some() {
                        return Err(duplicate_instruction_error);
                    }
                    self.requested_loaded_accounts_data_size_limit = Some((index, bytes));
                }
                _ => return Err(invalid_instruction_data_error),
            }
        }

        Ok(())
    }

    fn sanitize_requested_heap_size(bytes: u32) -> bool {
        (MIN_HEAP_FRAME_BYTES..=MAX_HEAP_FRAME_BYTES).contains(&bytes) && bytes % 1024 == 0
    }
}

impl BuiltinInstructionDetails {
    pub fn process_instruction<'a>(
        &mut self,
        program_id: &Pubkey,
        _instruction: &SVMInstruction,
    ) -> Result<()> {
        if let Some(builtin_ix_cost) = BUILTIN_INSTRUCTION_COSTS.get(program_id) {
            saturating_add_assign!(
                self.sum_builtin_compute_units,
                u32::try_from(*builtin_ix_cost).unwrap()
            );
            saturating_add_assign!(self.count_builtin_instructions, 1);
        } else {
            saturating_add_assign!(self.count_non_builtin_instructions, 1);
        }

        Ok(())
    }
}

impl InstructionDetails {
    pub fn sanitize_and_convert_to_compute_budget_limits(&self) -> Result<ComputeBudgetLimits> {
        // Sanitize requested heap size
        let updated_heap_bytes = self
            .compute_budget_instruction_details
            .requested_heap_size
            .map_or(MIN_HEAP_FRAME_BYTES, |(_index, requested_heap_size)| {
                requested_heap_size
            })
            .min(MAX_HEAP_FRAME_BYTES);

        // Calculate compute unit limit
        let compute_unit_limit = self
            .compute_budget_instruction_details
            .requested_compute_unit_limit
            .map_or_else(
                || {
                    // NOTE: to match current behavior of:
                    // num_non_compute_budget_instructions * DEFAULT
                    self.builtin_instruction_details
                        .count_builtin_instructions
                        .saturating_add(
                            self.builtin_instruction_details
                                .count_non_builtin_instructions,
                        )
                        .saturating_sub(
                            self.compute_budget_instruction_details
                                .count_compute_budget_instructions,
                        )
                        .saturating_mul(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT)
                },
                |(_index, requested_compute_unit_limit)| requested_compute_unit_limit,
            )
            .min(MAX_COMPUTE_UNIT_LIMIT);

        let compute_unit_price = self
            .compute_budget_instruction_details
            .requested_compute_unit_price
            .map_or(0, |(_index, requested_compute_unit_price)| {
                requested_compute_unit_price
            });

        let loaded_accounts_bytes =
            if let Some((_index, requested_loaded_accounts_data_size_limit)) = self
                .compute_budget_instruction_details
                .requested_loaded_accounts_data_size_limit
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

    pub fn try_from<'a>(
        instructions: impl Iterator<Item = (&'a Pubkey, SVMInstruction<'a>)>,
    ) -> Result<Self> {
        let mut compute_budget_instruction_details = ComputeBudgetInstructionDetails::default();
        let mut builtin_instruction_details = BuiltinInstructionDetails::default();

        for (i, (program_id, instruction)) in instructions.enumerate() {
            compute_budget_instruction_details.process_instruction(
                i as u8,
                program_id,
                &instruction,
            )?;
            builtin_instruction_details.process_instruction(program_id, &instruction)?;
        }

        Ok(Self {
            compute_budget_instruction_details,
            builtin_instruction_details,
        })
    }
}
