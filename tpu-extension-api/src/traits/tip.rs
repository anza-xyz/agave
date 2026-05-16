/// Called once per leader-slot transition to perform fork-specific setup.
///
/// The canonical use case is initializing tip-distribution PDAs (Jito-Solana),
/// but implementations may do any per-slot leader-side work here.
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

/// Contextual information passed to [`TipProcessor::process`] at each
/// leader-slot transition.
///
/// Marked `#[non_exhaustive]` so fields can be added in future API versions
/// without breaking existing implementations.
#[non_exhaustive]
pub struct TipContext {
    /// The slot the validator is about to lead.
    pub slot: u64,
    /// The epoch containing `slot`. Useful for once-per-epoch initialization.
    pub epoch: u64,
}

impl TipContext {
    pub fn new(slot: u64, epoch: u64) -> Self {
        Self { slot, epoch }
    }
}
