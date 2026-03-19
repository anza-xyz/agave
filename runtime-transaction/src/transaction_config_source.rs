use {
    agave_feature_set::FeatureSet,
    solana_compute_budget::compute_budget_limits::{
        ComputeBudgetLimits, MAX_COMPUTE_UNIT_LIMIT, MAX_HEAP_FRAME_BYTES,
        MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES, MIN_HEAP_FRAME_BYTES,
    },
    solana_compute_budget_instruction::compute_budget_instruction_details::ComputeBudgetInstructionDetails,
    solana_transaction_error::{TransactionError, TransactionResult as Result},
    std::num::NonZeroU32,
};

#[derive(Debug)]
#[cfg_attr(feature = "dev-context-only-utils", derive(Clone))]
pub struct TransactionConfigValues {
    pub priority_fee_lamports: u64,
    pub compute_unit_limit: u32,
    pub loaded_accounts_data_size_limit: u32,
    pub requested_heap_size: u32,
}

#[derive(Debug)]
#[cfg_attr(feature = "dev-context-only-utils", derive(Clone))]
pub enum TransactionConfigSource {
    LegacyAndV0(ComputeBudgetInstructionDetails),
    V1(TransactionConfigValues),
}

impl TransactionConfigValues {
    fn sanitize_and_convert_to_compute_budget_limits(&self) -> Result<ComputeBudgetLimits> {
        if self.compute_unit_limit > MAX_COMPUTE_UNIT_LIMIT {
            return Err(TransactionError::SanitizeFailure);
        }

        if self.loaded_accounts_data_size_limit > MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES.into() {
            return Err(TransactionError::SanitizeFailure);
        }

        if !(MIN_HEAP_FRAME_BYTES..=MAX_HEAP_FRAME_BYTES).contains(&self.requested_heap_size)
            || !self.requested_heap_size.is_multiple_of(1024)
        {
            return Err(TransactionError::SanitizeFailure);
        }

        Ok(ComputeBudgetLimits {
            updated_heap_bytes: self.requested_heap_size,
            compute_unit_limit: self.compute_unit_limit,
            compute_unit_price: self.priority_fee_lamports,
            loaded_accounts_bytes: NonZeroU32::new(self.loaded_accounts_data_size_limit)
                .ok_or(TransactionError::InvalidLoadedAccountsDataSizeLimit)?,
        })
    }
}

impl TransactionConfigSource {
    pub fn sanitize_and_convert_to_compute_budget_limits(
        &self,
        feature_set: &FeatureSet,
    ) -> Result<ComputeBudgetLimits> {
        match self {
            TransactionConfigSource::LegacyAndV0(details) => {
                details.sanitize_and_convert_to_compute_budget_limits(feature_set)
            }
            TransactionConfigSource::V1(config) => {
                config.sanitize_and_convert_to_compute_budget_limits()
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_compute_budget_instruction::compute_budget_instruction_details::ComputeBudgetInstructionDetails,
        solana_keypair::Keypair,
        solana_signer::Signer,
        solana_svm_transaction::svm_message::SVMStaticMessage,
        solana_transaction::{Message, Transaction, sanitized::SanitizedTransaction},
    };

    fn valid_v1_config() -> TransactionConfigValues {
        TransactionConfigValues {
            priority_fee_lamports: 123,
            compute_unit_limit: MAX_COMPUTE_UNIT_LIMIT,
            loaded_accounts_data_size_limit: 1,
            requested_heap_size: MIN_HEAP_FRAME_BYTES,
        }
    }

    #[test]
    fn test_v1_sanitize_and_convert_success() {
        let config = valid_v1_config();

        let limits = config
            .sanitize_and_convert_to_compute_budget_limits()
            .unwrap();

        assert_eq!(limits.updated_heap_bytes, config.requested_heap_size);
        assert_eq!(limits.compute_unit_limit, config.compute_unit_limit);
        assert_eq!(limits.compute_unit_price, config.priority_fee_lamports);
        assert_eq!(
            limits.loaded_accounts_bytes,
            NonZeroU32::new(config.loaded_accounts_data_size_limit).unwrap()
        );
    }

    #[test]
    fn test_v1_rejects_compute_unit_limit_too_large() {
        let mut config = valid_v1_config();
        config.compute_unit_limit = MAX_COMPUTE_UNIT_LIMIT.saturating_add(1);

        assert_eq!(
            config.sanitize_and_convert_to_compute_budget_limits(),
            Err(TransactionError::SanitizeFailure)
        );
    }

    #[test]
    fn test_v1_rejects_loaded_accounts_data_size_limit_too_large() {
        let mut config = valid_v1_config();
        config.loaded_accounts_data_size_limit =
            u32::from(MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES).saturating_add(1);

        assert_eq!(
            config.sanitize_and_convert_to_compute_budget_limits(),
            Err(TransactionError::SanitizeFailure)
        );
    }

    #[test]
    fn test_v1_rejects_heap_size_below_min() {
        let mut config = valid_v1_config();
        config.requested_heap_size = MIN_HEAP_FRAME_BYTES.saturating_sub(1024);

        assert_eq!(
            config.sanitize_and_convert_to_compute_budget_limits(),
            Err(TransactionError::SanitizeFailure)
        );
    }

    #[test]
    fn test_v1_rejects_heap_size_above_max() {
        let mut config = valid_v1_config();
        config.requested_heap_size = MAX_HEAP_FRAME_BYTES.saturating_add(1024);

        assert_eq!(
            config.sanitize_and_convert_to_compute_budget_limits(),
            Err(TransactionError::SanitizeFailure)
        );
    }

    #[test]
    fn test_v1_rejects_heap_size_not_multiple_of_1024() {
        let mut config = valid_v1_config();
        config.requested_heap_size = MIN_HEAP_FRAME_BYTES + 1;

        assert_eq!(
            config.sanitize_and_convert_to_compute_budget_limits(),
            Err(TransactionError::SanitizeFailure)
        );
    }

    #[test]
    fn test_v1_rejects_zero_loaded_accounts_data_size_limit() {
        let mut config = valid_v1_config();
        config.loaded_accounts_data_size_limit = 0;

        assert_eq!(
            config.sanitize_and_convert_to_compute_budget_limits(),
            Err(TransactionError::InvalidLoadedAccountsDataSizeLimit)
        );
    }

    #[test]
    fn test_source_v1_delegates_successfully() {
        let feature_set = FeatureSet::all_enabled();
        let source = TransactionConfigSource::V1(valid_v1_config());

        let limits = source
            .sanitize_and_convert_to_compute_budget_limits(&feature_set)
            .unwrap();

        assert_eq!(limits.updated_heap_bytes, MIN_HEAP_FRAME_BYTES);
        assert_eq!(limits.compute_unit_limit, MAX_COMPUTE_UNIT_LIMIT);
        assert_eq!(limits.compute_unit_price, 123);
        assert_eq!(limits.loaded_accounts_bytes, NonZeroU32::new(1).unwrap());
    }

    #[test]
    fn test_source_v1_delegates_error() {
        let feature_set = FeatureSet::all_enabled();
        let mut config = valid_v1_config();
        config.compute_unit_limit = MAX_COMPUTE_UNIT_LIMIT.saturating_add(1);

        let source = TransactionConfigSource::V1(config);

        assert_eq!(
            source.sanitize_and_convert_to_compute_budget_limits(&feature_set),
            Err(TransactionError::SanitizeFailure)
        );
    }

    #[test]
    fn test_source_legacy_and_v0_delegates_successfully() {
        let feature_set = FeatureSet::all_enabled();

        let instructions = vec![
            solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_limit(
                MAX_COMPUTE_UNIT_LIMIT,
            ),
            solana_compute_budget_interface::ComputeBudgetInstruction::set_compute_unit_price(123),
            solana_compute_budget_interface::ComputeBudgetInstruction::request_heap_frame(
                MIN_HEAP_FRAME_BYTES,
            ),
            solana_compute_budget_interface::ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(1),
        ];
        let payer_keypair = Keypair::new();
        let tx = SanitizedTransaction::from_transaction_for_tests(Transaction::new_unsigned(
            Message::new(&instructions, Some(&payer_keypair.pubkey())),
        ));

        let details = ComputeBudgetInstructionDetails::try_from(
            SVMStaticMessage::program_instructions_iter(&tx),
        )
        .unwrap();

        let expected = details
            .sanitize_and_convert_to_compute_budget_limits(&feature_set)
            .unwrap();

        let source = TransactionConfigSource::LegacyAndV0(details);

        let actual = source
            .sanitize_and_convert_to_compute_budget_limits(&feature_set)
            .unwrap();

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_source_legacy_and_v0_delegates_error() {
        let feature_set = FeatureSet::all_enabled();

        let instructions = vec![
            solana_compute_budget_interface::ComputeBudgetInstruction::request_heap_frame(
                MAX_HEAP_FRAME_BYTES.saturating_add(1024),
            ),
        ];

        let payer_keypair = Keypair::new();
        let tx = SanitizedTransaction::from_transaction_for_tests(Transaction::new_unsigned(
            Message::new(&instructions, Some(&payer_keypair.pubkey())),
        ));

        let details = ComputeBudgetInstructionDetails::try_from(
            SVMStaticMessage::program_instructions_iter(&tx),
        )
        .unwrap();

        let expected = details.sanitize_and_convert_to_compute_budget_limits(&feature_set);

        let source = TransactionConfigSource::LegacyAndV0(details);

        let actual = source.sanitize_and_convert_to_compute_budget_limits(&feature_set);

        assert_eq!(actual, expected);
        assert!(actual.is_err());
    }
}
