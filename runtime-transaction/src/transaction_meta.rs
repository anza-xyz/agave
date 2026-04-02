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

/// metadata can be extracted statically from sanitized transaction,
/// for example: message hash, simple-vote-tx flag, limits set by instructions
pub trait StaticMeta {
    fn message_hash(&self) -> &Hash;
    fn is_simple_vote_transaction(&self) -> bool;
    fn signature_details(&self) -> &TransactionSignatureDetails;
    fn compute_budget_limits(
        &self,
        feature_set: &FeatureSet,
    ) -> Result<ComputeBudgetLimits, TransactionError>;
    fn instruction_data_len(&self) -> u16;
}

/// Statically loaded meta is a supertrait of Dynamically loaded meta, when
/// transaction transited successfully into dynamically loaded, it should
/// have both meta data populated and available.
/// Dynamic metadata available after accounts addresses are loaded from
/// on-chain ALT, examples are: transaction usage costs, nonce account.
pub trait DynamicMeta: StaticMeta {}

#[cfg_attr(feature = "dev-context-only-utils", derive(Clone))]
#[derive(Debug)]
pub struct TransactionMeta {
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
    pub loaded_accounts_bytes: NonZeroU32,
}

impl From<ComputeBudgetLimits> for TransactionConfiguration {
    fn from(compute_budget_limits: ComputeBudgetLimits) -> Self {
        let prioritization_fee = compute_budget_limits.get_prioritization_fee();
        TransactionConfiguration {
            updated_heap_bytes: compute_budget_limits.updated_heap_bytes,
            compute_unit_limit: compute_budget_limits.compute_unit_limit,
            prioritization_fee,
            loaded_accounts_bytes: compute_budget_limits.loaded_accounts_bytes,
        }
    }
}
