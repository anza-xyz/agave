use {
    agave_tpu_extension_api::ExternalLocks,
    solana_pubkey::Pubkey,
    std::{
        collections::HashSet,
        sync::{
            RwLock,
            atomic::{AtomicUsize, Ordering},
        },
    },
};

/// Tracks accounts write-locked by in-flight bundles.
///
/// `BundleStage` calls `lock`/`unlock` around each bundle; the scheduler
/// calls `is_write_locked` to skip any transaction that would conflict.
/// The atomic count makes the hot-path `is_active` guard a single load.
pub struct BundleExternalLocks {
    write_locked_accounts: RwLock<HashSet<Pubkey>>,
    read_locked_accounts: RwLock<HashSet<Pubkey>>,
    locked_account_count: AtomicUsize,
}

impl BundleExternalLocks {
    pub fn new() -> Self {
        Self {
            write_locked_accounts: RwLock::new(HashSet::new()),
            read_locked_accounts: RwLock::new(HashSet::new()),
            locked_account_count: AtomicUsize::new(0),
        }
    }

    pub fn lock_write(&self, pubkey: Pubkey) {
        if self.write_locked_accounts.write().unwrap().insert(pubkey) {
            self.locked_account_count.fetch_add(1, Ordering::Release);
        }
    }

    pub fn lock_read(&self, pubkey: Pubkey) {
        if self.read_locked_accounts.write().unwrap().insert(pubkey) {
            self.locked_account_count.fetch_add(1, Ordering::Release);
        }
    }

    pub fn unlock_write(&self, pubkey: &Pubkey) {
        if self.write_locked_accounts.write().unwrap().remove(pubkey) {
            self.locked_account_count.fetch_sub(1, Ordering::Release);
        }
    }

    pub fn unlock_read(&self, pubkey: &Pubkey) {
        if self.read_locked_accounts.write().unwrap().remove(pubkey) {
            self.locked_account_count.fetch_sub(1, Ordering::Release);
        }
    }
}

impl ExternalLocks for BundleExternalLocks {
    #[inline(always)]
    fn is_write_locked(&self, pubkey: &Pubkey) -> bool {
        self.write_locked_accounts.read().unwrap().contains(pubkey)
    }

    #[inline(always)]
    fn is_read_locked(&self, pubkey: &Pubkey) -> bool {
        self.read_locked_accounts.read().unwrap().contains(pubkey)
    }

    #[inline(always)]
    fn is_active(&self) -> bool {
        self.locked_account_count.load(Ordering::Acquire) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_round_trip() {
        let locks = BundleExternalLocks::new();
        let account = Pubkey::new_from_array([1u8; 32]);
        assert!(!locks.is_active());
        locks.lock_write(account);
        assert!(locks.is_active());
        assert!(locks.is_write_locked(&account));
        locks.unlock_write(&account);
        assert!(!locks.is_active());
        assert!(!locks.is_write_locked(&account));
    }
}
