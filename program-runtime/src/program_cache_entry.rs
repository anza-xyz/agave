#[cfg(feature = "metrics")]
use crate::program_metrics::LoadProgramMetrics;
use {
    crate::{
        invoke_context::{BuiltinFunctionRegisterer, InvokeContext},
        loaded_programs::ProgramRuntimeEnvironment,
        program_metrics::ProgramStatistics,
    },
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    solana_sbpf::{elf::Executable, program::BuiltinProgram, verifier::RequisiteVerifier},
    solana_sdk_ids::{
        bpf_loader, bpf_loader_deprecated, bpf_loader_upgradeable, loader_v4, native_loader,
    },
    solana_svm_measure::measure::Measure,
    solana_svm_type_overrides::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

pub const DELAY_VISIBILITY_SLOT_OFFSET: Slot = 1;

/// The owner of a programs accounts, thus the loader of a program
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProgramCacheEntryOwner {
    #[default]
    NativeLoader,
    LoaderV1,
    LoaderV2,
    LoaderV3,
    LoaderV4,
}

impl TryFrom<&Pubkey> for ProgramCacheEntryOwner {
    type Error = ();
    fn try_from(loader_key: &Pubkey) -> Result<Self, ()> {
        if native_loader::check_id(loader_key) {
            Ok(ProgramCacheEntryOwner::NativeLoader)
        } else if bpf_loader_deprecated::check_id(loader_key) {
            Ok(ProgramCacheEntryOwner::LoaderV1)
        } else if bpf_loader::check_id(loader_key) {
            Ok(ProgramCacheEntryOwner::LoaderV2)
        } else if bpf_loader_upgradeable::check_id(loader_key) {
            Ok(ProgramCacheEntryOwner::LoaderV3)
        } else if loader_v4::check_id(loader_key) {
            Ok(ProgramCacheEntryOwner::LoaderV4)
        } else {
            Err(())
        }
    }
}

impl From<ProgramCacheEntryOwner> for Pubkey {
    fn from(program_cache_entry_owner: ProgramCacheEntryOwner) -> Self {
        match program_cache_entry_owner {
            ProgramCacheEntryOwner::NativeLoader => native_loader::id(),
            ProgramCacheEntryOwner::LoaderV1 => bpf_loader_deprecated::id(),
            ProgramCacheEntryOwner::LoaderV2 => bpf_loader::id(),
            ProgramCacheEntryOwner::LoaderV3 => bpf_loader_upgradeable::id(),
            ProgramCacheEntryOwner::LoaderV4 => loader_v4::id(),
        }
    }
}

/*
    The possible ProgramCacheEntryType transitions:

    DelayVisibility is special in that it is never stored in the cache.
    It is only returned by ProgramCacheForTxBatch::find() when a Loaded entry
    is encountered which is not effective yet.

    Builtin re/deployment:
    - Empty => Builtin in TransactionBatchProcessor::add_builtin
    - Builtin => Builtin in TransactionBatchProcessor::add_builtin

    Un/re/deployment (with delay and cooldown):
    - Empty / Closed => Loaded in UpgradeableLoaderInstruction::DeployWithMaxDataLen
    - Loaded / FailedVerification => Loaded in UpgradeableLoaderInstruction::Upgrade
    - Loaded / FailedVerification => Closed in UpgradeableLoaderInstruction::Close

    Loader migration:
    - Closed => Closed (in the same slot)
    - FailedVerification => FailedVerification (with different account_owner)
    - Loaded => Loaded (with different account_owner)

    Eviction and unloading (in the same slot):
    - Unloaded => Loaded in ProgramCache::assign_program
    - Loaded => Unloaded in ProgramCache::unload_program_entry

    At epoch boundary (when feature set and environment changes):
    - Loaded => FailedVerification in Bank::_new_from_parent
    - FailedVerification => Loaded in Bank::_new_from_parent

    Through pruning (when on orphan fork or overshadowed on the rooted fork):
    - Closed / Unloaded / Loaded / Builtin => Empty in ProgramCache::prune
*/

/// Actual payload of [ProgramCacheEntry].
#[derive(Default)]
pub enum ProgramCacheEntryType {
    /// Tombstone for programs which currently do not pass the verifier but could if the feature set changed.
    FailedVerification(ProgramRuntimeEnvironment),
    /// Tombstone for programs that were either explicitly closed or never deployed.
    ///
    /// It's also used for accounts belonging to program loaders, that don't actually contain program code (e.g. buffer accounts for LoaderV3 programs).
    #[default]
    Closed,
    /// Tombstone for programs which have recently been modified but the new version is not visible yet.
    DelayVisibility,
    /// Successfully verified but not currently compiled.
    ///
    /// It continues to track usage statistics even when the compiled executable of the program is evicted from memory.
    Unloaded(ProgramRuntimeEnvironment),
    /// Verified program.
    ///
    /// It may or may not be JIT compiled.
    Loaded(Executable<InvokeContext<'static, 'static>>),
    /// A built-in program which is not stored on-chain but backed into and distributed with the validator
    Builtin(BuiltinProgram<InvokeContext<'static, 'static>>),
}

impl std::fmt::Debug for ProgramCacheEntryType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct(match self {
            ProgramCacheEntryType::FailedVerification(_) => {
                "ProgramCacheEntryType::FailedVerification"
            }
            ProgramCacheEntryType::Closed => "ProgramCacheEntryType::Closed",
            ProgramCacheEntryType::DelayVisibility => "ProgramCacheEntryType::DelayVisibility",
            ProgramCacheEntryType::Unloaded(_) => "ProgramCacheEntryType::Unloaded",
            ProgramCacheEntryType::Loaded(_) => "ProgramCacheEntryType::Loaded",
            ProgramCacheEntryType::Builtin(_) => "ProgramCacheEntryType::Builtin",
        })
        .finish()
    }
}

impl ProgramCacheEntryType {
    /// Returns a reference to its environment if it has one
    pub fn get_environment(&self) -> Option<&ProgramRuntimeEnvironment> {
        match self {
            ProgramCacheEntryType::Loaded(program) => {
                Some(ProgramRuntimeEnvironment::from_ref(program.get_loader()))
            }
            ProgramCacheEntryType::FailedVerification(env)
            | ProgramCacheEntryType::Unloaded(env) => Some(env),
            _ => None,
        }
    }
}

/// Holds a program version at a specific address and on a specific slot / fork.
///
/// It contains the actual program in [ProgramCacheEntryType] and a bunch of meta-data.
#[derive(Debug, Default)]
pub struct ProgramCacheEntry {
    /// The program of this entry
    pub program: ProgramCacheEntryType,
    /// The loader of this entry
    pub account_owner: ProgramCacheEntryOwner,
    /// Size of account that stores the program and program data
    pub account_size: usize,
    /// Slot in which the program was (re)deployed
    pub deployment_slot: Slot,
    /// Slot in which this entry will become active (can be in the future)
    pub effective_slot: Slot,
    /// How often this entry was used by a transaction
    pub stats: Arc<ProgramStatistics>,
    pub latest_access_slot: AtomicU64,
}

impl PartialEq for ProgramCacheEntry {
    fn eq(&self, other: &Self) -> bool {
        self.effective_slot == other.effective_slot
            && self.deployment_slot == other.deployment_slot
            && self.is_tombstone() == other.is_tombstone()
    }
}

impl ProgramCacheEntry {
    /// Creates a new user program
    pub fn new(
        loader_key: &Pubkey,
        program_runtime_environment: ProgramRuntimeEnvironment,
        deployment_slot: Slot,
        effective_slot: Slot,
        elf_bytes: &[u8],
        account_size: usize,
        #[cfg(feature = "metrics")] metrics: &mut LoadProgramMetrics,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_internal(
            loader_key,
            program_runtime_environment,
            deployment_slot,
            effective_slot,
            elf_bytes,
            account_size,
            #[cfg(feature = "metrics")]
            metrics,
            false, /* reloading */
        )
    }

    /// Reloads a user program, *without* running the verifier.
    ///
    /// # Safety
    ///
    /// This method is unsafe since it assumes that the program has already been verified. Should
    /// only be called when the program was previously verified and loaded in the cache, but was
    /// unloaded due to inactivity. It should also be checked that the `program_runtime_environment`
    /// hasn't changed since it was unloaded.
    pub unsafe fn reload(
        loader_key: &Pubkey,
        program_runtime_environment: ProgramRuntimeEnvironment,
        deployment_slot: Slot,
        effective_slot: Slot,
        elf_bytes: &[u8],
        account_size: usize,
        #[cfg(feature = "metrics")] metrics: &mut LoadProgramMetrics,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::new_internal(
            loader_key,
            program_runtime_environment,
            deployment_slot,
            effective_slot,
            elf_bytes,
            account_size,
            #[cfg(feature = "metrics")]
            metrics,
            true, /* reloading */
        )
    }

    fn new_internal(
        loader_key: &Pubkey,
        program_runtime_environment: ProgramRuntimeEnvironment,
        deployment_slot: Slot,
        effective_slot: Slot,
        elf_bytes: &[u8],
        account_size: usize,
        #[cfg(feature = "metrics")] metrics: &mut LoadProgramMetrics,
        reloading: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let entry_stats = ProgramStatistics::default();
        #[cfg(feature = "metrics")]
        let load_elf_time = Measure::start("load_elf_time");
        let executable = Executable::load(elf_bytes, Arc::clone(&*program_runtime_environment))?;

        #[cfg(feature = "metrics")]
        {
            metrics.load_elf_us = load_elf_time.end_as_us();
        }

        if !reloading {
            #[cfg(feature = "metrics")]
            let verify_code_time = Measure::start("verify_code_time");
            executable.verify::<RequisiteVerifier>()?;
            #[cfg(feature = "metrics")]
            {
                metrics.verify_code_us = verify_code_time.end_as_us();
            }
        }

        #[cfg(all(not(target_os = "windows"), target_arch = "x86_64"))]
        {
            let jit_compile_time = Measure::start("jit_compile_time");
            executable.jit_compile()?;
            let jit_compile_time = jit_compile_time.end_as_us();
            entry_stats.jit_compiled(jit_compile_time);
            #[cfg(feature = "metrics")]
            {
                metrics.jit_compile_us = jit_compile_time;
            }
        }

        Ok(Self {
            deployment_slot,
            account_owner: ProgramCacheEntryOwner::try_from(loader_key).unwrap(),
            account_size,
            effective_slot,
            program: ProgramCacheEntryType::Loaded(executable),
            stats: entry_stats.into(),
            latest_access_slot: AtomicU64::new(0),
        })
    }

    pub fn to_unloaded(&self) -> Option<Self> {
        match &self.program {
            ProgramCacheEntryType::Loaded(_) => {}
            ProgramCacheEntryType::FailedVerification(_)
            | ProgramCacheEntryType::Closed
            | ProgramCacheEntryType::DelayVisibility
            | ProgramCacheEntryType::Unloaded(_)
            | ProgramCacheEntryType::Builtin(_) => {
                return None;
            }
        }
        Some(Self {
            program: ProgramCacheEntryType::Unloaded(self.program.get_environment()?.clone()),
            account_owner: self.account_owner,
            account_size: self.account_size,
            deployment_slot: self.deployment_slot,
            effective_slot: self.effective_slot,
            stats: Arc::clone(&self.stats),
            latest_access_slot: AtomicU64::new(self.latest_access_slot.load(Ordering::Relaxed)),
        })
    }

    /// Creates a new built-in program
    pub fn new_builtin(
        deployment_slot: Slot,
        account_size: usize,
        register_fn: BuiltinFunctionRegisterer,
    ) -> Self {
        let mut program = BuiltinProgram::new_builtin();
        register_fn(&mut program, "entrypoint").unwrap();
        Self {
            deployment_slot,
            account_owner: ProgramCacheEntryOwner::NativeLoader,
            account_size,
            effective_slot: deployment_slot,
            program: ProgramCacheEntryType::Builtin(program),
            stats: Arc::default(),
            latest_access_slot: AtomicU64::new(0),
        }
    }

    pub fn new_tombstone(
        slot: Slot,
        account_owner: ProgramCacheEntryOwner,
        reason: ProgramCacheEntryType,
    ) -> Self {
        Self::new_tombstone_with_stats(slot, account_owner, reason, Arc::default())
    }

    pub fn new_tombstone_with_stats(
        slot: Slot,
        account_owner: ProgramCacheEntryOwner,
        reason: ProgramCacheEntryType,
        stats: Arc<ProgramStatistics>,
    ) -> Self {
        let tombstone = Self {
            program: reason,
            account_owner,
            account_size: 0,
            deployment_slot: slot,
            effective_slot: slot,
            stats,
            latest_access_slot: AtomicU64::new(0),
        };
        debug_assert!(tombstone.is_tombstone());
        tombstone
    }

    pub fn is_tombstone(&self) -> bool {
        matches!(
            self.program,
            ProgramCacheEntryType::FailedVerification(_)
                | ProgramCacheEntryType::Closed
                | ProgramCacheEntryType::DelayVisibility
        )
    }

    pub(crate) fn is_implicit_delay_visibility_tombstone(&self, slot: Slot) -> bool {
        !matches!(self.program, ProgramCacheEntryType::Builtin(_))
            && self.effective_slot.saturating_sub(self.deployment_slot)
                == DELAY_VISIBILITY_SLOT_OFFSET
            && slot >= self.deployment_slot
            && slot < self.effective_slot
    }

    pub fn update_access_slot(&self, slot: Slot) {
        let _ = self.latest_access_slot.fetch_max(slot, Ordering::Relaxed);
    }

    /// Compute a retention score.
    ///
    /// Eviction uses an adapted GDSF scheme which incorporates frequency, recovery cost
    /// (recompilation) and time-based decay.
    ///
    /// How hard should we try to retain this entry. Higher number -> retention more likely.
    pub fn retention_score(&self) -> u64 {
        let last_access = self.latest_access_slot.load(Ordering::Relaxed);
        let recovery_cost = self.stats.compilation_time_ema.load(Ordering::Relaxed);
        let frequency = self.stats.uses.load(Ordering::Relaxed);
        retention_score(last_access, recovery_cost, frequency)
    }

    pub fn account_owner(&self) -> Pubkey {
        self.account_owner.into()
    }
}

/// See [`ProgramCacheEntry::retention_score`].
pub(crate) const fn retention_score(last_access: u64, recovery_cost: u64, frequency: u64) -> u64 {
    // Traditionally GDSF uses the following logic:
    //
    // on_access:
    //   entry.frequency += 1
    //   entry.H := cache.L + (entry.cost * entry.frequency) / entry.size
    //
    // on_eviction:
    //   victim = pick_victim_minimizing_H()
    //   cache.L := victim.H
    //
    // It achieves decay by virtue of L increasing over time (and therefore the “value” of
    // stored score of each entry decreasing over time.) Entry recovery and frequency, as well
    // as size are otherwise also accounted for by them inflating the overall score by a bit.
    //
    // We adapt this algorithm slightly: we already have a kind of `L` – access slot. It does
    // not include the weight of the evicted entry as the original algorithm does, that is
    // *probably* fine (the author has not done any empirical experiments to verify it it
    // actually matters.)
    //
    // Additionally we ignore the size component altogether as irrelevant and instead of
    // applying entry weight linearly, we use a `log_2`. We can't use plain `weight*frequency`
    // as the most heavily used entries would never ever get evicted after just some runtime,
    // even if they're no longer used. With `log_2` weight and frequency can contribute to
    // up-to 128 slots of "bonus" towards their retention compared to rarely used peers.
    //
    // Feel free to adjust the specific formulae used.
    let weight = (recovery_cost as u128).wrapping_mul(frequency as u128);
    let weight_log = u128::BITS.wrapping_sub(weight.leading_zeros());
    last_access.saturating_add(weight_log as u64)
}

#[cfg(test)]
mod tests {
    use {
        crate::{
            loaded_programs::tests::new_test_entry_with_usage, program_metrics::ProgramStatistics,
        },
        std::sync::atomic::{AtomicU64, Ordering},
    };

    #[test]
    fn test_retention_score_decay_horizon() {
        let stats = ProgramStatistics {
            uses: AtomicU64::new(u64::MAX),
            compilation_time_ema: AtomicU64::new(u64::MAX),
            ..Default::default()
        };
        let program = new_test_entry_with_usage(0, 0, stats);
        program.update_access_slot(1);
        assert!(
            dbg!(program.retention_score()) <= 129,
            "retention score should remain within sensible boundaries even for very frequently \
             used entries."
        );
    }

    #[test]
    fn test_retention_score_frequency_preference() {
        let stats = ProgramStatistics {
            uses: AtomicU64::new(16),
            compilation_time_ema: AtomicU64::new(1),
            ..Default::default()
        };
        let program = new_test_entry_with_usage(10, 11, stats);
        program.update_access_slot(15);
        let less_used_retention_score = program.retention_score();
        program.stats.uses.fetch_max(1024, Ordering::Relaxed);
        let more_used_retention_score = program.retention_score();
        assert!(
            less_used_retention_score > 15,
            "frequency should count for entry retention score"
        );
        assert!(
            dbg!(more_used_retention_score) > dbg!(less_used_retention_score),
            "retention score should prefer evicting less used entry over the more used one if \
             possible"
        );
    }

    #[test]
    fn test_retention_score_recovery_time_preference() {
        let stats = ProgramStatistics {
            uses: AtomicU64::new(1),
            compilation_time_ema: AtomicU64::new(1000),
            ..Default::default()
        };
        let program = new_test_entry_with_usage(10, 11, stats);
        program.update_access_slot(15);
        let cheaper_to_compile_score = program.retention_score();
        program
            .stats
            .compilation_time_ema
            .fetch_max(2000, Ordering::Relaxed);
        let more_expensive_to_compile_score = program.retention_score();
        assert!(
            cheaper_to_compile_score > 15,
            "compile time should count for entry retention score"
        );
        assert!(
            dbg!(more_expensive_to_compile_score) > dbg!(cheaper_to_compile_score),
            "retention score should prefer evicting cheaper-to-compile entries"
        );
    }

    #[test]
    fn test_retention_weight_metric_does_not_outweight_smaller_metric() {
        // Compilation time generally stays in the scale of 4 digits, while the uses counter can
        // become many millions. Neither should overshadow other too much.
        let stats = ProgramStatistics {
            uses: AtomicU64::new(100_000_000),
            compilation_time_ema: AtomicU64::new(1000),
            ..Default::default()
        };
        let program = new_test_entry_with_usage(10, 11, stats);
        program.update_access_slot(15);
        let previous_score = program.retention_score();
        program
            .stats
            .compilation_time_ema
            .fetch_max(2000, Ordering::Relaxed);
        let new_score = program.retention_score();
        assert!(
            dbg!(previous_score) != dbg!(new_score),
            "retention weight components shouldn't overshadow the other due to scale differences"
        );
    }
}
