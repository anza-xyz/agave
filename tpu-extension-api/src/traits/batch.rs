/// Copied into `Consumer` at build time; controls whether a failed transaction
/// reverts the entire batch or just itself.
///
/// - [`standard`](BatchCommitMode::standard): each transaction in a batch is
///   committed or rejected individually. A failed transaction does not affect
///   others. This is vanilla Agave behavior.
/// - [`all_or_nothing`](BatchCommitMode::all_or_nothing): if any transaction in
///   a batch fails, the whole batch is reverted. Used by bundle executors (e.g.
///   Jito) that need atomicity across a group of transactions.
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

/// Converts a fork-level policy object into the [`BatchCommitMode`] value
/// copied into `Consumer` at `BankingStage` build time.
///
/// This is sampled once at build time, not per-batch. The resulting
/// [`BatchCommitMode`] is a `Copy` value stored inline in `Consumer` so there
/// is no per-batch allocation or vtable call.
pub trait BatchCommitPolicy: Send + Sync + 'static {
    fn mode(&self) -> BatchCommitMode;
}
