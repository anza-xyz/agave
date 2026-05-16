use {
    crate::{AccountFilter, ExternalLocks, SchedulerGate, TipProcessor},
    std::sync::Arc,
};

#[derive(Clone)]
pub struct BankingHandles {
    pub scheduler_gate: Arc<dyn SchedulerGate>,
    pub packet_filter: Arc<dyn AccountFilter>,
    pub external_locks: Arc<dyn ExternalLocks>,
    pub tip_processor: Arc<dyn TipProcessor>,
}
