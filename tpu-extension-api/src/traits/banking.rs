use solana_pubkey::Pubkey;

/// Lets an extension pause packet scheduling while it owns a critical section.
///
/// Called at the top of every scheduling iteration. Return `true` to yield; the
/// scheduler spins (with a short sleep) until `should_yield` returns `false`.
/// Implementations must be non-blocking — holding a mutex here stalls all packet
/// processing.
///
/// [`NoGate`](crate::NoGate) inlines to `false`; the compiler eliminates the branch
/// entirely on the vanilla path.
pub trait SchedulerGate: Send + Sync + 'static {
    fn should_yield(&self) -> bool;
}

/// Rejects packets whose static account keys overlap with extension-owned accounts.
///
/// Called per packet in the hot path. If [`is_active`](Self::is_active) returns
/// `false`, the whole scan is skipped. This lets vanilla validators pay zero cost
/// when no filter is configured.
///
/// [`NoFilter`](crate::NoFilter) inlines both methods to `false`; the compiler
/// eliminates the filter branch entirely on the vanilla path.
pub trait AccountFilter: Send + Sync + 'static {
    fn is_blocked(&self, pubkey: &Pubkey) -> bool;

    /// `false` means "never blocked"; the scheduler skips the per-key scan entirely.
    /// Must return a stable value after construction — the scheduler caches the result.
    fn is_active(&self) -> bool {
        true
    }
}

/// Dynamic write-lock view over accounts currently owned by in-flight extension work.
///
/// Called per packet per scheduling cycle to prevent Agave from scheduling
/// transactions that conflict with extension-owned in-flight bundles. Must be
/// lock-free or read-biased: contention here directly delays packet intake.
/// TOCTOU is expected; ~50 µs staleness is acceptable.
///
/// [`NoExternalLocks`](crate::NoExternalLocks) inlines both methods to `false`;
/// the branch is eliminated entirely on the vanilla path.
pub trait ExternalLocks: Send + Sync + 'static {
    fn is_write_locked(&self, pubkey: &Pubkey) -> bool;

    /// `false` means no write locks are held; the scheduler skips the per-key scan.
    fn is_active(&self) -> bool {
        true
    }
}

/// Reserved read-lock view for a future packet-injection contract.
///
/// Paired with [`ExternalLocks`] to form [`BundleAccountLockView`]: together they
/// express both sides of the lock set that a bundle executor holds while a bundle
/// is in-flight. Not used by the current scheduler; reserved for the packet-injection
/// follow-up RFC.
pub trait ReadLockView: Send + Sync + 'static {
    fn is_read_locked(&self, pubkey: &Pubkey) -> bool;

    /// Same contract as [`ExternalLocks::is_active`]; reserved for future use.
    fn is_active(&self) -> bool {
        true
    }
}

/// Combined lock view for bundle executors that hold both read and write locks.
///
/// Implement this on the same type that implements [`ExternalLocks`] and
/// [`ReadLockView`] so the scheduler controller can accept a single shared
/// `Arc<T>` and query both lock dimensions through it.
pub trait BundleAccountLockView: ExternalLocks + ReadLockView {}
