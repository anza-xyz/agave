use {
    crate::{
        block_metadata_notifier_interface::BlockMetadataNotifier,
        geyser_plugin_manager::GeyserPluginManager,
    },
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        ReplicaBlockInfoV4, ReplicaBlockInfoVersions,
    },
    arc_swap::ArcSwap,
    log::*,
    solana_clock::{BankId, UnixTimestamp},
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
        bank_id: BankId,
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
            match plugin.notify_block_metadata_for_bank(block_info, bank_id) {
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

    #[allow(clippy::too_many_arguments)]
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

    pub fn new(plugin_manager: Arc<ArcSwap<GeyserPluginManager>>) -> Self {
        Self { plugin_manager }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::geyser_plugin_manager::{GeyserPluginManager, LoadedGeyserPlugin},
        agave_geyser_plugin_interface::geyser_plugin_interface::{GeyserPlugin, Result},
        arc_swap::ArcSwap,
        libloading::Library,
        std::sync::{Arc, Mutex},
    };

    type BlockMetadataUpdate = (u64, BankId, u64, u64);

    #[derive(Debug)]
    struct TestBlockMetadataPlugin {
        updates: Arc<Mutex<Vec<BlockMetadataUpdate>>>,
    }

    impl GeyserPlugin for TestBlockMetadataPlugin {
        fn name(&self) -> &'static str {
            "test-block-metadata-plugin"
        }

        fn notify_block_metadata_for_bank(
            &self,
            blockinfo: ReplicaBlockInfoVersions,
            bank_id: BankId,
        ) -> Result<()> {
            let ReplicaBlockInfoVersions::V0_0_4(blockinfo) = blockinfo else {
                panic!("expected V0_0_4 block info");
            };
            self.updates.lock().unwrap().push((
                blockinfo.slot,
                bank_id,
                blockinfo.executed_transaction_count,
                blockinfo.entry_count,
            ));
            Ok(())
        }
    }

    fn loaded_test_plugin(plugin: TestBlockMetadataPlugin) -> Arc<LoadedGeyserPlugin> {
        #[cfg(unix)]
        let library = libloading::os::unix::Library::this();
        #[cfg(windows)]
        let library = libloading::os::windows::Library::this().unwrap();

        Arc::new(LoadedGeyserPlugin::new(
            Library::from(library),
            Box::new(plugin),
            None,
        ))
    }

    #[test]
    fn test_notify_block_metadata_includes_bank_id() {
        let updates = Arc::new(Mutex::new(Vec::new()));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
            plugins: vec![loaded_test_plugin(TestBlockMetadataPlugin {
                updates: updates.clone(),
            })],
        })));
        let notifier = BlockMetadataNotifierImpl::new(plugin_manager);
        let rewards = KeyedRewardsAndNumPartitions {
            keyed_rewards: Vec::new(),
            num_partitions: None,
        };

        notifier.notify_block_metadata(
            41,
            "parent-blockhash",
            42,
            9,
            "blockhash",
            &rewards,
            Some(123),
            Some(10),
            7,
            3,
            false,
        );

        assert_eq!(*updates.lock().unwrap(), vec![(42, 9, 7, 3)]);
    }
}
