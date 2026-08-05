//! Callbacks required for program runtime operations.

use {
    crate::{program_cache_entry::ProgramCacheEntry, program_metrics::ProgramStatistics},
    solana_pubkey::Pubkey,
    solana_svm_type_overrides::sync::Arc,
};

/// Callback used by [`InvokeContext`] to load programs from the global
/// [`ProgramCache`] on the fly, as they are invoked.
///
/// [`InvokeContext`]: crate::invoke_context::InvokeContext
/// [`ProgramCache`]: crate::loaded_programs::ProgramCache
pub trait ProgramCacheCallback {
    /// Load the program at `program_id`, extracting it from the global
    /// [`ProgramCache`](crate::loaded_programs::ProgramCache) and compiling it
    /// if necessary.
    fn load_program(&self, _program_id: &Pubkey) -> Option<Arc<ProgramCacheEntry>> {
        None
    }

    /// Record that `program_id` was invoked, for usage accounting.
    ///
    /// Called for programs already resident in the batch-local cache, which
    /// [`ProgramCacheCallback::load_program`] never sees.
    fn record_program_use(&self, _program_id: &Pubkey, _entry: &Arc<ProgramCacheEntry>) {}

    /// Obtain usage statistics recorded for `program_id`, if found.
    ///
    /// Called during deployment to skip attempting to extract the program from
    /// the global [`ProgramCache`](crate::loaded_programs::ProgramCache).
    fn get_program_stats(
        &self,
        _program_id: &Pubkey,
        _loader_key: &Pubkey,
    ) -> Option<Arc<ProgramStatistics>> {
        None
    }
}

/// A [`ProgramCacheCallback`] that never loads anything.
///
/// Use with caution: this callback will never pull a program from the global
/// [`ProgramCache`](crate::loaded_programs::ProgramCache), so any program not
/// already present in the batch-local cache will fail to resolve at invocation
/// time. Only appropriate where that cache is fully populated up front.
#[cfg(feature = "dev-context-only-utils")]
pub struct NoOpProgramCacheCallback;

#[cfg(feature = "dev-context-only-utils")]
impl ProgramCacheCallback for NoOpProgramCacheCallback {}
