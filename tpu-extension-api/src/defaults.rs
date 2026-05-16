use {
    crate::{
        AccountFilter, BatchCommitMode, BatchCommitPolicy, BundleAccountLockView, ExternalLocks,
        ReadLockView, SchedulerGate, TipContext, TipProcessor, TipProcessorError,
    },
    solana_pubkey::Pubkey,
    std::collections::HashSet,
};

pub struct NoGate;
pub struct NoFilter;
pub struct NoExternalLocks;
pub struct NoTip;
pub struct StandardCommit;

impl SchedulerGate for NoGate {
    #[inline(always)]
    fn should_yield(&self) -> bool {
        false
    }
}

impl AccountFilter for NoFilter {
    #[inline(always)]
    fn is_blocked(&self, _: &Pubkey) -> bool {
        false
    }
    #[inline(always)]
    fn is_active(&self) -> bool {
        false
    }
}

impl ExternalLocks for NoExternalLocks {
    #[inline(always)]
    fn is_write_locked(&self, _: &Pubkey) -> bool {
        false
    }
    #[inline(always)]
    fn is_active(&self) -> bool {
        false
    }
}

impl ReadLockView for NoExternalLocks {
    #[inline(always)]
    fn is_read_locked(&self, _: &Pubkey) -> bool {
        false
    }
    #[inline(always)]
    fn is_active(&self) -> bool {
        false
    }
}

impl BundleAccountLockView for NoExternalLocks {}

impl TipProcessor for NoTip {
    #[inline(always)]
    fn process(&self, _: &TipContext<'_>) -> Result<(), TipProcessorError> {
        Ok(())
    }
}

impl BatchCommitPolicy for StandardCommit {
    #[inline(always)]
    fn mode(&self) -> BatchCommitMode {
        BatchCommitMode::standard()
    }
}

/// Bridges `ValidatorConfig::filter_keys` (the `--filter-keys` CLI flag) to [`AccountFilter`].
pub struct SetAccountFilter(pub HashSet<Pubkey>);

impl AccountFilter for SetAccountFilter {
    #[inline(always)]
    fn is_blocked(&self, pubkey: &Pubkey) -> bool {
        self.0.contains(pubkey)
    }
    #[inline(always)]
    fn is_active(&self) -> bool {
        !self.0.is_empty()
    }
}
