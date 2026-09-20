use {
    crate::geyser_plugin_manager::GeyserPluginManager,
    agave_geyser_plugin_interface::geyser_plugin_interface::SlotStatus,
    arc_swap::ArcSwap,
    log::*,
    solana_clock::{BankId, Slot},
    solana_rpc::slot_status_notifier::SlotStatusNotifierInterface,
    std::sync::Arc,
};

pub struct SlotStatusNotifierImpl {
    plugin_manager: Arc<ArcSwap<GeyserPluginManager>>,
}

impl SlotStatusNotifierInterface for SlotStatusNotifierImpl {
    fn notify_slot_confirmed(&self, slot: Slot, parent: Option<Slot>, bank_id: BankId) {
        self.notify_bank_status(slot, parent, SlotStatus::Confirmed, bank_id);
    }

    fn notify_slot_processed(&self, slot: Slot, parent: Option<Slot>, bank_id: BankId) {
        self.notify_bank_status(slot, parent, SlotStatus::Processed, bank_id);
    }

    fn notify_slot_rooted(&self, slot: Slot, parent: Option<Slot>, bank_id: BankId) {
        self.notify_bank_status(slot, parent, SlotStatus::Rooted, bank_id);
    }

    fn notify_first_shred_received(&self, slot: Slot) {
        self.notify_slot_status(slot, None, SlotStatus::FirstShredReceived);
    }

    fn notify_completed(&self, slot: Slot) {
        self.notify_slot_status(slot, None, SlotStatus::Completed);
    }

    fn notify_created_bank(&self, slot: Slot, parent: Slot, bank_id: BankId) {
        self.notify_bank_status(slot, Some(parent), SlotStatus::CreatedBank, bank_id);
    }

    fn notify_slot_dead(&self, slot: Slot, parent: Slot, error: String) {
        self.notify_slot_status(slot, Some(parent), SlotStatus::Dead(error));
    }
}

impl SlotStatusNotifierImpl {
    pub fn new(plugin_manager: Arc<ArcSwap<GeyserPluginManager>>) -> Self {
        Self { plugin_manager }
    }

    pub fn notify_slot_status(&self, slot: Slot, parent: Option<Slot>, slot_status: SlotStatus) {
        let plugin_manager = self.plugin_manager.load();
        if plugin_manager.plugins.is_empty() {
            return;
        }

        for plugin in plugin_manager.plugins.iter() {
            match plugin.update_slot_status(slot, parent, &slot_status) {
                Err(err) => {
                    error!(
                        "Failed to update slot status at slot {}, error: {} to plugin {}",
                        slot,
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully updated slot status at slot {} to plugin {}",
                        slot,
                        plugin.name()
                    );
                }
            }
        }
    }

    pub fn notify_bank_status(
        &self,
        slot: Slot,
        parent: Option<Slot>,
        slot_status: SlotStatus,
        bank_id: BankId,
    ) {
        let plugin_manager = self.plugin_manager.load();
        if plugin_manager.plugins.is_empty() {
            return;
        }

        for plugin in plugin_manager.plugins.iter() {
            match plugin.update_bank_status(slot, parent, &slot_status, bank_id) {
                Err(err) => {
                    error!(
                        "Failed to update bank status at slot {}, error: {} to plugin {}",
                        slot,
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Successfully updated bank status at slot {} to plugin {}",
                        slot,
                        plugin.name()
                    );
                }
            }
        }
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

    type SlotStatusUpdate = (Slot, Option<Slot>, SlotStatus, BankId);

    #[derive(Debug)]
    struct TestSlotStatusPlugin {
        updates: Arc<Mutex<Vec<SlotStatusUpdate>>>,
    }

    impl GeyserPlugin for TestSlotStatusPlugin {
        fn name(&self) -> &'static str {
            "test-slot-status-plugin"
        }

        fn update_bank_status(
            &self,
            slot: Slot,
            parent: Option<u64>,
            status: &SlotStatus,
            bank_id: BankId,
        ) -> Result<()> {
            self.updates
                .lock()
                .unwrap()
                .push((slot, parent, status.clone(), bank_id));
            Ok(())
        }
    }

    fn loaded_test_plugin(plugin: TestSlotStatusPlugin) -> Arc<LoadedGeyserPlugin> {
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

    fn create_notifier(updates: Arc<Mutex<Vec<SlotStatusUpdate>>>) -> SlotStatusNotifierImpl {
        let plugin_manager = Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
            plugins: vec![loaded_test_plugin(TestSlotStatusPlugin { updates })],
        })));
        SlotStatusNotifierImpl::new(plugin_manager)
    }

    #[test]
    fn test_notify_slot_status_bank_id() {
        let updates = Arc::new(Mutex::new(Vec::new()));
        let notifier = create_notifier(updates.clone());

        notifier.notify_created_bank(42, 41, 9);
        notifier.notify_slot_processed(42, Some(41), 9);

        assert_eq!(
            *updates.lock().unwrap(),
            vec![
                (42, Some(41), SlotStatus::CreatedBank, 9),
                (42, Some(41), SlotStatus::Processed, 9),
            ]
        );
    }
}
