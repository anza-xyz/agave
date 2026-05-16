use {crate::BatchCommitMode, solana_pubkey::Pubkey};

/// Validator-level tip configuration forwarded to [`crate::TipContext`] at each leader-slot transition.
#[derive(Clone, Copy, Default)]
pub struct TipConfig {
    pub validator_fee_payer: Pubkey,
}

/// Cold-path configuration for [`super::BankingHooks`].
///
/// These values are validator configuration, not services. Behavioral hooks such
/// as tip processing stay on [`super::BankingHooks`] directly.
#[derive(Clone)]
pub struct BankingConfig {
    pub tip_config: TipConfig,
    pub commit_mode: BatchCommitMode,
}

impl Default for BankingConfig {
    fn default() -> Self {
        Self {
            tip_config: TipConfig::default(),
            commit_mode: BatchCommitMode::standard(),
        }
    }
}
