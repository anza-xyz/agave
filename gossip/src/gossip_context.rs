use {
    arc_swap::ArcSwap,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc},
};

/// Network policy shared across gossip stages.
pub(crate) struct GossipContextSnapshot {
    pub(crate) stakes: Arc<HashMap<Pubkey, u64>>,
    pub(crate) is_full_alpenglow_epoch: bool,
}

/// Atomically publishes coherent network-policy snapshots.
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
        let current = self.snapshot.load();
        if Arc::ptr_eq(&current.stakes, &stakes)
            && current.is_full_alpenglow_epoch == is_full_alpenglow_epoch
        {
            return;
        }
        self.snapshot.store(Arc::new(GossipContextSnapshot {
            stakes,
            is_full_alpenglow_epoch,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_reuses_unchanged_snapshot() {
        let stakes = Arc::new(HashMap::new());
        let context = GossipContext::new(Arc::clone(&stakes), false);
        let before = context.load();
        context.update(stakes, false);
        assert!(Arc::ptr_eq(&before, &context.load()));
    }
}
