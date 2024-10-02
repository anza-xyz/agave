use {
    solana_feature_set::FeatureSet,
    solana_sdk::{
        precompiles::get_precompiles,
        transaction::{Result, TransactionError},
    },
    solana_svm_transaction::svm_message::SVMMessage,
};

pub fn verify_precompiles(message: &impl SVMMessage, feature_set: &FeatureSet) -> Result<()> {
    let mut all_instruction_data = None; // lazily collect this on first pre-compile

    let precompiles = get_precompiles();
    for (program_id, instruction) in message.program_instructions_iter() {
        for precompile in precompiles {
            if program_id == &precompile.program_id {
                let all_instruction_data: &Vec<&[u8]> = all_instruction_data
                    .get_or_insert_with(|| message.instructions_iter().map(|ix| ix.data).collect());
                precompile
                    .verify(instruction.data, all_instruction_data, feature_set)
                    .map_err(|_| TransactionError::InvalidAccountIndex)?;
                break;
            }
        }
    }

    Ok(())
}
