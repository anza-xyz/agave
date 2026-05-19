use {
    super::{BatchCommitMode, BundleExecution, NoBundleExecution, PacketIngress, TpuStage},
    agave_banking_stage_ingress_types::BankingPacketReceiver,
    crossbeam_channel::Receiver,
    solana_perf::packet::PacketBatch,
    std::{
        sync::{Arc, LazyLock, atomic::AtomicBool},
        thread::JoinHandle,
    },
};

static NO_BUNDLE_EXECUTION: LazyLock<Arc<dyn BundleExecution>> =
    LazyLock::new(|| Arc::new(NoBundleExecution));

/// Startup context passed to TPU stage factories.
///
/// Agave owns the internal channels and exposes
/// only narrow handles that extension stages need to start. Long-lived
/// validator-level objects (`cluster_info`, `bank_forks`, `poh_recorder`) are
/// validator-lifetime singletons — fork factories should capture them at
/// construction time (before calling `Validator::new_with_exit_with_tpu_extensions`),
/// not request them through this context.
pub trait TpuStageContext {
    fn packet_ingress(&self) -> &PacketIngress;

    /// Only one stage should take this — a second call returns `None`.
    /// Returns `None` if [`TpuExtensionsBuilder::with_packet_receiver`] was not enabled.
    fn take_packet_receiver(&self) -> Option<Receiver<PacketBatch>>;

    fn exit(&self) -> Arc<AtomicBool>;

    /// Returns the no-op implementation if no bundle runtime was wired by Agave.
    fn bundle_execution(&self) -> Arc<dyn BundleExecution> {
        Arc::clone(&NO_BUNDLE_EXECUTION)
    }
}

/// Builds a stage after Agave exposes its launch context.
///
/// Use for stages that need Agave-owned handles such as packet ingress or bundle
/// execution. Stages that need no context can be passed directly with
/// [`TpuExtensionsBuilder::processing_stage`] or [`TpuExtensionsBuilder::intake_stage`].
pub trait TpuStageFactory: Send + 'static {
    fn spawn(self: Box<Self>, context: &dyn TpuStageContext) -> Box<dyn TpuStage>;
}

/// Whether an extension worker pool runs beside or instead of Agave's non-vote scheduler.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BankingSchedulerMode {
    /// Keep Agave's built-in scheduler and worker threads.
    KeepInternal,
    /// Do not spawn Agave's non-vote scheduler. The extension pool owns non-vote
    /// intake from [`BankingStageContext::non_vote_receiver`].
    ReplaceInternal,
}

impl BankingSchedulerMode {
    pub const fn replaces_internal(self) -> bool {
        matches!(self, Self::ReplaceInternal)
    }
}

/// Startup context passed to fork-owned banking worker pools.
///
/// Intentionally narrow: no dynamic dispatch on the packet scan hot path.
pub trait BankingStageContext {
    fn non_vote_receiver(&self) -> BankingPacketReceiver;
    fn tpu_vote_receiver(&self) -> BankingPacketReceiver;
    fn gossip_vote_receiver(&self) -> BankingPacketReceiver;
    fn worker_exit_signal(&self) -> Arc<AtomicBool>;
    fn commit_mode(&self) -> BatchCommitMode;
}

/// Factory for extension-owned banking worker pools such as Jito BAM.
///
/// Returning thread handles lets Agave own shutdown/join without knowing the concrete pool type.
pub trait BankingWorkerPoolFactory: Send + Sync + 'static {
    fn scheduler_mode(&self) -> BankingSchedulerMode {
        BankingSchedulerMode::KeepInternal
    }

    fn spawn_threads(&self, context: &dyn BankingStageContext) -> Vec<JoinHandle<()>>;
}
