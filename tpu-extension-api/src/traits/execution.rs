use {
    agave_banking_stage_ingress_types::BankingPacketBatch, solana_pubkey::Pubkey,
    std::time::Duration,
};

/// How bundle transactions should be written into PoH entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BundleEntryPolicy {
    /// Pack all bundle transactions into a single PoH entry.
    ///
    /// Simpler and lower-latency, but fails atomically if any transaction in
    /// the bundle write-conflicts with another in the same entry.
    AllInOne,
    /// Preserve bundle order and split write-conflicting transactions into
    /// sequential entries before recording.
    ///
    /// Jito-Solana uses this policy so that bundles with internal conflicts
    /// still execute atomically — each conflict group becomes its own entry,
    /// and the entries are committed together.
    SplitConflicts,
}

/// Request to execute and record an extension-owned bundle.
///
/// The extension owns the bundle protocol and account-lock policy. Agave owns
/// the runtime details required to execute transactions, record entries, and
/// commit results to the working bank.
#[derive(Clone)]
pub struct BundleExecutionRequest {
    pub packets: BankingPacketBatch,
    pub read_locks: Vec<Pubkey>,
    pub write_locks: Vec<Pubkey>,
    pub entry_policy: BundleEntryPolicy,
    pub max_retry_duration: Duration,
}

impl BundleExecutionRequest {
    pub fn new(
        packets: BankingPacketBatch,
        read_locks: Vec<Pubkey>,
        write_locks: Vec<Pubkey>,
    ) -> Self {
        Self {
            packets,
            read_locks,
            write_locks,
            entry_policy: BundleEntryPolicy::SplitConflicts,
            max_retry_duration: Duration::from_millis(40),
        }
    }

    pub fn with_max_retry_duration(mut self, max_retry_duration: Duration) -> Self {
        self.max_retry_duration = max_retry_duration;
        self
    }
}

/// Result of extension bundle execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum BundleExecutionStatus {
    /// The full bundle was executed, recorded, and committed.
    Recorded,
    /// The bundle can be retried in the current leader slot.
    Retryable,
    /// The bundle failed permanently and should be dropped.
    Rejected,
    /// No bundle execution runtime was wired by Agave; drop the bundle.
    Unavailable,
}

/// Runtime bridge for extension-owned bundle executors.
///
/// This is a cold extension path: it is called by extension stages such as
/// Jito's `BundleStage`, not by Agave's packet scheduler while scanning normal
/// transactions.
pub trait BundleExecution: Send + Sync + 'static {
    fn execute_and_record_bundle(&self, request: BundleExecutionRequest) -> BundleExecutionStatus;
}

/// No-op bundle runtime for contexts that do not support bundle execution.
pub struct NoBundleExecution;

impl BundleExecution for NoBundleExecution {
    fn execute_and_record_bundle(&self, _: BundleExecutionRequest) -> BundleExecutionStatus {
        BundleExecutionStatus::Unavailable
    }
}
