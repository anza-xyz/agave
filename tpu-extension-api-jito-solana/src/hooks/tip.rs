use {
    agave_tpu_extension_api::{TipContext, TipProcessor, TipProcessorError},
    solana_pubkey::Pubkey,
    std::{collections::HashSet, sync::Mutex},
};

pub struct TipManager {
    tip_accounts: HashSet<Pubkey>,
    // Real Jito-Solana submits a transaction to initialize the TipDistributionAccount PDA
    // once per epoch. This Mutex is the in-process guard for the mock.
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
    fn process(&self, ctx: &TipContext<'_>) -> Result<(), TipProcessorError> {
        if self.tip_accounts.is_empty() {
            return Err(TipProcessorError::InitializationFailed {
                slot: ctx.slot,
                reason: "no tip accounts configured".to_string(),
            });
        }
        if !self.initialized_epochs.lock().unwrap().insert(ctx.epoch) {
            return Err(TipProcessorError::AlreadyInitialized);
        }
        Ok(())
    }
}
