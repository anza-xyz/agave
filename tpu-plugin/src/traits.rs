use {solana_pubkey::Pubkey, std::thread, thiserror::Error};

// abort() must be idempotent; separate from join() to allow concurrent draining.
pub trait LifecycleStage: Send + 'static {
    fn abort(&self);
    fn join(self: Box<Self>) -> thread::Result<()>;
}

/// A custom pipeline stage that runs alongside the TPU.
/// Out of scope for the initial wiring: bundle transaction injection (bypassing sigverify)
/// is addressed in a separate RFC.
pub trait TpuStage: LifecycleStage {}

// Must be non-blocking and lock-free.
pub trait YieldControl: Send + Sync + 'static {
    fn should_yield(&self) -> bool;
}

pub trait AccountFilter: Send + Sync + 'static {
    fn is_blocked(&self, pubkey: &Pubkey) -> bool;
    // Returns false when the filter has no entries, allowing O(1) short-circuit
    // before iterating all account keys in the packet.
    fn is_active(&self) -> bool { true }
}

/// Dynamic per-slot account lock view, typically backed by a `RwLock<HashSet<Pubkey>>`
/// that the bundle processor updates as bundles take and release account locks mid-slot.
/// Called per packet per scheduling cycle — implementation must be lock-free or read-biased.
// TOCTOU is expected; ~50 µs staleness is acceptable.
pub trait WriteLockView: Send + Sync + 'static {
    fn is_write_locked(&self, pubkey: &Pubkey) -> bool;
}

pub trait ReadLockView: Send + Sync + 'static {
    fn is_read_locked(&self, pubkey: &Pubkey) -> bool;
}

pub trait BundleAccountLockView: WriteLockView + ReadLockView {}

// Must be idempotent — AlreadyInitialized is non-fatal.
pub trait TipProcessor: Send + Sync + 'static {
    fn process(&self, ctx: &TipContext<'_>) -> Result<(), TipProcessorError>;
}

pub trait BatchCommitPolicy: Send + Sync + 'static {
    fn revert_batch_on_error(&self) -> bool;
    // When true, Consumer partitions the bundle into conflict-free PoH entries.
    // Required for Jito bundles, which can contain write-conflicting transactions.
    fn partition_into_entries(&self) -> bool;
}

#[non_exhaustive]
pub struct TipContext<'a> {
    pub slot: u64,
    pub epoch: u64,
    pub validator_identity: &'a Pubkey,
    pub block_builder_pubkey: Pubkey,
    pub block_builder_commission_bps: u16,
}

impl<'a> TipContext<'a> {
    pub fn new(
        slot: u64,
        epoch: u64,
        validator_identity: &'a Pubkey,
        block_builder_pubkey: Pubkey,
        block_builder_commission_bps: u16,
    ) -> Self {
        Self { slot, epoch, validator_identity, block_builder_pubkey, block_builder_commission_bps }
    }
}

#[derive(Debug, Error)]
pub enum TipProcessorError {
    /// Tip PDAs for this epoch were already initialized (e.g. a re-elected leader
    /// or concurrent initializer). Callers should treat this as success.
    #[error("tip PDAs already initialized for epoch {0}")]
    AlreadyInitialized(u64),
    #[error("tip PDA initialization failed for slot {slot}: {reason}")]
    InitializationFailed { slot: u64, reason: String },
}
