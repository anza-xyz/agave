/// Module responsible for notifying plugins of gossip messages
use {
    crate::geyser_plugin_manager::GeyserPluginManager,
    agave_geyser_plugin_interface::geyser_plugin_interface::FfiPubkey,
    log::*,
    solana_gossip::{
        contact_info::ContactInfo, contact_info_ffi::create_contact_info_interface,
        gossip_message_notifier_interface::GossipMessageNotifierInterface,
    },
    solana_measure::measure::Measure,
    solana_metrics::*,
    solana_sdk::pubkey::Pubkey,
    std::sync::{Arc, RwLock},
};

#[derive(Debug)]
pub(crate) struct GossipMessageNotifierImpl {
    plugin_manager: Arc<RwLock<GeyserPluginManager>>,
}

impl GossipMessageNotifierInterface for GossipMessageNotifierImpl {
    fn notify_receive_node_update(&self, contact_info: &ContactInfo) {
        self.notify_plugins_of_node_update(contact_info);
    }

    fn notify_remove_node(&self, pubkey: &Pubkey) {
        let ffi_pubkey = self.ffi_pubkey_from_pubkey(pubkey);
        self.notify_plugins_of_node_removal(&ffi_pubkey);
    }
}

impl GossipMessageNotifierImpl {
    pub fn new(plugin_manager: Arc<RwLock<GeyserPluginManager>>) -> Self {
        Self { plugin_manager }
    }

    fn ffi_pubkey_from_pubkey(&self, pubkey: &Pubkey) -> FfiPubkey {
        FfiPubkey {
            pubkey: pubkey.to_bytes(),
        }
    }

    fn notify_plugins_of_node_update(&self, contact_info: &ContactInfo) {
        let mut measure_all = Measure::start("geyser-plugin-notify_plugins_of_node_update");
        let plugin_manager = self.plugin_manager.read().unwrap();
        if plugin_manager.plugins.is_empty() {
            return;
        }

        let ffi_contact_info_interface = unsafe { create_contact_info_interface(contact_info) };

        for plugin in plugin_manager.plugins.iter() {
            let mut measure_plugin = Measure::start("geyser-plugin-notify_node_update");
            match plugin.notify_node_update(&ffi_contact_info_interface) {
                Err(err) => {
                    error!(
                        "Failed to insert ContactInfo w/ origin: {}, error: {} to plugin {}",
                        contact_info.pubkey(),
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "Inserted ContactInfo w/ origin: {} to plugin {}",
                        contact_info.pubkey(),
                        plugin.name()
                    )
                }
            }
            measure_plugin.stop();
            inc_new_counter_debug!(
                "geyser-plugin-notify_node_update-us",
                measure_plugin.as_us() as usize,
                100000,
                100000
            );
        }
        measure_all.stop();
        inc_new_counter_debug!(
            "geyser-plugin-notify_plugins_of_node_update-us",
            measure_all.as_us() as usize,
            100000,
            100000
        );
    }

    fn notify_plugins_of_node_removal(&self, ffi_pubkey: &FfiPubkey) {
        let mut measure_all = Measure::start("geyser-plugin-notify_plugins_of_node_removal");
        let plugin_manager = self.plugin_manager.read().unwrap();
        if plugin_manager.plugins.is_empty() {
            return;
        }

        for plugin in plugin_manager.plugins.iter() {
            let mut measure_plugin = Measure::start("geyser-plugin-notify_node_removal");
            match plugin.notify_node_removal(ffi_pubkey) {
                Err(err) => {
                    error!(
                        "Failed to remove pubkey: {}, error: {} to plugin {}",
                        Pubkey::from(ffi_pubkey.pubkey),
                        err,
                        plugin.name()
                    )
                }
                Ok(_) => {
                    trace!(
                        "removed pubkey: {} to plugin {}",
                        Pubkey::from(ffi_pubkey.pubkey),
                        plugin.name()
                    )
                }
            }
            measure_plugin.stop();
            inc_new_counter_debug!(
                "geyser-plugin-notify_node_removal-us",
                measure_plugin.as_us() as usize,
                100000,
                100000
            );
        }
        measure_all.stop();
        inc_new_counter_debug!(
            "geyser-plugin-notify_plugins_of_node_removal-us",
            measure_all.as_us() as usize,
            100000,
            100000
        );
    }
}
