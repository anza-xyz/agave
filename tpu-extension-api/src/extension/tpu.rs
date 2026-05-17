use {
    crate::{
        AccountFilter, BankingHooks, BankingWorkerPoolFactory, ExternalLocks, NoExternalLocks,
        NoFilter, NoGate, SchedulerGate, TpuStage, TpuStageContext, TpuStageFactory,
    },
    std::{boxed::Box, sync::Arc},
};

/// A TPU extension stage that is either already spawned or needs Agave's launch context.
///
/// Use [`running`](Self::running) for stages spawned before validator startup (e.g. a
/// sigverify thread that only needs inter-stage channels). Use [`factory`](Self::factory)
/// for stages that need Agave-created handles (e.g. a bundle executor that needs
/// `bundle_execution`).
pub enum TpuStageSpec {
    Running(Box<dyn TpuStage>),
    Factory(Box<dyn TpuStageFactory>),
}

impl TpuStageSpec {
    /// Wrap an already-spawned stage.
    pub fn running(stage: impl TpuStage) -> Self {
        Self::Running(Box::new(stage))
    }

    /// Wrap a factory that spawns after Agave exposes its launch context.
    pub fn factory(factory: impl TpuStageFactory) -> Self {
        Self::Factory(Box::new(factory))
    }

    pub fn spawn(self, context: &dyn TpuStageContext) -> Box<dyn TpuStage> {
        match self {
            Self::Running(stage) => stage,
            Self::Factory(factory) => factory.spawn(context),
        }
    }
}

pub type BankingWorkerPoolFactories = Vec<Arc<dyn BankingWorkerPoolFactory>>;

/// How Agave wires its raw packet intake to sigverify.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketIntakeMode {
    /// Fetch/QUIC packets flow directly into Agave sigverify.
    Direct,
    /// Fetch/QUIC packets flow through [`PacketIngress::packet_receiver`](crate::PacketIngress).
    ///
    /// The extension is responsible for forwarding accepted packets with
    /// [`PacketIngress::send_to_sigverify`](crate::PacketIngress::send_to_sigverify).
    Routed,
}

impl PacketIntakeMode {
    pub const fn is_routed(self) -> bool {
        matches!(self, Self::Routed)
    }
}

/// TPU extension pieces after startup handoff.
pub struct TpuExtensionParts<F, G, L>
where
    F: AccountFilter,
    G: SchedulerGate,
    L: ExternalLocks,
{
    pub stages: Vec<TpuStageSpec>,
    pub banking_worker_pools: BankingWorkerPoolFactories,
    pub banking: BankingHooks<F, G, L>,
    pub packet_intake_mode: PacketIntakeMode,
}

/// Bundles custom TPU-level stages, banking worker pools, and BankingStage hooks.
///
/// Stages live at the Agave `Tpu` level; banking worker pools and
/// [`BankingHooks`] live inside `BankingStage`.
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
    pub stages: Vec<TpuStageSpec>,
    pub banking_worker_pools: BankingWorkerPoolFactories,
    pub banking: BankingHooks<F, G, L>,
    pub packet_intake_mode: PacketIntakeMode,
}

impl<F: AccountFilter, G: SchedulerGate, L: ExternalLocks> TpuExtensions<F, G, L> {
    pub fn into_parts(self) -> TpuExtensionParts<F, G, L> {
        TpuExtensionParts {
            stages: self.stages,
            banking_worker_pools: self.banking_worker_pools,
            banking: self.banking,
            packet_intake_mode: self.packet_intake_mode,
        }
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
            banking_worker_pools: Vec::new(),
            banking: BankingHooks::default(),
            packet_intake_mode: PacketIntakeMode::Direct,
        }
    }
}

pub struct TpuExtensionsBuilder<F = NoFilter, G = NoGate, L = NoExternalLocks>
where
    F: AccountFilter,
    G: SchedulerGate,
    L: ExternalLocks,
{
    processing_stages: Vec<TpuStageSpec>,
    intake_stages: Vec<TpuStageSpec>,
    banking_worker_pools: BankingWorkerPoolFactories,
    banking: BankingHooks<F, G, L>,
    packet_intake_mode: PacketIntakeMode,
}

impl Default for TpuExtensionsBuilder<NoFilter, NoGate, NoExternalLocks> {
    fn default() -> Self {
        Self {
            processing_stages: Vec::new(),
            intake_stages: Vec::new(),
            banking_worker_pools: Vec::new(),
            banking: BankingHooks::default(),
            packet_intake_mode: PacketIntakeMode::Direct,
        }
    }
}

impl<F: AccountFilter, G: SchedulerGate, L: ExternalLocks> TpuExtensionsBuilder<F, G, L> {
    /// Add a processing stage (e.g. bundle executor).
    ///
    /// Processing stages are aborted last on shutdown, giving them time to drain
    /// before intake is cut off. Pass [`TpuStageSpec::running`] for stages spawned
    /// before validator startup, or [`TpuStageSpec::factory`] for stages that need
    /// Agave's launch context.
    pub fn processing_stage(mut self, spec: TpuStageSpec) -> Self {
        self.processing_stages.push(spec);
        self
    }

    /// Add an intake stage (e.g. block-engine relay, relayer, fetch manager).
    ///
    /// Intake stages are aborted first on shutdown, cutting off new work before
    /// processing stages are stopped. Pass [`TpuStageSpec::running`] for stages
    /// spawned before validator startup, or [`TpuStageSpec::factory`] for stages
    /// that need Agave's launch context.
    pub fn intake_stage(mut self, spec: TpuStageSpec) -> Self {
        self.intake_stages.push(spec);
        self
    }

    /// Register a fork-owned banking worker pool such as Jito BAM.
    pub fn banking_worker_pool(mut self, pool: impl BankingWorkerPoolFactory) -> Self {
        self.banking_worker_pools.push(Arc::new(pool));
        self
    }

    /// Route Agave's raw packet intake through an extension-owned receiver.
    ///
    /// This is the `with_packet_receiver` shape Jito needs for its
    /// fetch-stage manager: Agave still owns the sockets, while the extension
    /// owns routing policy before packets enter sigverify.
    pub fn with_packet_receiver(mut self) -> Self {
        self.packet_intake_mode = PacketIntakeMode::Routed;
        self
    }

    pub fn banking_hooks<NF: AccountFilter, NG: SchedulerGate, NL: ExternalLocks>(
        self,
        banking: BankingHooks<NF, NG, NL>,
    ) -> TpuExtensionsBuilder<NF, NG, NL> {
        TpuExtensionsBuilder {
            processing_stages: self.processing_stages,
            intake_stages: self.intake_stages,
            banking_worker_pools: self.banking_worker_pools,
            banking,
            packet_intake_mode: self.packet_intake_mode,
        }
    }

    pub fn build(self) -> TpuExtensions<F, G, L> {
        let Self {
            mut processing_stages,
            intake_stages,
            banking_worker_pools,
            banking,
            packet_intake_mode,
        } = self;
        processing_stages.extend(intake_stages);
        TpuExtensions {
            stages: processing_stages,
            banking_worker_pools,
            banking,
            packet_intake_mode,
        }
    }
}
