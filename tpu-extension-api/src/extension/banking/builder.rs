use {
    super::{BankingConfig, BankingHooks},
    crate::{
        AccountFilter, ExternalLocks, NoExternalLocks, NoFilter, NoGate, NoTip, SchedulerGate,
        TipProcessor,
    },
    std::sync::Arc,
};

pub struct BankingHooksBuilder<F = NoFilter, G = NoGate, L = NoExternalLocks>
where
    F: AccountFilter,
    G: SchedulerGate,
    L: ExternalLocks,
{
    scheduler_gate: Arc<G>,
    packet_filter: Arc<F>,
    external_locks: Arc<L>,
    tip_processor: Arc<dyn TipProcessor>,
    config: BankingConfig,
}

impl Default for BankingHooksBuilder<NoFilter, NoGate, NoExternalLocks> {
    fn default() -> Self {
        Self {
            scheduler_gate: Arc::new(NoGate),
            packet_filter: Arc::new(NoFilter),
            external_locks: Arc::new(NoExternalLocks),
            tip_processor: Arc::new(NoTip),
            config: BankingConfig::default(),
        }
    }
}

impl<F: AccountFilter, G: SchedulerGate, L: ExternalLocks> BankingHooksBuilder<F, G, L> {
    /// Set the scheduler gate. Type parameter `G` is updated to `NG`.
    pub fn scheduler_gate<NG: SchedulerGate>(self, gate: NG) -> BankingHooksBuilder<F, NG, L> {
        self.shared_scheduler_gate(Arc::new(gate))
    }

    /// Set a scheduler gate that is already shared with another stage.
    pub fn shared_scheduler_gate<NG: SchedulerGate>(
        self,
        gate: Arc<NG>,
    ) -> BankingHooksBuilder<F, NG, L> {
        BankingHooksBuilder {
            scheduler_gate: gate,
            packet_filter: self.packet_filter,
            external_locks: self.external_locks,
            tip_processor: self.tip_processor,
            config: self.config,
        }
    }

    /// Set the packet filter. Type parameter `F` is updated to `NF`.
    pub fn packet_filter<NF: AccountFilter>(self, filter: NF) -> BankingHooksBuilder<NF, G, L> {
        self.shared_packet_filter(Arc::new(filter))
    }

    /// Set a packet filter that is already shared with another component.
    pub fn shared_packet_filter<NF: AccountFilter>(
        self,
        filter: Arc<NF>,
    ) -> BankingHooksBuilder<NF, G, L> {
        BankingHooksBuilder {
            scheduler_gate: self.scheduler_gate,
            packet_filter: filter,
            external_locks: self.external_locks,
            tip_processor: self.tip_processor,
            config: self.config,
        }
    }

    /// Set the external locks view. Type parameter `L` is updated to `NL`.
    pub fn external_locks<NL: ExternalLocks>(self, locks: NL) -> BankingHooksBuilder<F, G, NL> {
        self.shared_external_locks(Arc::new(locks))
    }

    /// Set an external locks view that is already shared with another stage.
    pub fn shared_external_locks<NL: ExternalLocks>(
        self,
        locks: Arc<NL>,
    ) -> BankingHooksBuilder<F, G, NL> {
        BankingHooksBuilder {
            scheduler_gate: self.scheduler_gate,
            packet_filter: self.packet_filter,
            external_locks: locks,
            tip_processor: self.tip_processor,
            config: self.config,
        }
    }

    /// Set the tip processor. This is cold path: it runs on leader-slot transitions.
    pub fn tip_processor<T: TipProcessor>(self, tip_processor: T) -> Self {
        self.shared_tip_processor(Arc::new(tip_processor))
    }

    /// Set a tip processor that is already shared with another stage.
    pub fn shared_tip_processor<T: TipProcessor>(mut self, tip_processor: Arc<T>) -> Self {
        let tip_processor: Arc<dyn TipProcessor> = tip_processor;
        self.tip_processor = tip_processor;
        self
    }

    /// Set cold-path banking config.
    pub fn config(mut self, config: BankingConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> BankingHooks<F, G, L> {
        BankingHooks::new(
            self.scheduler_gate,
            self.packet_filter,
            self.external_locks,
            self.tip_processor,
            self.config,
        )
    }
}
