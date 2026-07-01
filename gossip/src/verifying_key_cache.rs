use {
    crate::cluster_info::CRDS_UNIQUE_PUBKEY_CAPACITY, lazy_lru::LruCache, solana_pubkey::Pubkey,
    std::sync::RwLock,
};

/// Cache of decompressed Ed25519 verifying keys. Decompressing a pubkey into a
/// curve point is a non-trivial cost on the gossip receive path; memoising it
/// avoids redoing that work on repeat verifies. Callers must insert only after
/// the signature verifies, so the cache can't be seeded with arbitrary pubkeys
/// to evict useful entries. Sized to the CRDS unique-pubkey capacity to cover
/// the cluster.
pub(crate) struct VerifyingKeyCache {
    cache: RwLock<LruCache<Pubkey, ed25519_dalek::VerifyingKey>>,
}

impl VerifyingKeyCache {
    pub(crate) fn new() -> Self {
        Self {
            cache: RwLock::new(LruCache::new(CRDS_UNIQUE_PUBKEY_CAPACITY)),
        }
    }

    pub(crate) fn get(&self, pubkey: &Pubkey) -> Option<ed25519_dalek::VerifyingKey> {
        self.cache.read().unwrap().get(pubkey).copied()
    }

    /// Insert only after a signature against `vk` has verified.
    pub(crate) fn insert(&self, pubkey: Pubkey, vk: ed25519_dalek::VerifyingKey) {
        self.cache.write().unwrap().put(pubkey, vk);
    }
}
