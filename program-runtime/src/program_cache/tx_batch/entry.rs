//! Batch-local program cache entry structure.
//!
//! The batch-local program cache keeps a different entry index than the
//! global program cache by design. This helps to differentiate which cache
//! handles which type of entry.
//!
//! The batch-local cache keeps more entry types than the global cache since it
//! may only need to track such states during the lifetime of the batch itself.
//! This lets the global cache simplify its index structure.
//!
//! Furthermore, the batch cache has no need to track program runtime
//! environments, since the environment is fixed for the lifetime of the batch.

use {
    crate::{
        invoke_context::InvokeContext,
        program_cache::{metrics::ProgramStatistics, owner::ProgramCacheEntryOwner},
    },
    solana_clock::Slot,
    solana_sbpf::{elf::Executable, program::BuiltinProgram},
    solana_svm_type_overrides::sync::Arc,
};

/// Types of entries served by the batch-local program cache.
pub enum TxBatchCacheEntryType {
    FailedVerification,
    Closed,
    DelayVisibility,
    Unloaded,
    Loaded(Arc<Executable<InvokeContext<'static, 'static>>>),
    Builtin(Arc<BuiltinProgram<InvokeContext<'static, 'static>>>),
}

/// A program entry served by the batch-local program cache.
pub struct TxBatchCacheEntry {
    /// The program of this entry.
    pub program: TxBatchCacheEntryType,
    /// The loader of this entry.
    pub account_owner: ProgramCacheEntryOwner,
    /// Slot in which the program was (re)deployed.
    pub deployment_slot: Slot,
    /// Usage statistics, shared with the global entry this was created from, if
    /// any.
    pub stats: Arc<ProgramStatistics>,
}
