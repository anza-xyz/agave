use {
    crate::{
        block_metadata_notifier_interface::BlockMetadataNotifier,
        geyser_plugin_manager::GeyserPluginManager,
    },
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReplicaBlockInfoV4, ReplicaBlockInfoVersions, ReplicaUpdateParentInfo,
        ReplicaUpdateParentInfoVersions,
    },
    arc_swap::ArcSwap,
    log::*,
    solana_clock::UnixTimestamp,
    solana_ledger::blockstore::UpdateParentSignal,
    solana_runtime::bank::KeyedRewardsAndNumPartitions,
    solana_transaction_status::{Reward, RewardsAndNumPartitions},
    std::sync::Arc,
};

pub(crate) struct BlockMetadataNotifierImpl {
    plugin_manager: Arc<ArcSwap<GeyserPluginManager>>,
}

impl BlockMetadataNotifier for BlockMetadataNotifierImpl {
    /// Notify the block metadata
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
    ) {
        let plugin_manager = self.plugin_manager.load();
        if plugin_manager.plugins.is_empty() {
            return;
        }

        let rewards = Self::build_rewards(rewards, commission_rate_in_basis_points);
        let block_info = Self::build_replica_block_info(
            parent_slot,
            parent_blockhash,
            slot,
            blockhash,
            &rewards,
            block_time,
            block_height,
            executed_transaction_count,
            entry_count,
        );

        for plugin in plugin_manager.plugins.iter() {
            let block_info = ReplicaBlockInfoVersions::V0_0_4(&block_info);
            match plugin.notify_block_metadata(block_info) {
                Err(err) => {
                    error!(
                        "Failed to update block metadata at slot {}, error: {} to plugin {}",
                        slot,
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully updated block metadata at slot {} to plugin {}",
                        slot,
                        plugin.name()
                    );
                }
            }
        }
    }

    fn notify_update_parent(&self, update_parent: &UpdateParentSignal) {
        let plugin_manager = self.plugin_manager.load();
        if plugin_manager.plugins.is_empty() {
            return;
        }

        let update_parent_info = Self::build_replica_update_parent_info(update_parent);
        for plugin in plugin_manager.plugins.iter() {
            match plugin
                .notify_update_parent(ReplicaUpdateParentInfoVersions::V0_0_1(&update_parent_info))
            {
                Err(err) => error!(
                    "Failed to notify block metadata UpdateParent, error: ({}) to plugin {}",
                    err,
                    plugin.name()
                ),
                Ok(_) => trace!(
                    "Successfully notified block metadata UpdateParent to plugin {}",
                    plugin.name()
                ),
            }
        }
    }
}

impl BlockMetadataNotifierImpl {
    fn build_rewards(
        rewards: &KeyedRewardsAndNumPartitions,
        commission_rate_in_basis_points: bool,
    ) -> RewardsAndNumPartitions {
        RewardsAndNumPartitions {
            rewards: rewards
                .keyed_rewards
                .iter()
                .map(|(pubkey, reward)| Reward {
                    pubkey: pubkey.to_string(),
                    lamports: reward.lamports,
                    post_balance: reward.post_balance,
                    reward_type: Some(reward.reward_type),
                    commission: if commission_rate_in_basis_points {
                        None
                    } else {
                        reward.commission_bps.map(|bps| (bps / 100) as u8)
                    },
                    commission_bps: if commission_rate_in_basis_points {
                        reward.commission_bps
                    } else {
                        None
                    },
                })
                .collect(),
            num_partitions: rewards.num_partitions,
        }
    }

    fn build_replica_block_info<'a>(
        parent_slot: u64,
        parent_blockhash: &'a str,
        slot: u64,
        blockhash: &'a str,
        rewards: &'a RewardsAndNumPartitions,
        block_time: Option<UnixTimestamp>,
        block_height: Option<u64>,
        executed_transaction_count: u64,
        entry_count: u64,
    ) -> ReplicaBlockInfoV4<'a> {
        ReplicaBlockInfoV4 {
            parent_slot,
            parent_blockhash,
            slot,
            blockhash,
            rewards,
            block_time,
            block_height,
            executed_transaction_count,
            entry_count,
        }
    }

    fn build_replica_update_parent_info(
        update_parent: &UpdateParentSignal,
    ) -> ReplicaUpdateParentInfo<'_> {
        ReplicaUpdateParentInfo {
            slot: update_parent.slot,
            update_parent_fec_set_index: update_parent.update_parent_fec_set_index,
            parent_slot: update_parent.parent_slot,
            parent_block_id: update_parent.parent_block_id.as_ref(),
        }
    }

    pub fn new(plugin_manager: Arc<ArcSwap<GeyserPluginManager>>) -> Self {
        Self { plugin_manager }
    }
}
