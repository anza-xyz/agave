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

#[cfg(any(test, feature = "dev-context-only-utils"))]
use crate::{
    invoke_context::BuiltinFunctionRegisterer, program_cache::env::ProgramRuntimeEnvironment,
};
use {
    crate::{
        invoke_context::InvokeContext,
        program_cache::{
            DELAY_VISIBILITY_SLOT_OFFSET,
            entry::{ProgramCacheEntry, ProgramCacheEntryType},
            metrics::ProgramStatistics,
            owner::ProgramCacheEntryOwner,
        },
    },
    solana_clock::Slot,
    solana_sbpf::{elf::Executable, program::BuiltinProgram},
    solana_svm_type_overrides::sync::Arc,
};

/// Types of entries served by the batch-local program cache.
#[derive(Clone)]
pub enum TxBatchCacheEntryType {
    FailedVerification,
    Closed,
    DelayVisibility,
    Unloaded,
    Loaded(Arc<Executable<InvokeContext<'static, 'static>>>),
    Builtin(Arc<BuiltinProgram<InvokeContext<'static, 'static>>>),
}

impl std::fmt::Debug for TxBatchCacheEntryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(match self {
            Self::FailedVerification => "TxBatchCacheEntryType::FailedVerification",
            Self::Closed => "TxBatchCacheEntryType::Closed",
            Self::DelayVisibility => "TxBatchCacheEntryType::DelayVisibility",
            Self::Unloaded => "TxBatchCacheEntryType::Unloaded",
            Self::Loaded(_) => "TxBatchCacheEntryType::Loaded",
            Self::Builtin(_) => "TxBatchCacheEntryType::Builtin",
        })
        .finish()
    }
}

#[cfg(any(test, feature = "dev-context-only-utils"))]
impl TxBatchCacheEntryType {
    pub fn get_environment(&self) -> Option<&ProgramRuntimeEnvironment> {
        match self {
            Self::Loaded(program) => {
                Some(ProgramRuntimeEnvironment::from_ref(program.get_loader()))
            }
            _ => None,
        }
    }
}

impl From<&ProgramCacheEntryType> for TxBatchCacheEntryType {
    fn from(program: &ProgramCacheEntryType) -> Self {
        match program {
            ProgramCacheEntryType::FailedVerification(_) => Self::FailedVerification,
            ProgramCacheEntryType::Closed => Self::Closed,
            ProgramCacheEntryType::DelayVisibility => Self::DelayVisibility,
            ProgramCacheEntryType::Unloaded(_) => Self::Unloaded,
            ProgramCacheEntryType::Loaded(executable) => Self::Loaded(Arc::clone(executable)),
            ProgramCacheEntryType::Builtin(builtin) => Self::Builtin(Arc::clone(builtin)),
        }
    }
}

/// A program entry served by the batch-local program cache.
#[derive(Clone, Debug)]
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

impl From<&ProgramCacheEntry> for TxBatchCacheEntry {
    fn from(entry: &ProgramCacheEntry) -> Self {
        Self {
            program: TxBatchCacheEntryType::from(&entry.program),
            account_owner: entry.account_owner,
            deployment_slot: entry.deployment_slot,
            stats: Arc::clone(&entry.stats),
        }
    }
}

impl TxBatchCacheEntry {
    pub fn new_unloaded(deployment_slot: Slot, account_owner: ProgramCacheEntryOwner) -> Self {
        Self {
            program: TxBatchCacheEntryType::Unloaded,
            account_owner,
            deployment_slot,
            stats: Arc::default(),
        }
    }

    pub fn new_closed_tombstone(closed_slot: Slot, account_owner: ProgramCacheEntryOwner) -> Self {
        Self {
            program: TxBatchCacheEntryType::Closed,
            account_owner,
            deployment_slot: closed_slot,
            stats: Arc::default(),
        }
    }

    pub fn effective_slot(&self) -> Slot {
        match self.program {
            TxBatchCacheEntryType::FailedVerification
            | TxBatchCacheEntryType::Closed
            | TxBatchCacheEntryType::DelayVisibility
            | TxBatchCacheEntryType::Builtin(_) => self.deployment_slot,
            TxBatchCacheEntryType::Unloaded | TxBatchCacheEntryType::Loaded(_) => self
                .deployment_slot
                .saturating_add(DELAY_VISIBILITY_SLOT_OFFSET),
        }
    }

    pub(super) fn is_implicit_delay_visibility_tombstone(&self, slot: Slot) -> bool {
        slot >= self.deployment_slot && slot < self.effective_slot()
    }
}

#[cfg(any(test, feature = "dev-context-only-utils"))]
impl TxBatchCacheEntry {
    pub fn new_builtin(register_fn: BuiltinFunctionRegisterer) -> Self {
        let mut program = BuiltinProgram::new_builtin();
        register_fn(&mut program, "entrypoint").unwrap();
        Self {
            program: TxBatchCacheEntryType::Builtin(Arc::new(program)),
            account_owner: ProgramCacheEntryOwner::NativeLoader,
            deployment_slot: 0, // Deployment slot is always zero
            stats: Arc::default(),
        }
    }

    pub fn is_builtin(&self) -> bool {
        matches!(self.program, TxBatchCacheEntryType::Builtin(_))
            && self.deployment_slot == 0
            && self.account_owner == ProgramCacheEntryOwner::NativeLoader
    }

    pub fn is_failed_verification_tombstone(
        &self,
        deployment_slot: Slot,
        account_owner: ProgramCacheEntryOwner,
    ) -> bool {
        matches!(self.program, TxBatchCacheEntryType::FailedVerification)
            && self.deployment_slot == deployment_slot
            && self.account_owner == account_owner
    }

    pub fn is_tombstone(&self) -> bool {
        matches!(
            self.program,
            TxBatchCacheEntryType::FailedVerification
                | TxBatchCacheEntryType::Closed
                | TxBatchCacheEntryType::DelayVisibility
        )
    }
}

#[cfg(test)]
impl TxBatchCacheEntry {
    /// Whether this entry was created from the given global entry.
    pub(crate) fn is_from(&self, entry: &ProgramCacheEntry) -> bool {
        let same_program = match (&self.program, &entry.program) {
            (TxBatchCacheEntryType::Loaded(a), ProgramCacheEntryType::Loaded(b)) => {
                Arc::ptr_eq(a, b)
            }
            (TxBatchCacheEntryType::Builtin(a), ProgramCacheEntryType::Builtin(b)) => {
                Arc::ptr_eq(a, b)
            }
            (TxBatchCacheEntryType::Unloaded, ProgramCacheEntryType::Unloaded(_))
            | (TxBatchCacheEntryType::Closed, ProgramCacheEntryType::Closed)
            | (TxBatchCacheEntryType::DelayVisibility, ProgramCacheEntryType::DelayVisibility)
            | (
                TxBatchCacheEntryType::FailedVerification,
                ProgramCacheEntryType::FailedVerification(_),
            ) => true,
            _ => false,
        };
        same_program
            && self.deployment_slot == entry.deployment_slot
            && self.account_owner == entry.account_owner
            && Arc::ptr_eq(&self.stats, &entry.stats)
    }
}
