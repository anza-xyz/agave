use {
    agave_tpu_plugin::{
        AccountFilter, BatchCommitPolicy, TipContext,
        TipProcessor, TipProcessorError, WriteLockView, YieldControl,
    },
    solana_pubkey::Pubkey,
    std::{
        collections::HashSet,
        sync::{Arc, Mutex, RwLock, atomic::{AtomicBool, Ordering}},
    },
};

#[allow(dead_code)]
pub struct BlockEngineConfig {
    pub block_engine_url: String,
    pub trust_packets: bool,
}

// The 8 mainnet Jito tip accounts (https://jito.network/docs/tip-accounts)
pub const TIP_ACCOUNTS: [&str; 8] = [
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "4xDsmeTWPNjgSVSS1VTfzFq3iHZhp77ffPkAmkZkdu71",
];

pub fn tip_account_pubkeys() -> impl Iterator<Item = Pubkey> {
    TIP_ACCOUNTS.iter().map(|s| s.parse().unwrap())
}

pub struct BundleFilter(HashSet<Pubkey>);

impl BundleFilter {
    pub fn jito_mainnet() -> Self {
        Self(tip_account_pubkeys().collect())
    }
}

impl AccountFilter for BundleFilter {
    #[inline(always)]
    fn is_blocked(&self, pubkey: &Pubkey) -> bool { self.0.contains(pubkey) }
    #[inline(always)]
    fn is_active(&self) -> bool { !self.0.is_empty() }
}

// Sharing is handled by the outer Arc at the call site; no inner Arc needed.
pub struct BundleLocks(RwLock<HashSet<Pubkey>>);

impl BundleLocks {
    pub fn new() -> Self {
        Self(RwLock::new(HashSet::new()))
    }

    #[allow(dead_code)]
    pub fn lock(&self, pubkey: Pubkey) { self.0.write().unwrap().insert(pubkey); }

    #[allow(dead_code)]
    pub fn unlock(&self, pubkey: &Pubkey) { self.0.write().unwrap().remove(pubkey); }
}

impl WriteLockView for BundleLocks {
    #[inline(always)]
    fn is_write_locked(&self, pubkey: &Pubkey) -> bool { self.0.read().unwrap().contains(pubkey) }
}

// ReadLockView and BundleAccountLockView have no call site in core yet (bundle injection
// requires a separate RFC). BundleLocks only needs WriteLockView for the current PR scope.

pub struct BundleYield(Arc<AtomicBool>);

impl BundleYield {
    pub fn new(flag: Arc<AtomicBool>) -> Self { Self(flag) }
}

impl YieldControl for BundleYield {
    #[inline(always)]
    fn should_yield(&self) -> bool { self.0.load(Ordering::Acquire) }
}

pub struct TipManager {
    #[allow(dead_code)]
    tip_accounts: HashSet<Pubkey>,
    // Real jito-solana submits a transaction to initialize the TipDistributionAccount PDA
    // once per epoch (PDA derivation uses epoch, not slot). This Mutex is the in-process
    // guard; the outer Arc<TipManager> handles sharing — no inner Arc needed.
    initialized_epochs: Mutex<HashSet<u64>>,
}

impl TipManager {
    pub fn new(tip_accounts: impl IntoIterator<Item = Pubkey>) -> Self {
        Self {
            tip_accounts: tip_accounts.into_iter().collect(),
            initialized_epochs: Mutex::new(HashSet::new()),
        }
    }
}

impl TipProcessor for TipManager {
    fn process(&self, ctx: &TipContext<'_>) -> Result<(), TipProcessorError> {
        // Guard is epoch-scoped: a re-elected leader in the same epoch must not
        // re-submit the initialization transaction (the PDA already exists on-chain).
        if !self.initialized_epochs.lock().unwrap().insert(ctx.epoch) {
            return Err(TipProcessorError::AlreadyInitialized(ctx.epoch));
        }
        // In production jito-solana: submit initialize_tip_distribution_account_tx here.
        Ok(())
    }
}

pub struct BundleBatchPolicy;

impl BatchCommitPolicy for BundleBatchPolicy {
    #[inline(always)]
    fn revert_batch_on_error(&self) -> bool { true }
    #[inline(always)]
    fn partition_into_entries(&self) -> bool { true }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tip_accounts_are_blocked() {
        let filter = BundleFilter::jito_mainnet();
        for s in TIP_ACCOUNTS {
            assert!(filter.is_blocked(&s.parse().unwrap()));
        }
    }

    #[test]
    fn non_tip_account_passes() {
        assert!(!BundleFilter::jito_mainnet().is_blocked(&Pubkey::default()));
    }

    #[test]
    fn lock_round_trip() {
        let locks = BundleLocks::new();
        let account = Pubkey::new_from_array([1u8; 32]);
        locks.lock(account);
        assert!(locks.is_write_locked(&account));
        locks.unlock(&account);
        assert!(!locks.is_write_locked(&account));
    }

    #[test]
    fn yield_flag_starts_false() {
        let flag = Arc::new(AtomicBool::new(false));
        let ctrl = BundleYield::new(Arc::clone(&flag));
        assert!(!ctrl.should_yield());
        flag.store(true, Ordering::Release);
        assert!(ctrl.should_yield());
    }

    #[test]
    fn bundle_commit_semantics() {
        assert!(BundleBatchPolicy.revert_batch_on_error());
        assert!(BundleBatchPolicy.partition_into_entries());
    }
}
