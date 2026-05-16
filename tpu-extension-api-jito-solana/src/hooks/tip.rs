use {
    agave_tpu_extension_api::{TipContext, TipProcessor},
    solana_pubkey::Pubkey,
    std::{
        collections::HashSet,
        sync::Mutex,
    },
};

/// Initializes tip-distribution PDAs at each leader-slot transition.
///
/// In production Jito-Solana this submits an on-chain transaction to create
/// the `TipDistributionAccount` PDA once per epoch. The reference mock records
/// the epoch in-process to stay idempotent without hitting the chain.
pub struct TipManager {
    tip_accounts: HashSet<Pubkey>,
    initialized_epochs: Mutex<HashSet<u64>>,
}

impl TipManager {
    pub fn new(tip_accounts: impl IntoIterator<Item = Pubkey>) -> Self {
        Self {
            tip_accounts: tip_accounts.into_iter().collect(),
            initialized_epochs: Mutex::new(HashSet::new()),
        }
    }
}

impl TipProcessor for TipManager {
    fn process(&self, ctx: &TipContext) {
        if self.tip_accounts.is_empty() {
            // Fatal: validator cannot produce MEV-compatible blocks without tip accounts.
            // Panic propagates through the scheduler thread and triggers validator shutdown.
            panic!(
                "no tip accounts configured; cannot initialize tip PDAs for slot {}",
                ctx.slot
            );
        }
        // Already initialized for this epoch — idempotent, nothing to do.
        self.initialized_epochs.lock().unwrap().insert(ctx.epoch);
    }
}
