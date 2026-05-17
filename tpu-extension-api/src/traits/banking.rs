use solana_pubkey::Pubkey;

/// Access kind for account-lock conflict checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountAccess {
    Read,
    Write,
}

impl AccountAccess {
    pub const fn from_is_writable(is_writable: bool) -> Self {
        if is_writable { Self::Write } else { Self::Read }
    }
}

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

    /// `false` means "never blocked"; receive paths skip the per-key scan entirely.
    /// Keep this check O(1); it runs before the per-packet account-key scan.
    fn is_active(&self) -> bool {
        true
    }
}

/// Dynamic account-lock view over accounts currently owned by in-flight extension work.
///
/// Called per packet per scheduling cycle to prevent Agave from scheduling
/// transactions that conflict with extension-owned in-flight bundles. Must be
/// lock-free or read-biased: contention here directly delays packet intake.
/// TOCTOU is expected; ~50 µs staleness is acceptable.
///
/// [`NoExternalLocks`](crate::NoExternalLocks) inlines all checks to `false`;
/// the branch is eliminated entirely on the vanilla path.
pub trait ExternalLocks: Send + Sync + 'static {
    fn is_write_locked(&self, pubkey: &Pubkey) -> bool;

    /// Returns whether an extension currently holds a read lock on `pubkey`.
    ///
    /// The default preserves the original write-lock-only contract.
    fn is_read_locked(&self, _: &Pubkey) -> bool {
        false
    }

    /// Returns whether a transaction access conflicts with extension-owned locks.
    #[inline(always)]
    fn conflicts(&self, pubkey: &Pubkey, access: AccountAccess) -> bool {
        match access {
            AccountAccess::Read => self.is_write_locked(pubkey),
            AccountAccess::Write => self.is_write_locked(pubkey) || self.is_read_locked(pubkey),
        }
    }

    /// `false` means no write locks are held; the scheduler skips the per-key scan.
    fn is_active(&self) -> bool {
        true
    }
}

/// Compatibility read-lock view for bundle executors.
///
/// The scheduler hot path uses [`ExternalLocks::conflicts`] because it can
/// decide read/write conflicts from a single concrete hook. [`ReadLockView`]
/// remains useful for code that wants to inspect both sides of a bundle lock set
/// through [`BundleAccountLockView`].
pub trait ReadLockView: Send + Sync + 'static {
    fn is_read_locked(&self, pubkey: &Pubkey) -> bool;

    /// Same contract as [`ExternalLocks::is_active`].
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
