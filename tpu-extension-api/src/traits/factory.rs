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
/// Mirrors Reth's `BuilderContext`: Agave owns the internal channels and exposes
/// only narrow handles that extension stages need to start. Long-lived
/// validator-level objects (`cluster_info`, `bank_forks`, `poh_recorder`) are
/// validator-lifetime singletons — fork factories should capture them at
/// construction time (before calling `Validator::new_with_exit_with_tpu_extensions`),
/// not request them through this context.
pub trait TpuStageContext {
    /// Cloneable packet ingress handle for sending packets into sigverify or
    /// trusted banking. Senders are freely cloneable.
    fn packet_ingress(&self) -> &PacketIngress;

    /// Take the raw Agave packet receiver for fork-owned intake routing.
    ///
    /// Only one stage should take this — calling it a second time returns `None`.
    /// Returns `None` if [`TpuExtensionsBuilder::with_packet_receiver`] was not
    /// enabled.
    fn take_packet_receiver(&self) -> Option<Receiver<PacketBatch>>;

    /// Validator-wide exit signal. Stages should stop when this is set.
    fn exit(&self) -> Arc<AtomicBool>;

    /// Runtime bridge for bundle execution.
    ///
    /// Returns the no-op implementation if no bundle runtime was wired by Agave.
    fn bundle_execution(&self) -> Arc<dyn BundleExecution> {
        Arc::clone(&NO_BUNDLE_EXECUTION)
    }
}

/// Builds a TPU stage after Agave has created its internal launch context.
///
/// Use this when a stage needs Agave-owned handles such as packet ingress. Stages
/// that do not need context can still be passed directly with
/// `TpuExtensionsBuilder::processing_stage` or `intake_stage`.
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
/// This is intentionally a narrow surface. Packet receivers, exit signal, and
/// commit mode are enough to show how a fork can own extra or replacement
/// workers without putting dynamic dispatch on Agave's packet scan hot path.
pub trait BankingStageContext {
    fn non_vote_receiver(&self) -> BankingPacketReceiver;
    fn tpu_vote_receiver(&self) -> BankingPacketReceiver;
    fn gossip_vote_receiver(&self) -> BankingPacketReceiver;
    fn worker_exit_signal(&self) -> Arc<AtomicBool>;
    fn commit_mode(&self) -> BatchCommitMode;
}

/// Factory for extension-owned banking worker pools such as Jito BAM.
///
/// Factories are invoked only when BankingStage starts or restarts its worker
/// set. Returning thread handles lets Agave own shutdown/join without knowing the
/// concrete pool type.
pub trait BankingWorkerPoolFactory: Send + Sync + 'static {
    fn scheduler_mode(&self) -> BankingSchedulerMode {
        BankingSchedulerMode::KeepInternal
    }

    fn spawn_threads(&self, context: &dyn BankingStageContext) -> Vec<JoinHandle<()>>;
}
