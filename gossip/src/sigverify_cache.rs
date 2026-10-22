use {
    crate::cluster_info::CRDS_UNIQUE_PUBKEY_CAPACITY, lazy_lru::LruCache, solana_hash::Hash,
    solana_pubkey::Pubkey, std::sync::RwLock,
};

/// Bundles the two caches consulted on the gossip signature-verification path:
/// `verifying_keys` memoises decompressed Ed25519 points per pubkey, and
/// `verified_values` short-circuits sigverify entirely for CrdsValue hashes that
/// were already verified.
pub(crate) struct SigVerifyCache {
    pub(crate) verifying_keys: VerifyingKeyCache,
    pub(crate) verified_values: VerifiedCrdsCache,
}

impl SigVerifyCache {
    pub(crate) fn new() -> Self {
        Self {
            verifying_keys: VerifyingKeyCache::new(),
            verified_values: VerifiedCrdsCache::new(),
        }
    }
}

/// Benchmarks show no meaningful improvements beyond this value.
const CAPACITY: usize = 16384;

/// LRU of CrdsValue hashes whose Ed25519 signature has already been verified,
/// letting duplicate gossip floods skip sigverify. Keyed by the CrdsValue hash
/// (`sha256(signature || serialized_data)`), so a hit guarantees identical bytes.
/// Insert only after a successful verification.
pub(crate) struct VerifiedCrdsCache {
    cache: RwLock<LruCache<Hash, ()>>,
}

impl VerifiedCrdsCache {
    pub(crate) fn new() -> Self {
        Self {
            cache: RwLock::new(LruCache::new(CAPACITY)),
        }
    }

    /// Returns true if a CrdsValue with this hash has already been verified
    /// (modulo LRU eviction).
    pub(crate) fn contains(&self, hash: &Hash) -> bool {
        self.cache.read().unwrap().get(hash).is_some()
    }

    /// Marks `hash` as verified. Call only after the surrounding signature has
    /// actually verified — see the struct-level note on insertion policy.
    pub(crate) fn insert(&self, hash: Hash) {
        self.cache.write().unwrap().put(hash, ());
    }
}

/// LRU of decompressed Ed25519 verifying keys, memoising the non-trivial pubkey
/// decompression on the gossip receive path. Sized to the CRDS unique-pubkey
/// capacity. Insert only after the signature verifies, so the cache can't be
/// seeded with arbitrary pubkeys to evict useful entries.
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
