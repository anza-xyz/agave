#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use solana_svm_transaction::svm_message::SVMMessage;
use {
    ahash::{AHashMap, AHashSet},
    solana_sdk::{
        message::AccountKeys,
        pubkey::Pubkey,
        transaction::{TransactionError, MAX_TX_ACCOUNT_LOCKS},
    },
    std::{cell::RefCell, collections::hash_map},
};

#[derive(Debug, Default)]
pub struct AccountLocks {
    write_locks: AHashSet<Pubkey>,
    readonly_locks: AHashMap<Pubkey, u64>,
}

impl AccountLocks {
    /// Lock the account keys in `keys` for a transaction.
    /// The bool in the tuple indicates if the account is writable.
    /// Returns an error if any of the accounts are already locked in a way
    /// that conflicts with the requested lock.
    pub fn try_lock_accounts<'a>(
        &mut self,
        keys: impl Iterator<Item = (&'a Pubkey, bool)> + Clone,
    ) -> Result<(), TransactionError> {
        for (key, writable) in keys.clone() {
            if writable {
                if !self.can_write_lock(key) {
                    return Err(TransactionError::AccountInUse);
                }
            } else if !self.can_read_lock(key) {
                return Err(TransactionError::AccountInUse);
            }
        }

        for (key, writable) in keys {
            if writable {
                self.lock_write(key);
            } else {
                self.lock_readonly(key);
            }
        }

        Ok(())
    }

    /// Unlock the account keys in `keys` after a transaction.
    /// The bool in the tuple indicates if the account is writable.
    /// In debug-mode this function will panic if an attempt is made to unlock
    /// an account that wasn't locked in the way requested.
    pub fn unlock_accounts<'a>(&mut self, keys: impl Iterator<Item = (&'a Pubkey, bool)>) {
        for (k, writable) in keys {
            if writable {
                self.unlock_write(k);
            } else {
                self.unlock_readonly(k);
            }
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn is_locked_readonly(&self, key: &Pubkey) -> bool {
        self.readonly_locks
            .get(key)
            .map_or(false, |count| *count > 0)
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn is_locked_write(&self, key: &Pubkey) -> bool {
        self.write_locks.contains(key)
    }

    fn can_read_lock(&self, key: &Pubkey) -> bool {
        // If the key is not write-locked, it can be read-locked
        !self.is_locked_write(key)
    }

    fn can_write_lock(&self, key: &Pubkey) -> bool {
        // If the key is not read-locked or write-locked, it can be write-locked
        !self.is_locked_readonly(key) && !self.is_locked_write(key)
    }

    fn lock_readonly(&mut self, key: &Pubkey) {
        *self.readonly_locks.entry(*key).or_default() += 1;
    }

    fn lock_write(&mut self, key: &Pubkey) {
        self.write_locks.insert(*key);
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

/// Validate account locks before locking.
pub fn validate_account_locks<'a, Tx: SVMMessage + 'a>(
    tx: &'a Tx,
    tx_account_lock_limit: usize,
) -> Result<(), TransactionError> {
    if tx.num_write_locks() > tx_account_lock_limit as u64 {
        Err(TransactionError::TooManyAccountLocks)
    } else if has_duplicates(tx.account_keys()) {
        Err(TransactionError::AccountLoadedTwice)
    } else {
        Ok(())
    }
}

thread_local! {
    static HAS_DUPLICATES_SET: RefCell<AHashSet<Pubkey>> = RefCell::new(AHashSet::with_capacity(MAX_TX_ACCOUNT_LOCKS));
}

/// Check for duplicate account keys.
fn has_duplicates(account_keys: AccountKeys) -> bool {
    // Benchmarking has shown that for sets of 32 or more keys, it is faster to
    // use a HashSet to check for duplicates.
    // For smaller sets a brute-force O(n^2) check seems to be faster.
    const USE_ACCOUNT_LOCK_SET_SIZE: usize = 32;
    if account_keys.len() >= USE_ACCOUNT_LOCK_SET_SIZE {
        HAS_DUPLICATES_SET.with_borrow_mut(|set| {
            let has_duplicates = account_keys.iter().any(|key| !set.insert(*key));
            set.clear();
            has_duplicates
        })
    } else {
        for (idx, key) in account_keys.iter().enumerate() {
            for jdx in idx + 1..account_keys.len() {
                if key == &account_keys[jdx] {
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_sdk::{
            hash::Hash,
            instruction::CompiledInstruction,
            message::{
                v0::{self, LoadedAddresses, MessageAddressTableLookup},
                MessageHeader, SanitizedMessage, SanitizedVersionedMessage, SimpleAddressLoader,
                VersionedMessage,
            },
        },
        std::collections::HashSet,
    };

    fn create_message_for_test(
        num_signers: u8,
        num_writable: u8,
        account_keys: Vec<Pubkey>,
        instructions: Vec<CompiledInstruction>,
        loaded_addresses: LoadedAddresses,
    ) -> SanitizedMessage {
        let header = MessageHeader {
            num_required_signatures: num_signers,
            num_readonly_signed_accounts: 0,
            num_readonly_unsigned_accounts: u8::try_from(account_keys.len()).unwrap()
                - num_writable,
        };
        let (versioned_message, loader) = (
            VersionedMessage::V0(v0::Message {
                header,
                account_keys,
                recent_blockhash: Hash::default(),
                instructions,
                address_table_lookups: vec![MessageAddressTableLookup {
                    account_key: Pubkey::new_unique(),
                    writable_indexes: (0..loaded_addresses.writable.len())
                        .map(|x| x as u8)
                        .collect(),
                    readonly_indexes: (0..loaded_addresses.readonly.len())
                        .map(|x| (loaded_addresses.writable.len() + x) as u8)
                        .collect(),
                }],
            }),
            SimpleAddressLoader::Enabled(loaded_addresses),
        );
        SanitizedMessage::try_new(
            SanitizedVersionedMessage::try_new(versioned_message).unwrap(),
            loader,
            &HashSet::new(),
        )
        .unwrap()
    }

    #[test]
    fn test_account_locks() {
        let mut account_locks = AccountLocks::default();

        let key1 = Pubkey::new_unique();
        let key2 = Pubkey::new_unique();

        // Add write and read-lock.
        let result = account_locks.try_lock_accounts([(&key1, true), (&key2, false)].into_iter());
        assert!(result.is_ok());

        // Try to add duplicate write-lock.
        let result = account_locks.try_lock_accounts([(&key1, true)].into_iter());
        assert_eq!(result, Err(TransactionError::AccountInUse));

        // Try to add write lock on read-locked account.
        let result = account_locks.try_lock_accounts([(&key2, true)].into_iter());
        assert_eq!(result, Err(TransactionError::AccountInUse));

        // Try to add read lock on write-locked account.
        let result = account_locks.try_lock_accounts([(&key1, false)].into_iter());
        assert_eq!(result, Err(TransactionError::AccountInUse));

        // Add read lock on read-locked account.
        let result = account_locks.try_lock_accounts([(&key2, false)].into_iter());
        assert!(result.is_ok());

        // Unlock write and read locks.
        account_locks.unlock_accounts([(&key1, true), (&key2, false)].into_iter());

        // No more remaining write-locks. Read-lock remains.
        assert!(!account_locks.is_locked_write(&key1));
        assert!(account_locks.is_locked_readonly(&key2));

        // Unlock read lock.
        account_locks.unlock_accounts([(&key2, false)].into_iter());
        assert!(!account_locks.is_locked_readonly(&key2));
    }

    #[test]
    fn test_validate_account_locks_valid_no_writable() {
        let message = create_message_for_test(
            1,
            0,
            vec![Pubkey::new_unique()],
            vec![],
            LoadedAddresses {
                writable: vec![],
                readonly: vec![],
            },
        );

        assert!(validate_account_locks(&message, MAX_TX_ACCOUNT_LOCKS).is_ok());
    }

    #[test]
    fn test_validate_account_locks_too_many() {
        let account_keys = vec![Pubkey::new_unique(), Pubkey::new_unique()];

        let message: SanitizedMessage = create_message_for_test(
            1,
            2,
            account_keys.clone(),
            vec![],
            LoadedAddresses {
                writable: account_keys,
                readonly: vec![],
            },
        );

        assert_eq!(
            validate_account_locks(&message, 1),
            Err(TransactionError::TooManyAccountLocks)
        );
    }

    #[test]
    fn test_validate_account_locks_duplicate() {
        let duplicate_key: Pubkey = Pubkey::new_unique();
        let message: SanitizedMessage = create_message_for_test(
            1,
            2,
            vec![duplicate_key, duplicate_key],
            vec![],
            LoadedAddresses {
                writable: vec![],
                readonly: vec![],
            },
        );
        assert_eq!(
            validate_account_locks(&message, MAX_TX_ACCOUNT_LOCKS),
            Err(TransactionError::AccountLoadedTwice)
        );
    }

    #[test]
    fn test_has_duplicates_small() {
        let mut keys = (0..16).map(|_| Pubkey::new_unique()).collect::<Vec<_>>();
        let account_keys = AccountKeys::new(&keys, None);
        assert!(!has_duplicates(account_keys));

        keys[14] = keys[3]; // Duplicate key
        let account_keys = AccountKeys::new(&keys, None);
        assert!(has_duplicates(account_keys));
    }

    #[test]
    fn test_has_duplicates_large() {
        let mut keys = (0..64).map(|_| Pubkey::new_unique()).collect::<Vec<_>>();
        let account_keys = AccountKeys::new(&keys, None);
        assert!(!has_duplicates(account_keys));

        keys[47] = keys[3]; // Duplicate key
        let account_keys = AccountKeys::new(&keys, None);
        assert!(has_duplicates(account_keys));
    }
}
