use {
    super::BankingHooksBuilder,
    crate::{
        AccountFilter, BatchCommitMode, ExternalLocks, NoExternalLocks, NoFilter, NoGate, NoTip,
        SchedulerGate, TipProcessor,
    },
    std::sync::Arc,
};

/// Cold-path configuration for [`BankingHooks`].
///
/// Validator-level config only — services (tip processor, filters) stay on
/// [`BankingHooks`] directly.
#[derive(Clone)]
pub struct BankingConfig {
    pub commit_mode: BatchCommitMode,
}

impl Default for BankingConfig {
    fn default() -> Self {
        Self {
            commit_mode: BatchCommitMode::standard(),
        }
    }
}

pub struct BankingHooks<F = NoFilter, G = NoGate, L = NoExternalLocks>
where
    F: AccountFilter,
    G: SchedulerGate,
    L: ExternalLocks,
{
    pub(crate) scheduler_gate: Arc<G>,
    pub(crate) packet_filter: Arc<F>,
    pub(crate) external_locks: Arc<L>,
    pub(crate) tip_processor: Arc<dyn TipProcessor>,
    pub(crate) config: BankingConfig,
}

impl BankingHooks<NoFilter, NoGate, NoExternalLocks> {
    pub fn builder() -> BankingHooksBuilder<NoFilter, NoGate, NoExternalLocks> {
        BankingHooksBuilder::default()
    }
}

impl<F: AccountFilter, G: SchedulerGate, L: ExternalLocks> BankingHooks<F, G, L> {
    pub(super) fn new(
        scheduler_gate: Arc<G>,
        packet_filter: Arc<F>,
        external_locks: Arc<L>,
        tip_processor: Arc<dyn TipProcessor>,
        config: BankingConfig,
    ) -> Self {
        Self {
            scheduler_gate,
            packet_filter,
            external_locks,
            tip_processor,
            config,
        }
    }

    pub fn scheduler_gate(&self) -> &G {
        self.scheduler_gate.as_ref()
    }

    pub fn packet_filter(&self) -> &F {
        self.packet_filter.as_ref()
    }

    /// Returns a cloned Arc for sharing `packet_filter` across threads that
    /// require the concrete type (e.g. `VotePacketReceiver<F>`).
    pub fn packet_filter_handle(&self) -> Arc<F> {
        Arc::clone(&self.packet_filter)
    }

    pub fn external_locks(&self) -> &L {
        self.external_locks.as_ref()
    }

    pub fn tip_processor_handle(&self) -> Arc<dyn TipProcessor> {
        Arc::clone(&self.tip_processor)
    }

    pub fn config(&self) -> &BankingConfig {
        &self.config
    }

    pub fn commit_mode(&self) -> BatchCommitMode {
        self.config.commit_mode
    }
}

impl Default for BankingHooks<NoFilter, NoGate, NoExternalLocks> {
    fn default() -> Self {
        Self::new(
            Arc::new(NoGate),
            Arc::new(NoFilter),
            Arc::new(NoExternalLocks),
            Arc::new(NoTip),
            BankingConfig::default(),
        )
    }
}

impl<F: AccountFilter, G: SchedulerGate, L: ExternalLocks> Clone for BankingHooks<F, G, L> {
    fn clone(&self) -> Self {
        Self {
            scheduler_gate: Arc::clone(&self.scheduler_gate),
            packet_filter: Arc::clone(&self.packet_filter),
            external_locks: Arc::clone(&self.external_locks),
            tip_processor: Arc::clone(&self.tip_processor),
            config: self.config.clone(),
        }
    }
}
