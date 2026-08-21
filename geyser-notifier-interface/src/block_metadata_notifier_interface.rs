use {
    solana_clock::{BankId, UnixTimestamp},
    solana_pubkey::Pubkey,
    solana_reward_info::RewardType,
    std::sync::Arc,
};

/// Reward earned by an account in a block, as delivered to
/// [`BlockMetadataNotifier`].
///
/// Mirrors the validator-internal reward representation
/// (`solana_runtime::RewardInfo`) field-for-field so the
/// conversion at the notification call site is trivial, without this crate
/// depending on the runtime.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockRewardInfo {
    pub reward_type: RewardType,
    pub lamports: i64,
    pub post_balance: u64,
    pub commission_bps: Option<u16>,
}

/// Interface for notifying block metadata changes
pub trait BlockMetadataNotifier {
    /// Notify the block metadata
    #[allow(clippy::too_many_arguments)]
    fn notify_block_metadata(
        &self,
        parent_slot: u64,
        parent_blockhash: &str,
        slot: u64,
        bank_id: BankId,
        blockhash: &str,
        keyed_rewards: &[(Pubkey, BlockRewardInfo)],
        num_partitions: Option<u64>,
        block_time: Option<UnixTimestamp>,
        block_height: Option<u64>,
        executed_transaction_count: u64,
        entry_count: u64,
        commission_rate_in_basis_points: bool,
    );
}

pub type BlockMetadataNotifierArc = Arc<dyn BlockMetadataNotifier + Sync + Send>;
