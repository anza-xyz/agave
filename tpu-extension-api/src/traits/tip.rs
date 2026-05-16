use solana_pubkey::Pubkey;

/// Called at each leader-slot transition.
///
/// Implementations must be idempotent — the scheduler may call this multiple
/// times for the same slot. If setup fails in a way that is truly fatal, the
/// implementation should `panic!`; the scheduler thread will propagate the
/// panic and trigger validator shutdown. Non-fatal errors (e.g. already
/// initialized) should be swallowed silently.
pub trait TipProcessor: Send + Sync + 'static {
    fn process(&self, ctx: &TipContext<'_>);
}

#[non_exhaustive]
pub struct TipContext<'a> {
    pub slot: u64,
    pub epoch: u64,
    pub validator_fee_payer: &'a Pubkey,
}

impl<'a> TipContext<'a> {
    pub fn new(slot: u64, epoch: u64, validator_fee_payer: &'a Pubkey) -> Self {
        Self {
            slot,
            epoch,
            validator_fee_payer,
        }
    }
}
