//! Process-wide cache of decompressed Ed25519 verifying keys, keyed by the
//! 32-byte pubkey. Decompressing the wire-format y-coordinate into an
//! `EdwardsPoint` (`sqrt_ratio_i` / `pow_p58`) accounts for ~6-7% of CPU on the
//! gossip receive path because the same validator pubkeys recur across every
//! push/pull message. Memoising the decompressed point lets repeat verifies
//! reuse it.
//!
//! Insertion policy: callers must only insert *after* the surrounding
//! signature has verified. That forces an attacker who wants to churn cache
//! entries to first produce a valid signature for each pubkey they insert,
//! which means paying full sigverify cost. With a bounded LRU on top, the
//! cache cannot be made cheaper to displace than honest gossip traffic.

use {
    lazy_lru::LruCache,
    solana_pubkey::Pubkey,
    std::sync::{OnceLock, RwLock},
};

// Capacity in entries. Each entry is ~200 bytes (the `VerifyingKey` struct
// holds the compressed bytes plus the decompressed `EdwardsPoint`), so 8192
// entries is ~1.6 MiB. A live cluster has on the order of 1-3k validators, so
// this comfortably covers the working set with headroom for transient peers.
const CAPACITY: usize = 8192;

fn cache() -> &'static RwLock<LruCache<Pubkey, ed25519_dalek::VerifyingKey>> {
    static CACHE: OnceLock<RwLock<LruCache<Pubkey, ed25519_dalek::VerifyingKey>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(LruCache::new(CAPACITY)))
}

/// Returns the cached decompressed verifying key for `pubkey`, or `None` on
/// miss. Does not insert; the caller is responsible for decompressing on miss
/// and calling [`insert`] after the surrounding signature has verified.
pub(crate) fn get(pubkey: &Pubkey) -> Option<ed25519_dalek::VerifyingKey> {
    cache().read().unwrap().get(pubkey).copied()
}

/// Inserts `vk` into the cache. Call only after a signature has been verified
/// against `vk` — see the module-level note on insertion policy.
pub(crate) fn insert(pubkey: Pubkey, vk: ed25519_dalek::VerifyingKey) {
    cache().write().unwrap().put(pubkey, vk);
}
