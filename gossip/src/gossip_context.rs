use {
    arc_swap::ArcSwap,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc},
};

/// Policy inputs shared by all stages of the gossip runtime.
pub(crate) struct GossipContextSnapshot {
    pub(crate) stakes: Arc<HashMap<Pubkey, u64>>,
    pub(crate) is_full_alpenglow_epoch: bool,
}

/// Publishes one coherent network-policy snapshot to validation, protocol,
/// and metrics threads.
pub(crate) struct GossipContext {
    snapshot: ArcSwap<GossipContextSnapshot>,
}

impl GossipContext {
    pub(crate) fn new(stakes: Arc<HashMap<Pubkey, u64>>, is_full_alpenglow_epoch: bool) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(GossipContextSnapshot {
                stakes,
                is_full_alpenglow_epoch,
            }),
        }
    }

    pub(crate) fn load(&self) -> Arc<GossipContextSnapshot> {
        self.snapshot.load_full()
    }

    pub(crate) fn update(&self, stakes: Arc<HashMap<Pubkey, u64>>, is_full_alpenglow_epoch: bool) {
        self.snapshot.store(Arc::new(GossipContextSnapshot {
            stakes,
            is_full_alpenglow_epoch,
        }));
    }
}
