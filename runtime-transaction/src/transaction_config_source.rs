use {
    agave_feature_set::FeatureSet,
    solana_compute_budget::compute_budget_limits::{
        DEFAULT_HEAP_COST, MAX_COMPUTE_UNIT_LIMIT, MAX_HEAP_FRAME_BYTES,
        MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES, MIN_HEAP_FRAME_BYTES,
    },
    solana_compute_budget_instruction::compute_budget_instruction_details::ComputeBudgetInstructionDetails,
    solana_fee_structure::FeeBudgetLimits,
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
    fn sanitize_and_convert_to_fee_budget_limits(&self) -> Result<FeeBudgetLimits> {
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

        Ok(FeeBudgetLimits {
            loaded_accounts_data_size_limit: NonZeroU32::new(self.loaded_accounts_data_size_limit)
                .ok_or(TransactionError::InvalidLoadedAccountsDataSizeLimit)?,
            heap_cost: DEFAULT_HEAP_COST,
            compute_unit_limit: u64::from(self.compute_unit_limit),
            prioritization_fee: self.priority_fee_lamports,
        })
    }
}

impl TransactionConfigSource {
    pub fn sanitize_and_convert_to_fee_budget_limits(
        &self,
        feature_set: &FeatureSet,
    ) -> Result<FeeBudgetLimits> {
        match self {
            TransactionConfigSource::LegacyAndV0(details) => {
                let compute_budget_limits =
                    details.sanitize_and_convert_to_compute_budget_limits(feature_set)?;
                Ok(compute_budget_limits.into())
            }
            TransactionConfigSource::V1(config) => {
                config.sanitize_and_convert_to_fee_budget_limits()
            }
        }
    }

    pub fn sanitize_and_convert_to_fee_budget_limits_and_requested_heap_size(
        &self,
        feature_set: &FeatureSet,
    ) -> Result<(FeeBudgetLimits, u32)> {
        match self {
            TransactionConfigSource::LegacyAndV0(details) => {
                let compute_budget_limits =
                    details.sanitize_and_convert_to_compute_budget_limits(feature_set)?;
                Ok((
                    compute_budget_limits.into(),
                    compute_budget_limits.updated_heap_bytes,
                ))
            }
            TransactionConfigSource::V1(config) => {
                let fee_budget_limits = config.sanitize_and_convert_to_fee_budget_limits()?;
                Ok((fee_budget_limits, config.requested_heap_size))
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

        let limits = config.sanitize_and_convert_to_fee_budget_limits().unwrap();

        assert_eq!(limits.compute_unit_limit, config.compute_unit_limit as u64);
        assert_eq!(limits.prioritization_fee, config.priority_fee_lamports);
        assert_eq!(
            limits.loaded_accounts_data_size_limit,
            NonZeroU32::new(config.loaded_accounts_data_size_limit).unwrap()
        );
    }

    #[test]
    fn test_v1_rejects_compute_unit_limit_too_large() {
        let mut config = valid_v1_config();
        config.compute_unit_limit = MAX_COMPUTE_UNIT_LIMIT.saturating_add(1);

        assert!(matches!(
            config.sanitize_and_convert_to_fee_budget_limits(),
            Err(TransactionError::SanitizeFailure)
        ));
    }

    #[test]
    fn test_v1_rejects_loaded_accounts_data_size_limit_too_large() {
        let mut config = valid_v1_config();
        config.loaded_accounts_data_size_limit =
            u32::from(MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES).saturating_add(1);

        assert!(matches!(
            config.sanitize_and_convert_to_fee_budget_limits(),
            Err(TransactionError::SanitizeFailure)
        ));
    }

    #[test]
    fn test_v1_rejects_heap_size_below_min() {
        let mut config = valid_v1_config();
        config.requested_heap_size = MIN_HEAP_FRAME_BYTES.saturating_sub(1024);

        assert!(matches!(
            config.sanitize_and_convert_to_fee_budget_limits(),
            Err(TransactionError::SanitizeFailure)
        ));
    }

    #[test]
    fn test_v1_rejects_heap_size_above_max() {
        let mut config = valid_v1_config();
        config.requested_heap_size = MAX_HEAP_FRAME_BYTES.saturating_add(1024);

        assert!(matches!(
            config.sanitize_and_convert_to_fee_budget_limits(),
            Err(TransactionError::SanitizeFailure)
        ));
    }

    #[test]
    fn test_v1_rejects_heap_size_not_multiple_of_1024() {
        let mut config = valid_v1_config();
        config.requested_heap_size = MIN_HEAP_FRAME_BYTES + 1;

        assert!(matches!(
            config.sanitize_and_convert_to_fee_budget_limits(),
            Err(TransactionError::SanitizeFailure)
        ));
    }

    #[test]
    fn test_v1_rejects_zero_loaded_accounts_data_size_limit() {
        let mut config = valid_v1_config();
        config.loaded_accounts_data_size_limit = 0;

        assert!(matches!(
            config.sanitize_and_convert_to_fee_budget_limits(),
            Err(TransactionError::InvalidLoadedAccountsDataSizeLimit)
        ));
    }

    #[test]
    fn test_source_v1_delegates_successfully() {
        let feature_set = FeatureSet::all_enabled();
        let source = TransactionConfigSource::V1(valid_v1_config());

        let (limits, updated_heap_bytes) = source
            .sanitize_and_convert_to_fee_budget_limits_and_requested_heap_size(&feature_set)
            .unwrap();

        assert_eq!(updated_heap_bytes, MIN_HEAP_FRAME_BYTES);
        assert_eq!(limits.compute_unit_limit, MAX_COMPUTE_UNIT_LIMIT as u64);
        assert_eq!(limits.prioritization_fee, 123);
        assert_eq!(
            limits.loaded_accounts_data_size_limit,
            NonZeroU32::new(1).unwrap()
        );
    }

    #[test]
    fn test_source_v1_delegates_error() {
        let feature_set = FeatureSet::all_enabled();
        let mut config = valid_v1_config();
        config.compute_unit_limit = MAX_COMPUTE_UNIT_LIMIT.saturating_add(1);

        let source = TransactionConfigSource::V1(config);

        assert!(
            source
                .sanitize_and_convert_to_fee_budget_limits(&feature_set)
                .is_err()
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

        let expected: FeeBudgetLimits = details
            .sanitize_and_convert_to_compute_budget_limits(&feature_set)
            .unwrap()
            .into();

        let source = TransactionConfigSource::LegacyAndV0(details);

        let actual = source
            .sanitize_and_convert_to_fee_budget_limits(&feature_set)
            .unwrap();

        assert_eq!(
            actual.loaded_accounts_data_size_limit,
            expected.loaded_accounts_data_size_limit
        );
        assert_eq!(actual.heap_cost, expected.heap_cost);
        assert_eq!(actual.compute_unit_limit, expected.compute_unit_limit);
        assert_eq!(actual.prioritization_fee, expected.prioritization_fee);
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
        let source = TransactionConfigSource::LegacyAndV0(details);

        let actual = source.sanitize_and_convert_to_fee_budget_limits(&feature_set);

        assert!(actual.is_err());
    }
}
