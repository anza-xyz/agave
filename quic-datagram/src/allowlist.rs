use {
    arc_swap::ArcSwap,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc},
};

pub trait Allowlist: Send + Sync + 'static {
    /// Called once per handshake, then periodically during connection lifetime.
    ///  A `false` answer closes the connection with the `NOT_ADMITTED` error code.
    fn allow(&self, peer: &Pubkey) -> bool;
}

/// Snapshot of the currently-allowed peer set.
#[derive(Default)]
pub struct StakedNodesAllowlist {
    inner: ArcSwap<HashMap<Pubkey, u64>>,
}

impl StakedNodesAllowlist {
    /// `peers` maps admitted pubkeys to epoch stake.
    pub fn new(peers: HashMap<Pubkey, u64>) -> Self {
        Self {
            inner: ArcSwap::new(Arc::new(peers)),
        }
    }

    /// Atomically publish a new allowlist generation.
    pub fn swap(&self, peers: Arc<HashMap<Pubkey, u64>>) {
        self.inner.store(peers);
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn is_empty(&self) -> bool {
        self.inner.load().is_empty()
    }
}

impl Allowlist for StakedNodesAllowlist {
    fn allow(&self, peer: &Pubkey) -> bool {
        self.inner.load().contains_key(peer)
    }
}

/// Allow every peer.
pub struct AllowAll;

impl Allowlist for AllowAll {
    fn allow(&self, _: &Pubkey) -> bool {
        true
    }
}
