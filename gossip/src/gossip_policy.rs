use {
    arc_swap::ArcSwap,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc},
};

/// Network policy shared across gossip stages.
pub(crate) struct GossipPolicySnapshot {
    pub(crate) stakes: Arc<HashMap<Pubkey, u64>>,
    pub(crate) is_full_alpenglow_epoch: bool,
}

/// Atomically publishes coherent network-policy snapshots.
pub(crate) struct GossipPolicy {
    snapshot: ArcSwap<GossipPolicySnapshot>,
}

impl GossipPolicy {
    pub(crate) fn new(stakes: Arc<HashMap<Pubkey, u64>>, is_full_alpenglow_epoch: bool) -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(GossipPolicySnapshot {
                stakes,
                is_full_alpenglow_epoch,
            }),
        }
    }

    pub(crate) fn load(&self) -> Arc<GossipPolicySnapshot> {
        self.snapshot.load_full()
    }

    /// Publishes a new snapshot, reusing the current one when nothing changed.
    pub(crate) fn update(&self, stakes: Arc<HashMap<Pubkey, u64>>, is_full_alpenglow_epoch: bool) {
        let current = self.snapshot.load();
        if Arc::ptr_eq(&current.stakes, &stakes)
            && current.is_full_alpenglow_epoch == is_full_alpenglow_epoch
        {
            return;
        }
        self.snapshot.store(Arc::new(GossipPolicySnapshot {
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
        let policy = GossipPolicy::new(Arc::clone(&stakes), false);
        let before = policy.load();
        policy.update(stakes, false);
        assert!(Arc::ptr_eq(&before, &policy.load()));
    }
}
