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
    solana_compute_budget::compute_budget_limits::ComputeBudgetLimits,
    solana_compute_budget_instruction::compute_budget_instruction_details::ComputeBudgetInstructionDetails,
    solana_hash::Hash, solana_message::TransactionSignatureDetails,
    solana_transaction::TransactionError, std::num::NonZeroU32,
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
pub struct CachedTransactionMeta {
    pub(crate) message_hash: Hash,
    pub(crate) is_simple_vote_transaction: bool,
    pub(crate) signature_details: TransactionSignatureDetails,
    pub(crate) compute_budget_instruction_details: ComputeBudgetInstructionDetails,
    pub(crate) instruction_data_len: u16,
}

pub struct TransactionConfiguration {
    pub updated_heap_bytes: u32,
    pub compute_unit_limit: u32,
    pub prioritization_fee: u64,
    pub loaded_accounts_data_size_limit: NonZeroU32,
}

impl From<ComputeBudgetLimits> for TransactionConfiguration {
    fn from(compute_budget_limits: ComputeBudgetLimits) -> Self {
        let prioritization_fee = compute_budget_limits.get_prioritization_fee();
        TransactionConfiguration {
            updated_heap_bytes: compute_budget_limits.updated_heap_bytes,
            compute_unit_limit: compute_budget_limits.compute_unit_limit,
            prioritization_fee,
            loaded_accounts_data_size_limit: compute_budget_limits.loaded_accounts_bytes,
        }
    }
}

impl TransactionConfiguration {
    /// Compute the compute unit price in micro-lamports per compute unit.
    ///
    /// Note: that this will return an effective price according to the actual
    /// fee paid - i.e. legacy/v0 transactions that have fees rounded up will
    /// return a higher value here than their specified cu_price in the
    /// compute budget instruction.
    pub fn compute_unit_price_in_microlamports(&self) -> u64 {
        self.prioritization_fee
            .saturating_mul(1_000_000)
            .checked_div(self.compute_unit_limit as u64)
            .unwrap_or(0)
    }
}
