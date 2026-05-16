use {
    crate::{AccountFilter, ExternalLocks, SchedulerGate, TipProcessor},
    std::sync::Arc,
};

/// Fully type-erased handles to the active banking hook instances.
///
/// `BankingHooks<F, G, L>` is generic so the hot-path hooks can be
/// monomorphized and inlined. Components that run on separate threads and
/// don't need static dispatch — e.g. the vote-packet receiver or the
/// tip-processor — obtain shared access through this type instead.
///
/// Obtained via [`BankingHooks::handles`] or [`TpuExtensions::handles`].
/// Cloning shares the same underlying `Arc`s; no additional allocation occurs.
#[derive(Clone)]
pub struct BankingHandles {
    /// Shared gate for components that need to check the scheduler yield signal.
    pub scheduler_gate: Arc<dyn SchedulerGate>,
    /// Shared filter for components that need to check packet account keys
    /// (e.g. vote-packet receiver) without requiring the concrete type.
    pub packet_filter: Arc<dyn AccountFilter>,
    /// Shared lock view for components that need to check external write locks
    /// without requiring the concrete type.
    pub external_locks: Arc<dyn ExternalLocks>,
    /// Shared tip processor, passed to the scheduler thread.
    pub tip_processor: Arc<dyn TipProcessor>,
}
