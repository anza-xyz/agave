//! Process-wide LRU of CrdsValue hashes whose Ed25519 signature has already
//! been verified. Gossip flood-broadcasts mean the same (signature, data)
//! tuple arrives many times within seconds; consulting this cache before
//! sigverify lets duplicates skip the curve arithmetic entirely.
//!
//! The cache is keyed by `sha256(signature || serialized_data)` (already
//! pre-computed and stored on each `CrdsValue`), so a hit is a guarantee that
//! the *same* bytes were previously verified — no risk of confusing two
//! different CrdsValues that happen to share a pubkey.
//!
//! Insertion policy: only insert after a successful signature verification. An
//! attacker who wants to churn this cache must produce a valid signature per
//! entry, which costs them the same as honest gossip.

use {
    lazy_lru::LruCache,
    solana_hash::Hash,
    std::sync::{OnceLock, RwLock},
};

// 64Ki entries × (32-byte Hash + LRU overhead) ≈ a few MiB, covering several
// seconds of worst-case gossip throughput from the live cluster.
const CAPACITY: usize = 65536;

fn cache() -> &'static RwLock<LruCache<Hash, ()>> {
    static CACHE: OnceLock<RwLock<LruCache<Hash, ()>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(LruCache::new(CAPACITY)))
}

/// Returns true if a CrdsValue with this hash has already been verified during
/// this process's lifetime (modulo LRU eviction).
pub(crate) fn contains(hash: &Hash) -> bool {
    cache().read().unwrap().get(hash).is_some()
}

/// Marks `hash` as verified. Call only after the surrounding signature has
/// actually verified — see the module-level note on insertion policy.
pub(crate) fn insert(hash: Hash) {
    cache().write().unwrap().put(hash, ());
}
