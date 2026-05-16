use solana_pubkey::Pubkey;

/// Lets an extension briefly pause packet scheduling while it owns the critical section.
///
/// This is called in the packet receive loop, so implementations must be non-blocking.
pub trait SchedulerGate: Send + Sync + 'static {
    fn should_yield(&self) -> bool;
}

/// Rejects packets that reference accounts owned by an extension protocol.
pub trait AccountFilter: Send + Sync + 'static {
    fn is_blocked(&self, pubkey: &Pubkey) -> bool;

    /// Return false for lifetime no-op filters so the caller can skip the key scan.
    fn is_active(&self) -> bool {
        true
    }
}

/// Dynamic per-slot account lock view for extension-owned in-flight work.
///
/// Called per packet per scheduling cycle; implementations must be lock-free or
/// read-biased. TOCTOU is expected; around 50 us staleness is acceptable.
pub trait ExternalLocks: Send + Sync + 'static {
    fn is_write_locked(&self, pubkey: &Pubkey) -> bool;

    /// Return false when no external write locks are held so the caller can skip the key scan.
    fn is_active(&self) -> bool {
        true
    }
}

/// Reserved read-lock view for a future packet-injection contract.
pub trait ReadLockView: Send + Sync + 'static {
    fn is_read_locked(&self, pubkey: &Pubkey) -> bool;

    /// Reserved for the packet-injection follow-up; same lifetime contract as [`ExternalLocks`].
    fn is_active(&self) -> bool {
        true
    }
}

pub trait BundleAccountLockView: ExternalLocks + ReadLockView {}
