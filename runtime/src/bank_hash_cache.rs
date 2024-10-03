//! Lightweight cache tracking the bank hashes of replayed banks.
//! This can be useful to avoid read-locking bank forks just to query the bank hash.

use {
    solana_sdk::{clock::Slot, hash::Hash},
    std::collections::BTreeMap,
};

#[derive(Default, Debug)]
pub struct BankHashCache {
    hashes: BTreeMap<Slot, Hash>,
}

impl BankHashCache {
    /// Insert new frozen bank, returning the hash of the previously dumped bank if it exists
    pub fn freeze(&mut self, slot: Slot, hash: Hash) -> Option<Hash> {
        self.hashes.insert(slot, hash)
    }

    /// Returns the replayed bank hash of `slot`.
    /// If `slot` has been dumped, returns the previously replayed hash.
    pub fn bank_hash(&self, slot: Slot) -> Option<Hash> {
        self.hashes.get(&slot).copied()
    }

    /// Removes `slots` from the cache. Intended to be used with `BankForks::prune_non_rooted`
    pub fn prune<I>(&mut self, slots: I)
    where
        I: Iterator<Item = Slot>,
    {
        for slot in slots {
            self.hashes.remove(&slot);
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{create_genesis_config, GenesisConfigInfo},
        },
        solana_sdk::pubkey::Pubkey,
    };

    #[test]
    fn test_bank_hash_cache() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank0 = Bank::new_for_tests(&genesis_config);
        let slot = bank0.slot();
        let bank_forks = BankForks::new_rw_arc(bank0);
        let bank_hash_cache = bank_forks.read().unwrap().bank_hash_cache();
        bank_forks.read().unwrap()[slot].freeze();
        assert_eq!(
            bank_hash_cache.read().unwrap().bank_hash(slot).unwrap(),
            bank_forks.read().unwrap()[slot].hash()
        );

        let bank0 = bank_forks.read().unwrap().get(slot).unwrap();
        let slot = 10;
        let bank10 = Bank::new_from_parent(bank0, &Pubkey::new_unique(), slot);
        bank_forks.write().unwrap().insert(bank10);
        bank_forks.read().unwrap()[slot].freeze();
        assert_eq!(
            bank_hash_cache.read().unwrap().bank_hash(slot).unwrap(),
            bank_forks.read().unwrap()[slot].hash()
        );
    }
}
