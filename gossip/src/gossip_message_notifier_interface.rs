use {crate::contact_info::ContactInfo, solana_sdk::pubkey::Pubkey, std::sync::Arc};

pub trait GossipMessageNotifierInterface: std::fmt::Debug {
    /// Notified when a CrdsValue is upserted
    fn notify_receive_node_update(&self, contact_info: &ContactInfo);

    /// Notified when a node is removed/purged from crds table
    fn notify_remove_node(&self, pubkey: &Pubkey);
}

pub type GossipMessageNotifier = Arc<dyn GossipMessageNotifierInterface + Sync + Send>;
