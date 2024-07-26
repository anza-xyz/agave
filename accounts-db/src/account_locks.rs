use {
    ahash::{AHashMap, AHashSet},
    log::debug,
    solana_sdk::{pubkey::Pubkey, transaction::TransactionError},
    std::collections::hash_map,
};

#[derive(Debug, Default)]
pub struct AccountLocks {
    pub write_locks: AHashSet<Pubkey>,
    pub readonly_locks: AHashMap<Pubkey, u64>,
}

impl AccountLocks {
    /// Lock the account keys in `keys` for a transaction.
    /// The bool in the tuple indicates if the account is writable.
    /// Returns an error if any of the accounts are already locked in a way
    /// that conflicts with the requested lock.
    pub fn lock_accounts_for_transaction<'a>(
        &mut self,
        keys: impl Iterator<Item = (&'a Pubkey, bool)> + Clone,
    ) -> Result<(), TransactionError> {
        for (k, writable) in keys.clone() {
            if writable {
                if self.is_locked_write(k) || self.is_locked_readonly(k) {
                    debug!("Writable account in use: {:?}", k);
                    return Err(TransactionError::AccountInUse);
                }
            } else if self.is_locked_write(k) {
                debug!("Read-only account in use: {:?}", k);
                return Err(TransactionError::AccountInUse);
            }
        }

        for (k, writable) in keys {
            if writable {
                self.write_locks.insert(*k);
            } else if !self.lock_readonly(k) {
                self.insert_new_readonly(k);
            }
        }

        Ok(())
    }

    /// Unlock the account keys in `keys` after a transaction.
    /// The bool in the tuple indicates if the account is writable.
    /// In debug-mode this function will panic if an attempt is made to unlock
    /// an account that wasn't locked in the way requested.
    pub fn unlock_accounts_for_transaction<'a>(
        &mut self,
        keys: impl Iterator<Item = (&'a Pubkey, bool)>,
    ) {
        for (k, writable) in keys {
            if writable {
                self.unlock_write(k);
            } else {
                self.unlock_readonly(k);
            }
        }
    }

    fn is_locked_readonly(&self, key: &Pubkey) -> bool {
        self.readonly_locks
            .get(key)
            .map_or(false, |count| *count > 0)
    }

    fn is_locked_write(&self, key: &Pubkey) -> bool {
        self.write_locks.contains(key)
    }

    fn insert_new_readonly(&mut self, key: &Pubkey) {
        assert!(self.readonly_locks.insert(*key, 1).is_none());
    }

    fn lock_readonly(&mut self, key: &Pubkey) -> bool {
        self.readonly_locks.get_mut(key).map_or(false, |count| {
            *count += 1;
            true
        })
    }

    fn unlock_readonly(&mut self, key: &Pubkey) {
        if let hash_map::Entry::Occupied(mut occupied_entry) = self.readonly_locks.entry(*key) {
            let count = occupied_entry.get_mut();
            *count -= 1;
            if *count == 0 {
                occupied_entry.remove_entry();
            }
        } else {
            debug_assert!(
                false,
                "Attempted to remove a read-lock for a key that wasn't read-locked"
            );
        }
    }

    fn unlock_write(&mut self, key: &Pubkey) {
        let removed = self.write_locks.remove(key);
        debug_assert!(
            removed,
            "Attempted to remove a write-lock for a key that wasn't write-locked"
        );
    }
}
