#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BatchCommitMode {
    revert_on_error: bool,
}

impl BatchCommitMode {
    pub const fn standard() -> Self {
        Self {
            revert_on_error: false,
        }
    }

    pub const fn all_or_nothing() -> Self {
        Self {
            revert_on_error: true,
        }
    }

    pub const fn reverts_on_error(self) -> bool {
        self.revert_on_error
    }
}

pub trait BatchCommitPolicy: Send + Sync + 'static {
    /// Convert a fork-level policy object into the value copied into BankingStage.
    fn mode(&self) -> BatchCommitMode;
}
