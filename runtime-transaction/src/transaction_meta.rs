//! Transaction Meta contains data that follows a transaction through the
//! execution pipeline in runtime. Examples of metadata could be limits
//! specified by compute-budget instructions, simple-vote flag, transaction
//! costs, durable nonce account etc;
//!
//! The premise is if anything qualifies as metadata, then it must be valid
//! and available as long as the transaction itself is valid and available.
//! Hence they are not Option<T> type. Their visibility at different states
//! are defined in traits.
//!
//! The StaticMeta and DynamicMeta traits are accessor traits on the
//! RuntimeTransaction types, not the TransactionMeta itself.
//!
use {
    agave_feature_set::FeatureSet,
    solana_compute_budget::compute_budget_limits::{
        MAX_COMPUTE_UNIT_LIMIT, MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES,
    },
    solana_compute_budget_instruction::compute_budget_instruction_details::ComputeBudgetInstructionDetails,
    solana_hash::Hash, solana_message::TransactionSignatureDetails,
    solana_transaction::TransactionError,
};

pub trait TransactionMeta {
    fn message_hash(&self) -> &Hash;
    fn is_simple_vote_transaction(&self) -> bool;
    fn signature_details(&self) -> &TransactionSignatureDetails;
    fn transaction_configuration(
        &self,
        feature_set: &FeatureSet,
    ) -> Result<TransactionConfiguration, TransactionError>;
    fn instruction_data_len(&self) -> u16;
}

#[cfg_attr(feature = "dev-context-only-utils", derive(Clone))]
#[derive(Debug)]
pub(crate) struct CachedTransactionMeta {
    pub(crate) message_hash: Hash,
    pub(crate) is_simple_vote_transaction: bool,
    pub(crate) signature_details: TransactionSignatureDetails,
    pub(crate) versioned_transaction_config: VersionedTransactionConfiguration,
    pub(crate) instruction_data_len: u16,
}

#[cfg_attr(feature = "dev-context-only-utils", derive(Clone))]
#[derive(Debug)]
pub struct TransactionConfiguration {
    pub updated_heap_bytes: u32,
    pub compute_unit_limit: u32,
    pub priority_fee_lamports: u64,
    pub loaded_accounts_data_size_limit: u32,
}

impl TransactionConfiguration {
    /// Compute the compute unit price in micro-lamports per compute unit.
    ///
    /// Note: that this will return an effective price according to the actual
    /// fee paid - i.e. legacy/v0 transactions that have fees rounded up will
    /// return a higher value here than their specified cu_price in the
    /// compute budget instruction.
    pub fn compute_unit_price_in_microlamports(&self) -> u64 {
        (self.priority_fee_lamports as u128)
            .saturating_mul(1_000_000u128)
            .checked_div(self.compute_unit_limit as u128)
            .and_then(|x| u64::try_from(x).ok())
            .unwrap_or(0)
    }
}

#[cfg_attr(feature = "dev-context-only-utils", derive(Clone))]
#[derive(Debug)]
pub(crate) enum VersionedTransactionConfiguration {
    LegacyAndV0(ComputeBudgetInstructionDetails),
    V1(TransactionConfiguration),
}

impl VersionedTransactionConfiguration {
    pub(crate) fn try_into_config(
        &self,
        feature_set: &FeatureSet,
    ) -> Result<TransactionConfiguration, TransactionError> {
        match self {
            Self::LegacyAndV0(compute_budget_instruction_details) => {
                let compute_budget_limits = compute_budget_instruction_details
                    .sanitize_and_convert_to_compute_budget_limits(feature_set)?;
                Ok(TransactionConfiguration {
                    updated_heap_bytes: compute_budget_limits.updated_heap_bytes,
                    compute_unit_limit: compute_budget_limits.compute_unit_limit,
                    priority_fee_lamports: compute_budget_limits.get_prioritization_fee(),
                    loaded_accounts_data_size_limit: compute_budget_limits.loaded_accounts_bytes,
                })
            }
            Self::V1(transaction_configuration) => {
                // NOTE: transaction_configuration is already sanitized in View
                Ok(TransactionConfiguration {
                    updated_heap_bytes: transaction_configuration.updated_heap_bytes,
                    compute_unit_limit: transaction_configuration
                        .compute_unit_limit
                        .min(MAX_COMPUTE_UNIT_LIMIT),
                    priority_fee_lamports: transaction_configuration.priority_fee_lamports,
                    loaded_accounts_data_size_limit: transaction_configuration
                        .loaded_accounts_data_size_limit
                        .min(MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES),
                })
            }
        }
    }
}
