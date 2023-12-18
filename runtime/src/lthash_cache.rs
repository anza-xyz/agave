/*!
A cache for lattice hash
*/

use solana_accounts_db::accounts_db::LTHashCacheMap;

/// LTHashCache - A simple cache to avoid recomputing the LTHash for accounts
/// that are written EVERY slot.
///
/// The cache inherits all LTHashes for all account written in the parent slot
/// in `written_accounts_before` member, and stores the 2048 bytes of LTHashes
/// computed from all the accounts written in this slot in `written_account_after`
/// member.
#[derive(Default, Debug, AbiExample)]
pub struct LTHashCache {
    /// The pubkey, lthash/account pairs that were written by the last slot.
    /// MANY accounts are written EVERY slot. This avoids a re-hashing.
    pub written_accounts_before: LTHashCacheMap,

    /// The pubkey, lthash pairs that were written in this slot.
    pub written_accounts_after: LTHashCacheMap,
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_accounts_db::{accounts_db::LTHashCacheValue, accounts_hash::AccountLTHash},
        solana_sdk::{account::AccountSharedData, pubkey::Pubkey},
        //std::{collections::HashMap, sync::RwLock},
    };

    #[test]
    fn test_lt_hash_cache() {
        // Check cache default to empty.
        let c = LTHashCache::default();
        assert!(c.written_accounts_before.read().unwrap().is_empty());
        assert!(c.written_accounts_after.read().unwrap().is_empty());

        // Write to cache.
        let k = solana_sdk::pubkey::new_rand();
        let account = AccountSharedData::new(100, 0, &Pubkey::default());
        let h = AccountLTHash::default();
        c.written_accounts_before
            .write()
            .unwrap()
            .insert(k, LTHashCacheValue::Hash(Box::new(h)));
        c.written_accounts_after
            .write()
            .unwrap()
            .insert(k, LTHashCacheValue::Account(Box::new(account.clone())));
        assert!(!c.written_accounts_before.read().unwrap().is_empty());
        assert!(!c.written_accounts_after.read().unwrap().is_empty());

        // Read from Cache.
        let map1 = c.written_accounts_before.read().unwrap();
        let d1 = map1.get(&k).unwrap();
        assert_eq!(d1, &LTHashCacheValue::Hash(Box::new(h)));

        let map2 = c.written_accounts_after.write().unwrap();
        let d2 = map2.get(&k).unwrap();
        assert_eq!(d2, &LTHashCacheValue::Account(Box::new(account)));
    }
}
