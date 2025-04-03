use {
    crate::{
        instruction_data_len::InstructionDataLenBuilder,
        signature_details::{PrecompileSignatureDetails, PrecompileSignatureDetailsBuilder},
    },
    solana_compute_budget_instruction::compute_budget_instruction_details::{
        ComputeBudgetInstructionDetails, ComputeBudgetInstructionDetailsBuilder,
    },
    solana_pubkey::Pubkey,
    solana_svm_transaction::instruction::SVMInstruction,
    solana_transaction_error::TransactionError,
};

pub struct InstructionMeta {
    pub precompile_signature_details: PrecompileSignatureDetails,
    pub instruction_data_len: u16,
    pub compute_budget_instruction_details: ComputeBudgetInstructionDetails,
}

impl InstructionMeta {
    pub fn try_new<'a>(
        instructions: impl Iterator<Item = (&'a Pubkey, SVMInstruction<'a>)>,
    ) -> Result<Self, TransactionError> {
        let mut precompile_signature_details_builder = PrecompileSignatureDetailsBuilder::default();
        let mut instruction_data_len_builder = InstructionDataLenBuilder::default();
        let mut compute_budget_instruction_details_builder =
            ComputeBudgetInstructionDetailsBuilder::default();
        for (program_id, instruction) in instructions {
            precompile_signature_details_builder.process_instruction(program_id, &instruction);
            instruction_data_len_builder.process_instruction(program_id, &instruction);
            compute_budget_instruction_details_builder
                .process_instruction(program_id, &instruction)?;
        }

        Ok(Self {
            precompile_signature_details: precompile_signature_details_builder.build(),
            instruction_data_len: instruction_data_len_builder.build(),
            compute_budget_instruction_details: compute_budget_instruction_details_builder.build(),
        })
    }
}
