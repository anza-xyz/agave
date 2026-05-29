//! Process-wide cache of decompressed Ed25519 verifying keys. Pubkey
//! decompression is ~6-7% of CPU on the gossip receive path; memoising it lets
//! repeat verifies skip `sqrt_ratio_i`. Callers must only insert after the
//! signature has verified, so churning the LRU costs an attacker a full
//! sigverify per entry.

use {
    lazy_lru::LruCache,
    solana_pubkey::Pubkey,
    std::sync::{OnceLock, RwLock},
};

// 8192 entries × ~200 B/entry ≈ 1.6 MiB; covers a 1-3k-validator cluster.
const CAPACITY: usize = 8192;

fn cache() -> &'static RwLock<LruCache<Pubkey, ed25519_dalek::VerifyingKey>> {
    static CACHE: OnceLock<RwLock<LruCache<Pubkey, ed25519_dalek::VerifyingKey>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(LruCache::new(CAPACITY)))
}

pub(crate) fn get(pubkey: &Pubkey) -> Option<ed25519_dalek::VerifyingKey> {
    cache().read().unwrap().get(pubkey).copied()
}

/// Insert only after a signature against `vk` has verified.
pub(crate) fn insert(pubkey: Pubkey, vk: ed25519_dalek::VerifyingKey) {
    cache().write().unwrap().put(pubkey, vk);
}
