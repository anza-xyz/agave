use {
    crate::{
        AccountFilter, BatchCommitPolicy, BundleAccountLockView, ReadLockView,
        TipContext, TipProcessor, TipProcessorError, WriteLockView, YieldControl,
    },
    solana_pubkey::Pubkey,
    std::collections::HashSet,
};

pub struct NoYield;
pub struct NoFilter;
pub struct NoLocks;
pub struct NoTip;
pub struct StandardCommit;

impl YieldControl for NoYield {
    #[inline(always)]
    fn should_yield(&self) -> bool { false }
}

impl AccountFilter for NoFilter {
    #[inline(always)]
    fn is_blocked(&self, _: &Pubkey) -> bool { false }
    #[inline(always)]
    fn is_active(&self) -> bool { false }
}

impl WriteLockView for NoLocks {
    #[inline(always)]
    fn is_write_locked(&self, _: &Pubkey) -> bool { false }
}

impl ReadLockView for NoLocks {
    #[inline(always)]
    fn is_read_locked(&self, _: &Pubkey) -> bool { false }
}

impl BundleAccountLockView for NoLocks {}

impl TipProcessor for NoTip {
    #[inline(always)]
    fn process(&self, _: &TipContext<'_>) -> Result<(), TipProcessorError> { Ok(()) }
}

impl BatchCommitPolicy for StandardCommit {
    #[inline(always)]
    fn revert_batch_on_error(&self) -> bool { false }
    #[inline(always)]
    fn partition_into_entries(&self) -> bool { false }
}

/// Bridges `ValidatorConfig::filter_keys` (the `--filter-keys` CLI flag) to [`AccountFilter`].
pub struct SetAccountFilter(pub HashSet<Pubkey>);

impl AccountFilter for SetAccountFilter {
    #[inline(always)]
    fn is_blocked(&self, pubkey: &Pubkey) -> bool { self.0.contains(pubkey) }
    #[inline(always)]
    fn is_active(&self) -> bool { !self.0.is_empty() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BankingHooks, TipConfig, TpuPlugin};
    use std::sync::Arc;

    #[test]
    fn null_impls_are_no_ops() {
        let key = Pubkey::default();
        assert!(!NoYield.should_yield());
        assert!(!NoFilter.is_blocked(&key));
        assert!(!NoLocks.is_write_locked(&key));
        assert!(!NoLocks.is_read_locked(&key));
        assert!(!StandardCommit.revert_batch_on_error());
        assert!(!StandardCommit.partition_into_entries());
        let ctx = TipContext::new(0, 0, &key, key, 0);
        assert!(NoTip.process(&ctx).is_ok());
    }

    #[test]
    fn default_plugin_is_empty_and_no_op() {
        let plugin = TpuPlugin::default();
        assert!(plugin.stages.is_empty());
        assert!(!plugin.banking.yield_control.should_yield());
        assert!(!plugin.banking.account_filter.is_blocked(&Pubkey::default()));
        assert!(!plugin.banking.batch_commit.revert_batch_on_error());
        assert!(!plugin.banking.batch_commit.partition_into_entries());
    }

    #[test]
    fn banking_hooks_clone_shares_arcs() {
        let hooks = BankingHooks::new(
            Arc::new(NoYield),
            Arc::new(NoFilter),
            Arc::new(NoLocks),
            Arc::new(NoTip),
            Arc::new(StandardCommit),
            TipConfig::default(),
        );
        let cloned = hooks.clone();
        assert!(Arc::ptr_eq(&hooks.yield_control, &cloned.yield_control));
        assert!(Arc::ptr_eq(&hooks.account_filter, &cloned.account_filter));
        assert!(Arc::ptr_eq(&hooks.account_lock_view, &cloned.account_lock_view));
        assert!(Arc::ptr_eq(&hooks.tip_processor, &cloned.tip_processor));
        assert!(Arc::ptr_eq(&hooks.batch_commit, &cloned.batch_commit));
    }

    #[test]
    fn set_account_filter_delegates_to_hash_set() {
        let key = Pubkey::from([1u8; 32]);
        let filter = SetAccountFilter(HashSet::from([key]));
        assert!(filter.is_blocked(&key));
        assert!(!filter.is_blocked(&Pubkey::default()));
        assert!(filter.is_active());
        assert!(!SetAccountFilter(HashSet::new()).is_active());
    }
}
