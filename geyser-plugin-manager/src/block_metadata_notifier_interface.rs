use {
    solana_clock::UnixTimestamp, solana_runtime::bank::KeyedRewardsAndNumPartitions, std::sync::Arc,
};

/// Interface for notifying block metadata changes
pub trait BlockMetadataNotifier {
    /// Notify the block metadata
    #[allow(clippy::too_many_arguments)]
    fn notify_block_metadata(
        &self,
        parent_slot: u64,
        parent_blockhash: &str,
        slot: u64,
        blockhash: &str,
        rewards: &KeyedRewardsAndNumPartitions,
        block_time: Option<UnixTimestamp>,
        block_height: Option<u64>,
        executed_transaction_count: u64,
        entry_count: u64,
        commission_rate_in_basis_points: bool,
    );
}

pub type BlockMetadataNotifierArc = Arc<dyn BlockMetadataNotifier + Sync + Send>;
