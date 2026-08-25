/// Module responsible for notifying plugins about entries
use {
    agave_geyser_plugin_host::GeyserPluginManager,
    agave_geyser_plugin_interface::{
        block_footer,
        geyser_plugin_interface::{
            ReplicaBlockFooterInfo, ReplicaBlockFooterInfoVersions, ReplicaEntryInfoV2,
            ReplicaEntryInfoVersions, ReplicaEntryUpdateParentInfo,
            ReplicaEntryUpdateParentInfoVersions,
        },
    },
    arc_swap::ArcSwap,
    log::*,
    solana_clock::{BankId, Slot},
    solana_entry::{block_component, entry::EntrySummary},
    solana_ledger::entry_notifier_interface::{EntryNotifier, EntryUpdateParentInfo},
    std::sync::Arc,
};

/// Converts the internal block footer into the plugin interface mirror.
///
/// The mirror borrows all variable-length data from the internal footer, so
/// the conversion allocates nothing. The exhaustive destructuring below is
/// deliberate: if a field is added to any of the internal types, this stops
/// compiling instead of silently dropping data on the plugin boundary.
fn convert_block_footer(
    footer: &block_component::VersionedBlockFooter,
) -> block_footer::VersionedBlockFooter<'_> {
    match footer {
        block_component::VersionedBlockFooter::V1(block_component::BlockFooterV1 {
            bank_hash,
            block_producer_time_nanos,
            block_user_agent,
            block_final_cert,
            skip_reward_cert,
            notar_reward_cert,
        }) => block_footer::VersionedBlockFooter::V1(block_footer::BlockFooterV1 {
            bank_hash: *bank_hash,
            block_producer_time_nanos: *block_producer_time_nanos,
            block_user_agent,
            block_final_cert: block_final_cert.as_ref().map(convert_finalization_cert),
            skip_reward_cert: skip_reward_cert.as_ref().map(|cert| {
                let (slot, signature, bitmap) = cert.as_parts();
                block_footer::SkipRewardCertificate {
                    slot,
                    signature: *signature,
                    bitmap,
                }
            }),
            notar_reward_cert: notar_reward_cert.as_ref().map(|cert| {
                let (slot, block_id, signature, bitmap) = cert.as_parts();
                block_footer::NotarRewardCertificate {
                    slot,
                    block_id: *block_id,
                    signature: *signature,
                    bitmap,
                }
            }),
        }),
    }
}

fn convert_finalization_cert(
    cert: &block_component::BlockFinalizationCert,
) -> block_footer::BlockFinalizationCert<'_> {
    let block_component::BlockFinalizationCert {
        slot,
        block_id,
        final_aggregate,
        notar_aggregate,
    } = cert;
    block_footer::BlockFinalizationCert {
        slot: *slot,
        block_id: *block_id,
        final_aggregate: convert_votes_aggregate(final_aggregate),
        notar_aggregate: notar_aggregate.as_ref().map(convert_votes_aggregate),
    }
}

fn convert_votes_aggregate(
    aggregate: &block_component::VotesAggregate,
) -> block_footer::VotesAggregate<'_> {
    let (signature, bitmap) = aggregate.as_parts();
    block_footer::VotesAggregate {
        signature: *signature,
        bitmap,
    }
}

pub(crate) struct EntryNotifierImpl {
    plugin_manager: Arc<ArcSwap<GeyserPluginManager>>,
}

impl EntryNotifier for EntryNotifierImpl {
    fn notify_entry<'a>(
        &'a self,
        slot: Slot,
        bank_id: BankId,
        index: usize,
        entry: &'a EntrySummary,
        starting_transaction_index: usize,
    ) {
        let plugin_manager = self.plugin_manager.load();
        if plugin_manager.plugins().is_empty() {
            return;
        }

        let entry_info =
            Self::build_replica_entry_info(slot, index, entry, starting_transaction_index);

        for plugin in plugin_manager.plugins().iter() {
            if !plugin.entry_notifications_enabled() {
                continue;
            }
            match plugin
                .notify_entry_for_bank(ReplicaEntryInfoVersions::V0_0_2(&entry_info), bank_id)
            {
                Err(err) => {
                    error!(
                        "Failed to notify entry, error: ({}) to plugin {}",
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!("Successfully notified entry to plugin {}", plugin.name());
                }
            }
        }
    }

    fn notify_block_footer(
        &self,
        slot: Slot,
        bank_id: BankId,
        block_footer: &block_component::VersionedBlockFooter,
    ) {
        let plugin_manager = self.plugin_manager.load();
        if plugin_manager.plugins().is_empty() {
            return;
        }

        let block_footer = convert_block_footer(block_footer);
        let block_footer_info = ReplicaBlockFooterInfo {
            slot,
            block_footer: &block_footer,
        };
        for plugin in plugin_manager.plugins().iter() {
            if !plugin.block_footer_notifications_enabled() {
                continue;
            }
            match plugin.notify_block_footer(
                ReplicaBlockFooterInfoVersions::V0_0_2(&block_footer_info),
                bank_id,
            ) {
                Err(err) => error!(
                    "Failed to notify block footer, error: ({}) to plugin {}",
                    err,
                    plugin.name()
                ),
                Ok(_) => trace!(
                    "Successfully notified block footer to plugin {}",
                    plugin.name()
                ),
            }
        }
    }

    fn notify_entry_update_parent(&self, update_parent: &EntryUpdateParentInfo) {
        let plugin_manager = self.plugin_manager.load();
        let update_parent_info = ReplicaEntryUpdateParentInfo {
            slot: update_parent.slot,
            cleared_bank_id: update_parent.cleared_bank_id,
            parent_slot: update_parent.parent_slot,
            parent_block_id: &update_parent.parent_block_id,
        };
        for plugin in plugin_manager.plugins().iter() {
            if plugin.entry_notifications_enabled()
                && let Err(err) = plugin.notify_entry_update_parent(
                    ReplicaEntryUpdateParentInfoVersions::V0_0_1(&update_parent_info),
                )
            {
                error!(
                    "Failed to notify entry UpdateParent, error: ({err}) to plugin {}",
                    plugin.name()
                );
            }
        }
    }
}

impl EntryNotifierImpl {
    pub fn new(plugin_manager: Arc<ArcSwap<GeyserPluginManager>>) -> Self {
        Self { plugin_manager }
    }

    fn build_replica_entry_info(
        slot: Slot,
        index: usize,
        entry: &'_ EntrySummary,
        starting_transaction_index: usize,
    ) -> ReplicaEntryInfoV2<'_> {
        ReplicaEntryInfoV2 {
            slot,
            index,
            num_hashes: entry.num_hashes,
            hash: entry.hash.as_ref(),
            executed_transaction_count: entry.num_transactions,
            starting_transaction_index,
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        agave_geyser_plugin_host::{GeyserPluginManager, LoadedGeyserPlugin},
        agave_geyser_plugin_interface::geyser_plugin_interface::{GeyserPlugin, Result},
        agave_votor_messages::reward_certificate::{NotarRewardCertificate, SkipRewardCertificate},
        arc_swap::ArcSwap,
        libloading::Library,
        solana_bls_signatures::{BLS_SIGNATURE_COMPRESSED_SIZE, SignatureCompressed},
        solana_entry::block_component::{
            BlockFinalizationCert, BlockFooterV1, VersionedBlockFooter, VotesAggregate,
        },
        solana_hash::Hash,
        std::sync::{Arc, Mutex},
    };

    type EntryUpdate = (Slot, BankId, usize, usize);
    type EntryUpdateParent = (Slot, BankId, Slot, Hash);
    // The mirror borrows from the notifying call's stack, so the mock stores
    // an owned Debug snapshot instead of the borrowed struct itself.
    type BlockFooterUpdate = (Slot, BankId, String);

    #[derive(Debug)]
    struct TestEntryPlugin {
        entry_notifications_enabled: bool,
        block_footer_notifications_enabled: bool,
        entry_updates: Arc<Mutex<Vec<EntryUpdate>>>,
        update_parents: Arc<Mutex<Vec<EntryUpdateParent>>>,
        block_footer_updates: Arc<Mutex<Vec<BlockFooterUpdate>>>,
    }

    impl GeyserPlugin for TestEntryPlugin {
        fn name(&self) -> &'static str {
            "test-entry-plugin"
        }

        fn notify_entry_for_bank(
            &self,
            entry: ReplicaEntryInfoVersions,
            bank_id: BankId,
        ) -> Result<()> {
            let ReplicaEntryInfoVersions::V0_0_2(entry) = entry else {
                panic!("expected V0_0_2 entry info");
            };
            self.entry_updates.lock().unwrap().push((
                entry.slot,
                bank_id,
                entry.index,
                entry.starting_transaction_index,
            ));
            Ok(())
        }

        fn notify_block_footer(
            &self,
            block_footer: ReplicaBlockFooterInfoVersions,
            bank_id: BankId,
        ) -> Result<()> {
            let ReplicaBlockFooterInfoVersions::V0_0_2(block_footer) = block_footer;
            self.block_footer_updates.lock().unwrap().push((
                block_footer.slot,
                bank_id,
                format!("{:?}", block_footer.block_footer),
            ));
            Ok(())
        }

        fn notify_entry_update_parent(
            &self,
            update_parent: ReplicaEntryUpdateParentInfoVersions,
        ) -> Result<()> {
            let ReplicaEntryUpdateParentInfoVersions::V0_0_1(update_parent) = update_parent;
            self.update_parents.lock().unwrap().push((
                update_parent.slot,
                update_parent.cleared_bank_id,
                update_parent.parent_slot,
                *update_parent.parent_block_id,
            ));
            Ok(())
        }

        fn entry_notifications_enabled(&self) -> bool {
            self.entry_notifications_enabled
        }

        fn block_footer_notifications_enabled(&self) -> bool {
            self.block_footer_notifications_enabled
        }
    }

    fn loaded_test_plugin(plugin: TestEntryPlugin) -> Arc<LoadedGeyserPlugin> {
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
    fn test_notify_entry_update_parent_and_block_footer_independently() {
        let entry_plugin_entry_updates = Arc::new(Mutex::new(Vec::new()));
        let entry_plugin_update_parents = Arc::new(Mutex::new(Vec::new()));
        let entry_plugin_block_footer_updates = Arc::new(Mutex::new(Vec::new()));
        let block_footer_plugin_entry_updates = Arc::new(Mutex::new(Vec::new()));
        let block_footer_plugin_update_parents = Arc::new(Mutex::new(Vec::new()));
        let block_footer_plugin_block_footer_updates = Arc::new(Mutex::new(Vec::new()));
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager::from_plugins(
            vec![
                loaded_test_plugin(TestEntryPlugin {
                    entry_notifications_enabled: true,
                    block_footer_notifications_enabled: false,
                    entry_updates: entry_plugin_entry_updates.clone(),
                    update_parents: entry_plugin_update_parents.clone(),
                    block_footer_updates: entry_plugin_block_footer_updates.clone(),
                }),
                loaded_test_plugin(TestEntryPlugin {
                    entry_notifications_enabled: false,
                    block_footer_notifications_enabled: true,
                    entry_updates: block_footer_plugin_entry_updates.clone(),
                    update_parents: block_footer_plugin_update_parents.clone(),
                    block_footer_updates: block_footer_plugin_block_footer_updates.clone(),
                }),
            ],
        ))));
        let notifier = EntryNotifierImpl::new(plugin_manager);
        let entry = EntrySummary {
            num_hashes: 1,
            hash: Hash::new_unique(),
            num_transactions: 2,
        };
        let bank_hash = Hash::new_unique();
        let block_id = Hash::new_unique();
        let signature = SignatureCompressed([7; BLS_SIGNATURE_COMPRESSED_SIZE]);
        let block_footer = VersionedBlockFooter::V1(BlockFooterV1 {
            bank_hash,
            block_producer_time_nanos: 123,
            block_user_agent: b"test-validator".to_vec(),
            block_final_cert: Some(BlockFinalizationCert {
                slot: 42,
                block_id,
                final_aggregate: VotesAggregate::new(signature, vec![1, 2, 3]),
                notar_aggregate: Some(VotesAggregate::new(signature, vec![4, 5])),
            }),
            skip_reward_cert: Some(SkipRewardCertificate::try_new(41, signature, vec![6]).unwrap()),
            notar_reward_cert: Some(
                NotarRewardCertificate::try_new(42, block_id, signature, vec![8]).unwrap(),
            ),
        });
        let expected_block_footer =
            block_footer::VersionedBlockFooter::V1(block_footer::BlockFooterV1 {
                bank_hash,
                block_producer_time_nanos: 123,
                block_user_agent: b"test-validator",
                block_final_cert: Some(block_footer::BlockFinalizationCert {
                    slot: 42,
                    block_id,
                    final_aggregate: block_footer::VotesAggregate {
                        signature,
                        bitmap: &[1, 2, 3],
                    },
                    notar_aggregate: Some(block_footer::VotesAggregate {
                        signature,
                        bitmap: &[4, 5],
                    }),
                }),
                skip_reward_cert: Some(block_footer::SkipRewardCertificate {
                    slot: 41,
                    signature,
                    bitmap: &[6],
                }),
                notar_reward_cert: Some(block_footer::NotarRewardCertificate {
                    slot: 42,
                    block_id,
                    signature,
                    bitmap: &[8],
                }),
            });
        let parent_block_id = Hash::new_unique();

        notifier.notify_entry(42, 9, 3, &entry, 7);
        notifier.notify_entry_update_parent(&EntryUpdateParentInfo {
            slot: 42,
            cleared_bank_id: 9,
            parent_slot: 40,
            parent_block_id,
        });
        notifier.notify_block_footer(42, 9, &block_footer);

        assert_eq!(
            *entry_plugin_entry_updates.lock().unwrap(),
            vec![(42, 9, 3, 7)]
        );
        assert_eq!(
            *entry_plugin_update_parents.lock().unwrap(),
            vec![(42, 9, 40, parent_block_id)]
        );
        assert!(entry_plugin_block_footer_updates.lock().unwrap().is_empty());
        assert!(block_footer_plugin_entry_updates.lock().unwrap().is_empty());
        assert!(
            block_footer_plugin_update_parents
                .lock()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            *block_footer_plugin_block_footer_updates.lock().unwrap(),
            vec![(42, 9, format!("{expected_block_footer:?}"))]
        );
    }
}
