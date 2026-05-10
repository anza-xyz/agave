use {
    crate::{
        AccountFilter, BatchCommitPolicy, NoFilter, NoLocks, NoTip, NoYield, StandardCommit,
        TipProcessor, TpuStage, WriteLockView, YieldControl,
    },
    solana_pubkey::Pubkey,
    std::sync::Arc,
};

/// Config forwarded to [`crate::TipContext`] at each leader-slot transition.
///
/// All three fields default to `Pubkey::default()` / `0` on the vanilla path,
/// which means `NoTip::process()` is called but immediately returns `Ok(())`.
/// Fork implementations set real values via [`BankingHooks::new`].
#[derive(Clone, Copy, Default)]
pub struct TipConfig {
    pub validator_identity: Pubkey,
    pub block_builder_pubkey: Pubkey,
    pub block_builder_commission_bps: u16,
}

// Stages are aborted in reverse push order — push order is the public contract.
#[non_exhaustive]
pub struct TpuPlugin<F = NoFilter>
where
    F: AccountFilter,
{
    pub stages: Vec<Box<dyn TpuStage>>,
    pub banking: BankingHooks<F>,
}

impl<F: AccountFilter> TpuPlugin<F> {
    pub fn new(stages: Vec<Box<dyn TpuStage>>, banking: BankingHooks<F>) -> Self {
        Self { stages, banking }
    }
}

impl Default for TpuPlugin<NoFilter> {
    fn default() -> Self {
        Self { stages: Vec::new(), banking: BankingHooks::default() }
    }
}

// F = NoFilter monomorphises filter checks to zero cost on the vanilla path.
#[non_exhaustive]
pub struct BankingHooks<F = NoFilter>
where
    F: AccountFilter,
{
    pub yield_control: Arc<dyn YieldControl>,
    pub account_filter: Arc<F>,
    pub account_lock_view: Arc<dyn WriteLockView>,
    pub tip_processor: Arc<dyn TipProcessor>,
    pub batch_commit: Arc<dyn BatchCommitPolicy>,
    pub tip_config: TipConfig,
}

impl<F: AccountFilter> BankingHooks<F> {
    /// Construct a full hook set.
    ///
    /// Arguments 1–5 are positional `Arc<dyn …>` — transposition compiles silently.
    /// Use the field names as a checklist: `yield_control`, `account_filter`,
    /// `account_lock_view`, `tip_processor`, `batch_commit`.
    pub fn new(
        yield_control: Arc<dyn YieldControl>,
        account_filter: Arc<F>,
        account_lock_view: Arc<dyn WriteLockView>,
        tip_processor: Arc<dyn TipProcessor>,
        batch_commit: Arc<dyn BatchCommitPolicy>,
        tip_config: TipConfig,
    ) -> Self {
        Self {
            yield_control,
            account_filter,
            account_lock_view,
            tip_processor,
            batch_commit,
            tip_config,
        }
    }
}

impl Default for BankingHooks<NoFilter> {
    fn default() -> Self {
        Self::new(
            Arc::new(NoYield),
            Arc::new(NoFilter),
            Arc::new(NoLocks),
            Arc::new(NoTip),
            Arc::new(StandardCommit),
            TipConfig::default(),
        )
    }
}

impl<F: AccountFilter> Clone for BankingHooks<F> {
    fn clone(&self) -> Self {
        Self {
            yield_control: Arc::clone(&self.yield_control),
            account_filter: Arc::clone(&self.account_filter),
            account_lock_view: Arc::clone(&self.account_lock_view),
            tip_processor: Arc::clone(&self.tip_processor),
            batch_commit: Arc::clone(&self.batch_commit),
            tip_config: self.tip_config,
        }
    }
}
