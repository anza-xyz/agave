/// Called once per leader-slot transition for fork-specific setup (e.g. tip-distribution init).
///
/// **Idempotency:** the scheduler may call `process` multiple times for the
/// same slot (e.g. if the validator is re-elected leader for an epoch it already
/// processed). Implementations must detect and silently skip already-done work.
///
/// **Error policy:** if initialization fails fatally, `panic!`. The scheduler
/// thread propagates the panic and triggers validator shutdown. Non-fatal
/// conditions (already initialized, nothing to do) must be swallowed.
///
/// **Configuration:** any validator-level parameters the implementation needs
/// (e.g. fee-payer keypair, tip account pubkeys) should be given to the
/// `TipProcessor` at construction time, not through [`TipContext`].
pub trait TipProcessor: Send + Sync + 'static {
    fn process(&self, ctx: &TipContext);
}

/// `#[non_exhaustive]` so fields can be added without breaking existing implementations.
#[non_exhaustive]
pub struct TipContext {
    pub slot: u64,
    /// Useful for once-per-epoch initialization.
    pub epoch: u64,
}

impl TipContext {
    pub fn new(slot: u64, epoch: u64) -> Self {
        Self { slot, epoch }
    }
}
