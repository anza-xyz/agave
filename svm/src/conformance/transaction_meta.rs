use {
    agave_feature_set::FeatureSet,
    solana_compute_budget::compute_budget_limits::{
        MAX_COMPUTE_UNIT_LIMIT, MAX_HEAP_FRAME_BYTES, MIN_HEAP_FRAME_BYTES,
    },
    solana_compute_budget_instruction::instructions_processor::process_compute_budget_instructions,
    solana_message::SanitizedMessage,
    solana_program_entrypoint::HEAP_LENGTH,
    solana_svm_transaction::svm_message::SVMStaticMessage,
    solana_transaction_error::TransactionError,
};

/// Bare minimum configuration, only for the harness usage.
pub struct TransactionConfiguration {
    pub updated_heap_bytes: u32,
    pub compute_unit_limit: u32,
}

impl TryFrom<(&SanitizedMessage, &FeatureSet)> for TransactionConfiguration {
    type Error = TransactionError;

    fn try_from(
        (message, feature_set): (&SanitizedMessage, &FeatureSet),
    ) -> Result<Self, Self::Error> {
        match message {
            SanitizedMessage::Legacy(_) | SanitizedMessage::V0(_) => {
                let limits = process_compute_budget_instructions(
                    SVMStaticMessage::program_instructions_iter(message),
                    feature_set,
                )?;
                Ok(Self {
                    updated_heap_bytes: limits.updated_heap_bytes,
                    compute_unit_limit: limits.compute_unit_limit,
                })
            }
            SanitizedMessage::V1(message) => {
                let updated_heap_bytes = message
                    .message
                    .config
                    .heap_size
                    .unwrap_or(HEAP_LENGTH as u32);
                if !(MIN_HEAP_FRAME_BYTES..=MAX_HEAP_FRAME_BYTES).contains(&updated_heap_bytes)
                    || !updated_heap_bytes.is_multiple_of(1024)
                {
                    return Err(TransactionError::SanitizeFailure);
                }

                Ok(Self {
                    updated_heap_bytes,
                    compute_unit_limit: message
                        .message
                        .config
                        .compute_unit_limit
                        .unwrap_or(0)
                        .min(MAX_COMPUTE_UNIT_LIMIT),
                })
            }
        }
    }
}
