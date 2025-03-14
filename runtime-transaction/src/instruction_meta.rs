use {
    crate::signature_details::{PrecompileSignatureDetails, PrecompileSignatureDetailsBuilder},
    solana_pubkey::Pubkey,
    solana_svm_transaction::instruction::SVMInstruction,
    solana_transaction_error::TransactionError,
};

pub struct InstructionMeta {
    pub precompile_signature_details: PrecompileSignatureDetails,
}

impl InstructionMeta {
    pub fn try_new<'a>(
        instructions: impl Iterator<Item = (&'a Pubkey, SVMInstruction<'a>)>,
    ) -> Result<Self, TransactionError> {
        let mut precompile_signature_details_builder = PrecompileSignatureDetailsBuilder::default();
        for (program_id, instruction) in instructions {
            precompile_signature_details_builder.process_instruction(program_id, instruction);
        }

        Ok(Self {
            precompile_signature_details: precompile_signature_details_builder.build(),
        })
    }
}
