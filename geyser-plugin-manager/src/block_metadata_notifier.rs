use {
    crate::{
        block_metadata_notifier_interface::BlockMetadataNotifier,
        geyser_plugin_manager::GeyserPluginManager,
    },
    agave_geyser_plugin_interface::{
        geyser_plugin_interface::{ReplicaBlockInfoV5, ReplicaBlockInfoVersions},
        transaction_status_meta::{Reward, RewardsAndNumPartitions},
    },
    arc_swap::ArcSwap,
    log::*,
    solana_clock::{BankId, UnixTimestamp},
    solana_runtime::bank::KeyedRewardsAndNumPartitions,
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

        // Scratch buffers owned by this call; only slices into them cross
        // the plugin boundary.
        let reward_pubkeys: Vec<String> = rewards
            .keyed_rewards
            .iter()
            .map(|(pubkey, _)| pubkey.to_string())
            .collect();
        let mirror_rewards: Vec<Reward> =
            Self::build_rewards(rewards, &reward_pubkeys, commission_rate_in_basis_points);
        let rewards = RewardsAndNumPartitions {
            rewards: &mirror_rewards,
            num_partitions: rewards.num_partitions,
        };
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
            let block_info = ReplicaBlockInfoVersions::V0_0_5(&block_info);
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
    fn build_rewards<'a>(
        rewards: &KeyedRewardsAndNumPartitions,
        reward_pubkeys: &'a [String],
        commission_rate_in_basis_points: bool,
    ) -> Vec<Reward<'a>> {
        rewards
            .keyed_rewards
            .iter()
            .zip(reward_pubkeys)
            .map(|((_, reward), pubkey)| Reward {
                pubkey,
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
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn build_replica_block_info<'a>(
        parent_slot: u64,
        parent_blockhash: &'a str,
        slot: u64,
        blockhash: &'a str,
        rewards: &'a RewardsAndNumPartitions<'a>,
        block_time: Option<UnixTimestamp>,
        block_height: Option<u64>,
        executed_transaction_count: u64,
        entry_count: u64,
    ) -> ReplicaBlockInfoV5<'a> {
        ReplicaBlockInfoV5 {
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
        agave_geyser_plugin_interface::geyser_plugin_interface::{
            GeyserPlugin, GeyserPluginError, Result,
        },
        arc_swap::ArcSwap,
        libloading::Library,
        solana_accounts_db::stake_rewards::StakeRewardInfo,
        solana_pubkey::Pubkey,
        solana_reward_info::RewardType,
        std::sync::{Arc, Mutex},
    };

    type BlockMetadataUpdate = (u64, BankId, u64, u64);

    #[derive(Debug)]
    struct TestBlockMetadataPlugin {
        updates: Arc<Mutex<Vec<BlockMetadataUpdate>>>,
        rewards_debug: Arc<Mutex<Vec<String>>>,
        fail: bool,
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
            let ReplicaBlockInfoVersions::V0_0_5(blockinfo) = blockinfo;
            self.updates.lock().unwrap().push((
                blockinfo.slot,
                bank_id,
                blockinfo.executed_transaction_count,
                blockinfo.entry_count,
            ));
            self.rewards_debug
                .lock()
                .unwrap()
                .push(format!("{:?}", blockinfo.rewards));
            if self.fail {
                return Err(GeyserPluginError::Custom(Box::new(std::io::Error::other(
                    "boom",
                ))));
            }
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
                rewards_debug: Arc::new(Mutex::new(Vec::new())),
                fail: false,
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

    #[test]
    fn converts_rewards_reports_errors_and_skips_empty() {
        // Empty plugin set: dispatch returns before building anything.
        let empty = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
            plugins: vec![],
        })));
        let notifier = BlockMetadataNotifierImpl::new(empty);
        let no_rewards = KeyedRewardsAndNumPartitions {
            keyed_rewards: vec![],
            num_partitions: None,
        };
        notifier.notify_block_metadata(0, "p", 1, 1, "b", &no_rewards, None, None, 0, 0, false);

        // A failing plugin first, a recording plugin second: the error is
        // logged and must not stop delivery to the second plugin.
        let fail_updates = Arc::new(Mutex::new(Vec::new()));
        let fail_rewards_debug = Arc::new(Mutex::new(Vec::new()));
        let updates = Arc::new(Mutex::new(Vec::new()));
        let rewards_debug = Arc::new(Mutex::new(Vec::new()));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
            plugins: vec![
                loaded_test_plugin(TestBlockMetadataPlugin {
                    updates: fail_updates.clone(),
                    rewards_debug: fail_rewards_debug.clone(),
                    fail: true,
                }),
                loaded_test_plugin(TestBlockMetadataPlugin {
                    updates: updates.clone(),
                    rewards_debug: rewards_debug.clone(),
                    fail: false,
                }),
            ],
        })));
        let notifier = BlockMetadataNotifierImpl::new(plugin_manager);

        let pk = Pubkey::new_unique();
        // RewardInfo lives in a private runtime module; construct it through
        // the Vec's element type via From<StakeRewardInfo>.
        let mut keyed = KeyedRewardsAndNumPartitions {
            keyed_rewards: vec![],
            num_partitions: Some(2),
        };
        keyed.keyed_rewards.push((
            pk,
            StakeRewardInfo {
                reward_type: RewardType::Staking,
                lamports: 5,
                post_balance: 100,
                commission_bps: Some(300),
            }
            .into(),
        ));
        notifier.notify_block_metadata(41, "ph", 42, 9, "bh", &keyed, None, None, 7, 3, false);
        notifier.notify_block_metadata(42, "ph", 43, 9, "bh", &keyed, None, None, 7, 3, true);

        let pk_s = pk.to_string();
        let expected_pct = format!(
            "{:?}",
            RewardsAndNumPartitions {
                rewards: &[Reward {
                    pubkey: &pk_s,
                    lamports: 5,
                    post_balance: 100,
                    reward_type: Some(RewardType::Staking),
                    commission: Some(3),
                    commission_bps: None,
                }],
                num_partitions: Some(2),
            }
        );
        let expected_bps = format!(
            "{:?}",
            RewardsAndNumPartitions {
                rewards: &[Reward {
                    pubkey: &pk_s,
                    lamports: 5,
                    post_balance: 100,
                    reward_type: Some(RewardType::Staking),
                    commission: None,
                    commission_bps: Some(300),
                }],
                num_partitions: Some(2),
            }
        );
        assert_eq!(
            *rewards_debug.lock().unwrap(),
            vec![expected_pct.clone(), expected_bps.clone()]
        );
        assert_eq!(
            *fail_rewards_debug.lock().unwrap(),
            vec![expected_pct, expected_bps]
        );
        assert_eq!(updates.lock().unwrap().len(), 2);
        assert_eq!(fail_updates.lock().unwrap().len(), 2);
    }
}
