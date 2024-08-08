use {
    solana_compute_budget::compute_budget_limits::*,
    solana_sdk::{
        borsh1::try_from_slice_unchecked,
        compute_budget::{self, ComputeBudgetInstruction},
        instruction::{CompiledInstruction, InstructionError},
        pubkey::Pubkey,
        saturating_add_assign,
        transaction::{Result, TransactionError},
    },
};

#[cfg_attr(test, derive(Eq, PartialEq))]
#[derive(Default, Debug)]
pub struct ComputeBudgetInstructionDetails {
    // compute-budget instruction details:
    // the first field in tuple is instruction index, second field is the unsanitized value set by user
    pub requested_compute_unit_limit: Option<(u8, u32)>,
    pub requested_compute_unit_price: Option<(u8, u64)>,
    pub requested_heap_size: Option<(u8, u32)>,
    pub requested_loaded_accounts_data_size_limit: Option<(u8, u32)>,
    pub count_compute_budget_instructions: u32,
}

impl ComputeBudgetInstructionDetails {
    pub fn process_instruction<'a>(
        &mut self,
        index: u8,
        program_id: &'a Pubkey,
        instruction: &'a CompiledInstruction,
    ) -> Result<()> {
        if compute_budget::check_id(program_id) {
            let invalid_instruction_data_error =
                TransactionError::InstructionError(index, InstructionError::InvalidInstructionData);
            let duplicate_instruction_error = TransactionError::DuplicateInstruction(index);

            match try_from_slice_unchecked(&instruction.data) {
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
            saturating_add_assign!(self.count_compute_budget_instructions, 1);
        }

        Ok(())
    }

    fn sanitize_requested_heap_size(bytes: u32) -> bool {
        (MIN_HEAP_FRAME_BYTES..=MAX_HEAP_FRAME_BYTES).contains(&bytes) && bytes % 1024 == 0
    }
}

#[cfg(test)]
mod test {
    use {super::*, solana_sdk::instruction::Instruction};

    fn setup_test_instruction(
        index: u8,
        instruction: Instruction,
    ) -> (Pubkey, CompiledInstruction) {
        (
            instruction.program_id,
            CompiledInstruction {
                program_id_index: index,
                data: instruction.data.clone(),
                accounts: vec![],
            },
        )
    }

    #[test]
    fn test_process_instruction_request_heap() {
        let mut index = 0;
        let mut expected_details = ComputeBudgetInstructionDetails::default();
        let mut compute_budget_instruction_details = ComputeBudgetInstructionDetails::default();

        // invalid data results error
        let expected_err = Err(TransactionError::InstructionError(
            index,
            InstructionError::InvalidInstructionData,
        ));
        let (program_id, ix) = setup_test_instruction(
            index,
            ComputeBudgetInstruction::request_heap_frame(MIN_HEAP_FRAME_BYTES - 1),
        );
        assert_eq!(
            compute_budget_instruction_details.process_instruction(index, &program_id, &ix),
            expected_err
        );
        let (program_id, ix) = setup_test_instruction(
            index,
            ComputeBudgetInstruction::request_heap_frame(MAX_HEAP_FRAME_BYTES + 1),
        );
        assert_eq!(
            compute_budget_instruction_details.process_instruction(index, &program_id, &ix),
            expected_err
        );
        let (program_id, ix) = setup_test_instruction(
            index,
            ComputeBudgetInstruction::request_heap_frame(1024 + 1),
        );
        assert_eq!(
            compute_budget_instruction_details.process_instruction(index, &program_id, &ix),
            expected_err
        );
        let (program_id, ix) = setup_test_instruction(
            index,
            ComputeBudgetInstruction::request_heap_frame(31 * 1024),
        );
        assert_eq!(
            compute_budget_instruction_details.process_instruction(index, &program_id, &ix),
            expected_err
        );

        // irrelevant instruction makes no change
        index += 1;
        let (program_id, ix) = setup_test_instruction(
            index,
            Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
        );
        assert!(compute_budget_instruction_details
            .process_instruction(index, &program_id, &ix)
            .is_ok());
        assert_eq!(compute_budget_instruction_details, expected_details);

        // valid instruction
        index += 1;
        let (program_id, ix) = setup_test_instruction(
            index,
            ComputeBudgetInstruction::request_heap_frame(40 * 1024),
        );
        expected_details.requested_heap_size = Some((index, 40 * 1024));
        expected_details.count_compute_budget_instructions = 1;
        assert!(compute_budget_instruction_details
            .process_instruction(index, &program_id, &ix)
            .is_ok());
        assert_eq!(compute_budget_instruction_details, expected_details);

        // duplicate instruction results error
        index += 1;
        let expected_err = Err(TransactionError::DuplicateInstruction(index));
        let (program_id, ix) = setup_test_instruction(
            index,
            ComputeBudgetInstruction::request_heap_frame(50 * 1024),
        );
        assert_eq!(
            compute_budget_instruction_details.process_instruction(index, &program_id, &ix),
            expected_err
        );
        assert_eq!(compute_budget_instruction_details, expected_details);

        // irrelevant instruction makes no change
        index += 1;
        let (program_id, ix) = setup_test_instruction(
            index,
            Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
        );
        assert!(compute_budget_instruction_details
            .process_instruction(index, &program_id, &ix)
            .is_ok());
        assert_eq!(compute_budget_instruction_details, expected_details);
    }

    #[test]
    fn test_process_instruction_compute_unit_limit() {
        let mut index = 0;
        let mut expected_details = ComputeBudgetInstructionDetails::default();
        let mut compute_budget_instruction_details = ComputeBudgetInstructionDetails::default();

        // irrelevant instruction makes no change
        let (program_id, ix) = setup_test_instruction(
            index,
            Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
        );
        assert!(compute_budget_instruction_details
            .process_instruction(index, &program_id, &ix)
            .is_ok());
        assert_eq!(compute_budget_instruction_details, expected_details);

        // valid instruction,
        index += 1;
        let (program_id, ix) = setup_test_instruction(
            index,
            ComputeBudgetInstruction::set_compute_unit_limit(u32::MAX),
        );
        expected_details.requested_compute_unit_limit = Some((index, u32::MAX));
        expected_details.count_compute_budget_instructions = 1;
        assert!(compute_budget_instruction_details
            .process_instruction(index, &program_id, &ix)
            .is_ok());
        assert_eq!(compute_budget_instruction_details, expected_details);

        // duplicate instruction results error
        index += 1;
        let expected_err = Err(TransactionError::DuplicateInstruction(index));
        let (program_id, ix) = setup_test_instruction(
            index,
            ComputeBudgetInstruction::set_compute_unit_limit(MAX_COMPUTE_UNIT_LIMIT),
        );
        assert_eq!(
            compute_budget_instruction_details.process_instruction(index, &program_id, &ix),
            expected_err
        );
        assert_eq!(compute_budget_instruction_details, expected_details);

        // irrelevant instruction makes no change
        index += 1;
        let (program_id, ix) = setup_test_instruction(
            index,
            Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
        );
        assert!(compute_budget_instruction_details
            .process_instruction(index, &program_id, &ix)
            .is_ok());
        assert_eq!(compute_budget_instruction_details, expected_details);
    }

    #[test]
    fn test_process_instruction_compute_unit_price() {
        let mut index = 0;
        let mut expected_details = ComputeBudgetInstructionDetails::default();
        let mut compute_budget_instruction_details = ComputeBudgetInstructionDetails::default();

        // irrelevant instruction makes no change
        let (program_id, ix) = setup_test_instruction(
            index,
            Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
        );
        assert!(compute_budget_instruction_details
            .process_instruction(index, &program_id, &ix)
            .is_ok());
        assert_eq!(compute_budget_instruction_details, expected_details);

        // valid instruction,
        index += 1;
        let (program_id, ix) = setup_test_instruction(
            index,
            ComputeBudgetInstruction::set_compute_unit_price(u64::MAX),
        );
        expected_details.requested_compute_unit_price = Some((index, u64::MAX));
        expected_details.count_compute_budget_instructions = 1;
        assert!(compute_budget_instruction_details
            .process_instruction(index, &program_id, &ix)
            .is_ok());
        assert_eq!(compute_budget_instruction_details, expected_details);

        // duplicate instruction results error
        index += 1;
        let expected_err = Err(TransactionError::DuplicateInstruction(index));
        let (program_id, ix) =
            setup_test_instruction(index, ComputeBudgetInstruction::set_compute_unit_price(0));
        assert_eq!(
            compute_budget_instruction_details.process_instruction(index, &program_id, &ix),
            expected_err
        );
        assert_eq!(compute_budget_instruction_details, expected_details);

        // irrelevant instruction makes no change
        index += 1;
        let (program_id, ix) = setup_test_instruction(
            index,
            Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
        );
        assert!(compute_budget_instruction_details
            .process_instruction(index, &program_id, &ix)
            .is_ok());
        assert_eq!(compute_budget_instruction_details, expected_details);
    }

    #[test]
    fn test_process_instruction_loaded_accounts_data_size_limit() {
        let mut index = 0;
        let mut expected_details = ComputeBudgetInstructionDetails::default();
        let mut compute_budget_instruction_details = ComputeBudgetInstructionDetails::default();

        // irrelevant instruction makes no change
        let (program_id, ix) = setup_test_instruction(
            index,
            Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
        );
        assert!(compute_budget_instruction_details
            .process_instruction(index, &program_id, &ix)
            .is_ok());
        assert_eq!(compute_budget_instruction_details, expected_details);

        // valid instruction,
        index += 1;
        let (program_id, ix) = setup_test_instruction(
            index,
            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(u32::MAX),
        );
        expected_details.requested_loaded_accounts_data_size_limit = Some((index, u32::MAX));
        expected_details.count_compute_budget_instructions = 1;
        assert!(compute_budget_instruction_details
            .process_instruction(index, &program_id, &ix)
            .is_ok());
        assert_eq!(compute_budget_instruction_details, expected_details);

        // duplicate instruction results error
        index += 1;
        let expected_err = Err(TransactionError::DuplicateInstruction(index));
        let (program_id, ix) = setup_test_instruction(
            index,
            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(0),
        );
        assert_eq!(
            compute_budget_instruction_details.process_instruction(index, &program_id, &ix),
            expected_err
        );
        assert_eq!(compute_budget_instruction_details, expected_details);

        // irrelevant instruction makes no change
        index += 1;
        let (program_id, ix) = setup_test_instruction(
            index,
            Instruction::new_with_bincode(Pubkey::new_unique(), &0_u8, vec![]),
        );
        assert!(compute_budget_instruction_details
            .process_instruction(index, &program_id, &ix)
            .is_ok());
        assert_eq!(compute_budget_instruction_details, expected_details);
    }
}
