//! The Epoch Boundary Preparation Phase (EBPP) is an optimization offered by
//! the program cache in order to preemptively reload programs for the
//! anticipated upcoming environment based on feature activations.
//!
//! Without such a phase, pending features would activate at the epoch boundary
//! and change the [`ProgramRuntimeEnvironment`], leaving no cached program
//! usable under the new environment. Each program would then have to be
//! reloaded, verified, and compiled on first use, slowing transaction execution
//! at the start of the epoch.
//!
//! Instead, if the anticipated environment differs from the current one, one
//! program per slot is recompiled against it during the final slots of the
//! epoch, starting with the highest retention score. This is best-effort: any
//! program not recompiled in time is loaded on demand after the boundary.

use {
    crate::program_cache::{entry::ProgramCacheEntry, env::ProgramRuntimeEnvironment},
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_svm_type_overrides::sync::Arc,
};

/// Globally manages the transition between environments at the epoch boundary
#[derive(Debug, Default)]
pub struct EpochBoundaryPreparation {
    /// The epoch of the upcoming_environment
    pub upcoming_epoch: Epoch,
    /// Anticipated replacement for `environments` at the next epoch
    ///
    /// This is only `Some` around the boundaries when a changed environment is
    /// actually coming. It starts with the cache preparation phase a few
    /// hundred slots before the epoch boundary, and it ends with the first
    /// rerooting after the epoch boundary.
    pub upcoming_environment: Option<ProgramRuntimeEnvironment>,
    /// List of loaded programs which should be recompiled before the next epoch (but don't have to).
    pub programs_to_recompile: Vec<(Pubkey, Arc<ProgramCacheEntry>)>,
}

impl EpochBoundaryPreparation {
    pub fn new(epoch: Epoch) -> Self {
        Self {
            upcoming_epoch: epoch,
            upcoming_environment: None,
            programs_to_recompile: Vec::default(),
        }
    }

    /// Returns the upcoming environments depending on the given epoch
    pub fn get_upcoming_environment_for_epoch(
        &self,
        epoch: Epoch,
    ) -> Option<ProgramRuntimeEnvironment> {
        if epoch == self.upcoming_epoch {
            return self.upcoming_environment.clone();
        }
        None
    }

    /// Before rerooting the blockstore this concludes the epoch boundary
    /// preparation. Returns whether there was one to conclude.
    pub fn reroot(&mut self, epoch: Epoch) -> bool {
        if epoch == self.upcoming_epoch && self.upcoming_environment.take().is_some() {
            self.programs_to_recompile.clear();
            return true;
        }

        false
    }
}
