use {
    crate::{
        AccountFilter, BankingHooks, ExternalLocks, NoExternalLocks, NoFilter, NoGate,
        SchedulerGate, TpuStage,
    },
    std::boxed::Box,
};

/// Bundles custom TPU-level stages with BankingStage-level hooks for the validator entry point.
///
/// Stages live at the Agave `Tpu` level; [`BankingHooks`] live inside `BankingStage`.
/// [`into_parts`](TpuExtensions::into_parts) separates them for hand-off to their
/// respective owners during startup.
///
/// Stage abort order is the reverse of push order (last pushed = first aborted).
/// Use [`TpuExtensionsBuilder::intake_stage`] and
/// [`TpuExtensionsBuilder::processing_stage`] to express intent explicitly.
pub struct TpuExtensions<F = NoFilter, G = NoGate, L = NoExternalLocks>
where
    F: AccountFilter,
    G: SchedulerGate,
    L: ExternalLocks,
{
    pub stages: Vec<Box<dyn TpuStage>>,
    pub banking: BankingHooks<F, G, L>,
}

impl<F: AccountFilter, G: SchedulerGate, L: ExternalLocks> TpuExtensions<F, G, L> {
    pub fn new(stages: Vec<Box<dyn TpuStage>>, banking: BankingHooks<F, G, L>) -> Self {
        Self { stages, banking }
    }

    pub fn into_parts(self) -> (Vec<Box<dyn TpuStage>>, BankingHooks<F, G, L>) {
        (self.stages, self.banking)
    }
}

impl TpuExtensions<NoFilter, NoGate, NoExternalLocks> {
    pub fn builder() -> TpuExtensionsBuilder<NoFilter, NoGate, NoExternalLocks> {
        TpuExtensionsBuilder::default()
    }
}

impl Default for TpuExtensions<NoFilter, NoGate, NoExternalLocks> {
    fn default() -> Self {
        Self::noop()
    }
}

impl TpuExtensions<NoFilter, NoGate, NoExternalLocks> {
    /// No-op extensions: no stages, all hooks are vanilla Agave defaults.
    pub fn noop() -> Self {
        Self {
            stages: Vec::new(),
            banking: BankingHooks::default(),
        }
    }
}

pub struct TpuExtensionsBuilder<F = NoFilter, G = NoGate, L = NoExternalLocks>
where
    F: AccountFilter,
    G: SchedulerGate,
    L: ExternalLocks,
{
    processing_stages: Vec<Box<dyn TpuStage>>,
    intake_stages: Vec<Box<dyn TpuStage>>,
    banking: BankingHooks<F, G, L>,
}

impl Default for TpuExtensionsBuilder<NoFilter, NoGate, NoExternalLocks> {
    fn default() -> Self {
        Self {
            processing_stages: Vec::new(),
            intake_stages: Vec::new(),
            banking: BankingHooks::default(),
        }
    }
}

impl<F: AccountFilter, G: SchedulerGate, L: ExternalLocks> TpuExtensionsBuilder<F, G, L> {
    /// Add a processing stage (long-running executor such as bundle execution).
    /// Processing stages are pushed first and therefore aborted last on shutdown,
    /// giving them time to drain before the intake is closed.
    pub fn processing_stage(mut self, stage: impl TpuStage) -> Self {
        self.processing_stages.push(Box::new(stage));
        self
    }

    /// Add an intake stage (upstream ingress such as a block-engine relay).
    /// Intake stages are pushed last and therefore aborted first on shutdown,
    /// cutting off new work before processing stages are stopped.
    pub fn intake_stage(mut self, stage: impl TpuStage) -> Self {
        self.intake_stages.push(Box::new(stage));
        self
    }

    /// Low-level escape hatch: push a stage at the back (aborted first).
    /// Prefer [`intake_stage`](Self::intake_stage) or
    /// [`processing_stage`](Self::processing_stage) to make ordering intent explicit.
    pub fn stage(mut self, stage: impl TpuStage) -> Self {
        self.intake_stages.push(Box::new(stage));
        self
    }

    pub fn stages(mut self, stages: Vec<Box<dyn TpuStage>>) -> Self {
        self.intake_stages.extend(stages);
        self
    }

    pub fn banking_hooks<NF: AccountFilter, NG: SchedulerGate, NL: ExternalLocks>(
        self,
        banking: BankingHooks<NF, NG, NL>,
    ) -> TpuExtensionsBuilder<NF, NG, NL> {
        TpuExtensionsBuilder {
            processing_stages: self.processing_stages,
            intake_stages: self.intake_stages,
            banking,
        }
    }

    pub fn build(self) -> TpuExtensions<F, G, L> {
        let Self {
            mut processing_stages,
            intake_stages,
            banking,
        } = self;
        processing_stages.extend(intake_stages);
        TpuExtensions::new(processing_stages, banking)
    }
}
