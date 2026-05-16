use crate::BatchCommitMode;

/// Cold-path configuration for [`super::BankingHooks`].
///
/// These values are validator configuration, not services. Behavioral hooks such
/// as tip processing stay on [`super::BankingHooks`] directly.
#[derive(Clone)]
pub struct BankingConfig {
    pub commit_mode: BatchCommitMode,
}

impl Default for BankingConfig {
    fn default() -> Self {
        Self {
            commit_mode: BatchCommitMode::standard(),
        }
    }
}
