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
/// Implementations must be non-blocking — holding a mutex here stalls all packet
/// processing.
pub trait SchedulerGate: Send + Sync + 'static {
    fn should_yield(&self) -> bool;
}

/// Per-packet account key check in the hot path.
pub trait AccountFilter: Send + Sync + 'static {
    fn is_blocked(&self, pubkey: &Pubkey) -> bool;

    /// Keep this check O(1); it gates the per-packet account-key scan.
    fn is_active(&self) -> bool {
        true
    }
}

/// Per-packet account lock check in the hot path.
///
/// Must be lock-free or read-biased: contention here directly delays packet intake.
/// TOCTOU is expected; ~50 µs staleness is acceptable.
pub trait ExternalLocks: Send + Sync + 'static {
    fn is_write_locked(&self, pubkey: &Pubkey) -> bool;

    /// Default preserves the original write-lock-only contract.
    fn is_read_locked(&self, _: &Pubkey) -> bool {
        false
    }

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

/// Separate read-lock view for code that inspects both lock dimensions via [`BundleAccountLockView`].
///
/// The scheduler hot path uses [`ExternalLocks::conflicts`] instead; this trait
/// exists so a single `Arc<T>` can satisfy both supertraits at once.
pub trait ReadLockView: Send + Sync + 'static {
    fn is_read_locked(&self, pubkey: &Pubkey) -> bool;

    /// Same contract as [`ExternalLocks::is_active`].
    fn is_active(&self) -> bool {
        true
    }
}

/// Combined lock view — implement on the same concrete type as [`ExternalLocks`] + [`ReadLockView`]
/// so the scheduler can accept a single `Arc<T>` for both dimensions.
pub trait BundleAccountLockView: ExternalLocks + ReadLockView {}
