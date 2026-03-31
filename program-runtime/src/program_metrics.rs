#[cfg(feature = "metrics")]
use solana_svm_timings::ExecuteDetailsTimings;
use {
    crate::loaded_programs::ForkGraph,
    log::{debug, log_enabled, trace},
    solana_pubkey::Pubkey,
    std::{
        collections::HashMap,
        sync::atomic::{AtomicU64, Ordering},
    },
};

#[derive(Debug, Default)]
pub struct ProgramStatistics {
    pub uses: AtomicU64,

    pub compilations: AtomicU64,
    pub total_compilation_time_us: AtomicU64,
    /// Exponential moving average of the compilation time.
    pub compilation_time_ema: AtomicU64,

    pub jit_invocations: AtomicU64,
    pub total_jit_execution_time_us: AtomicU64,
    /// Exponential moving average of the JIT execution time.
    pub jit_execution_time_ema: AtomicU64,

    pub interpreted_invocations: AtomicU64,
    pub total_interpretation_time_us: AtomicU64,
    /// Exponential moving average of the interpreted execution time.
    pub interpretation_time_ema: AtomicU64,
}

/// Number of compilation observations contributing to the the [`Self::compilation_time_ema`].
const COMPILATION_EMA_WINDOW_SIZE: u64 = 10;
/// Number of execution observations contributing to the execution EMA stats.
const EXECUTION_EMA_WINDOW_SIZE: u64 = 500;
/// Track exponential moving average in scaled-up units.
///
/// Doing so allows to mitigate error from rounding-towards-zero we get when using integer math.
pub(crate) const EMA_SCALE: u64 = 1_000;

impl ProgramStatistics {
    fn observe_ema<const WINDOW_SIZE: u64>(counter: &AtomicU64, duration_us: u64) {
        let duration_ema = duration_us.saturating_mul(EMA_SCALE);
        counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |ema| {
                // Exponential moving average iteratively is computed as $ema' = alpha *
                // observation + (1 - alpha) * ema$. This works great for floating point, but we
                // want integers. For purposes of convenience we also want to really think in terms
                // of simple moving average window sizes as that is easier to reason about.
                //
                // Exponential moving average and simple moving average of window N has a rough
                // equivalence of `alpha ≈ 2 / (N + 1)`. Slotting this into our original iterative
                // formula:
                //
                // $$ ema' = 2 / (N+1) * observation + (1 - 2/(N+1)) * ema $$
                //
                // we get
                //
                // $$ ema' = (2*observation)/(N+1) + (N+1-2)*ema/(N+1) $$
                let (numer, denom) = const { (2, 1 + WINDOW_SIZE) };
                Some(if ema == 0 {
                    duration_ema
                } else {
                    let weighted_observation = duration_ema.saturating_mul(numer);
                    let previous_observations = ema.saturating_mul(denom.saturating_sub(numer));
                    weighted_observation
                        .saturating_add(previous_observations)
                        .checked_div(denom)
                        .expect("unreachable: denom is >= 1")
                })
            })
            .expect("unreachable: closure always returns a Some");
    }

    /// Record information about JIT compilation.
    pub fn jit_compiled(&self, duration_us: u64) {
        let ord = Ordering::Relaxed;
        self.compilations.fetch_add(1, ord);
        self.total_compilation_time_us.fetch_add(duration_us, ord);
        Self::observe_ema::<COMPILATION_EMA_WINDOW_SIZE>(&self.compilation_time_ema, duration_us);
    }

    /// Record information about JIT-compiled program having been executed.
    pub fn jit_executed(&self, duration_us: u64) {
        let ord = Ordering::Relaxed;
        self.jit_invocations.fetch_add(1, ord);
        self.total_jit_execution_time_us.fetch_add(duration_us, ord);
        Self::observe_ema::<EXECUTION_EMA_WINDOW_SIZE>(&self.jit_execution_time_ema, duration_us);
    }

    /// Record information about program executed with the interpreter.
    pub fn interpreter_executed(&self, duration_us: u64) {
        let ord = Ordering::Relaxed;
        self.interpreted_invocations.fetch_add(1, ord);
        self.total_interpretation_time_us
            .fetch_add(duration_us, ord);
        Self::observe_ema::<EXECUTION_EMA_WINDOW_SIZE>(&self.interpretation_time_ema, duration_us);
    }

    pub fn merge_from(&self, other: &ProgramStatistics) {
        let ord = Ordering::Relaxed;
        self.uses.fetch_add(other.uses.load(ord), ord);
        let other_compilations = other.compilations.load(ord);
        let this_compilations = self.compilations.fetch_add(other_compilations, ord);
        self.total_compilation_time_us
            .fetch_add(other.total_compilation_time_us.load(ord), ord);
        let other_jit_invocations = other.jit_invocations.load(ord);
        let this_jit_invocations = self.jit_invocations.fetch_add(other_jit_invocations, ord);
        self.total_jit_execution_time_us
            .fetch_add(other.total_jit_execution_time_us.load(ord), ord);
        let other_interpretations = other.interpreted_invocations.load(ord);
        let this_interpretations = self
            .interpreted_invocations
            .fetch_add(other.interpreted_invocations.load(ord), ord);
        self.total_interpretation_time_us
            .fetch_add(other.total_interpretation_time_us.load(ord), ord);
        if let Some(comp_ema) = ProgramCacheStats::combined_ema::<
            COMPILATION_EMA_WINDOW_SIZE,
            COMPILATION_EMA_WINDOW_SIZE,
        >(
            &self.compilation_time_ema,
            &other.compilation_time_ema,
            this_compilations,
            other_compilations,
        ) {
            self.compilation_time_ema.store(comp_ema, ord);
        }
        if let Some(exec_ema) =
            ProgramCacheStats::combined_ema::<EXECUTION_EMA_WINDOW_SIZE, EXECUTION_EMA_WINDOW_SIZE>(
                &self.jit_execution_time_ema,
                &other.jit_execution_time_ema,
                this_jit_invocations,
                other_jit_invocations,
            )
        {
            self.jit_execution_time_ema.store(exec_ema, ord);
        }
        if let Some(interp_ema) =
            ProgramCacheStats::combined_ema::<EXECUTION_EMA_WINDOW_SIZE, EXECUTION_EMA_WINDOW_SIZE>(
                &self.interpretation_time_ema,
                &other.interpretation_time_ema,
                this_interpretations,
                other_interpretations,
            )
        {
            self.interpretation_time_ema.store(interp_ema, ord);
        }
    }
}

/// Global cache statistics for [ProgramCache].
#[derive(Debug, Default)]
pub struct ProgramCacheStats {
    /// a program was already in the cache
    pub hits: AtomicU64,
    /// a program was not found and loaded instead
    pub misses: AtomicU64,
    /// a compiled executable was unloaded
    pub evictions: HashMap<Pubkey, u64>,
    /// an unloaded program was loaded again (opposite of eviction)
    pub reloads: AtomicU64,
    /// a program was loaded or un/re/deployed
    pub insertions: AtomicU64,
    /// a program was loaded but can not be extracted on its own fork anymore
    pub lost_insertions: AtomicU64,
    /// a program which was already in the cache was reloaded by mistake
    pub replacements: AtomicU64,
    /// a program was only used once before being unloaded
    pub one_hit_wonders: AtomicU64,
    /// a program became unreachable in the fork graph because of rerooting
    pub prunes_orphan: AtomicU64,
    /// a program got pruned because it was not recompiled for the next epoch
    pub prunes_environment: AtomicU64,
    /// a program had no entries because all slot versions got pruned
    pub empty_entries: AtomicU64,
    /// water level of loaded entries currently cached
    pub water_level: AtomicU64,
}

impl ProgramCacheStats {
    pub fn reset(&mut self) {
        *self = ProgramCacheStats::default();
    }
    pub fn log(&self) {
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let evictions: u64 = self.evictions.values().sum();
        let reloads = self.reloads.load(Ordering::Relaxed);
        let insertions = self.insertions.load(Ordering::Relaxed);
        let lost_insertions = self.lost_insertions.load(Ordering::Relaxed);
        let replacements = self.replacements.load(Ordering::Relaxed);
        let one_hit_wonders = self.one_hit_wonders.load(Ordering::Relaxed);
        let prunes_orphan = self.prunes_orphan.load(Ordering::Relaxed);
        let prunes_environment = self.prunes_environment.load(Ordering::Relaxed);
        let empty_entries = self.empty_entries.load(Ordering::Relaxed);
        let water_level = self.water_level.load(Ordering::Relaxed);
        debug!(
            "Loaded Programs Cache Stats -- Hits: {hits}, Misses: {misses}, Evictions: \
             {evictions}, Reloads: {reloads}, Insertions: {insertions}, Lost-Insertions: \
             {lost_insertions}, Replacements: {replacements}, One-Hit-Wonders: {one_hit_wonders}, \
             Prunes-Orphan: {prunes_orphan}, Prunes-Environment: {prunes_environment}, Empty: \
             {empty_entries}, Water-Level: {water_level}"
        );

        if log_enabled!(log::Level::Trace) && !self.evictions.is_empty() {
            let mut evictions = self.evictions.iter().collect::<Vec<_>>();
            evictions.sort_by_key(|e| e.1);
            let evictions = evictions
                .into_iter()
                .rev()
                .map(|(program_id, evictions)| {
                    format!("  {:<44}  {}", program_id.to_string(), evictions)
                })
                .collect::<Vec<_>>();
            let evictions = evictions.join("\n");
            trace!(
                "Eviction Details:\n  {:<44}  {}\n{}",
                "Program", "Count", evictions
            );
        }
    }

    fn combined_ema<const WINDOW1: u64, const WINDOW2: u64>(
        into_ema: &AtomicU64,
        from_ema: &AtomicU64,
        into_observations: u64,
        from_observations: u64,
    ) -> Option<u64> {
        // This is a mild non-sense, but there is no good mathematically rigorous way to merge
        // two independent EMA trackers AFAICT and this is the best I (nagisa) could come up
        // with…
        let other_ema_val = from_ema.load(Ordering::Relaxed);
        let other_ema_weight = std::cmp::max(WINDOW1, from_observations);
        let this_ema_val = into_ema.load(Ordering::Relaxed);
        let this_ema_weight = std::cmp::max(WINDOW2, into_observations);
        other_ema_val
            .wrapping_mul(other_ema_weight)
            .wrapping_add(this_ema_val.wrapping_mul(this_ema_weight))
            .checked_div(other_ema_weight.wrapping_add(this_ema_weight))
    }
}

#[cfg(feature = "metrics")]
/// Time measurements for loading a single [ProgramCacheEntry].
#[derive(Debug, Default)]
pub struct LoadProgramMetrics {
    /// Program address, but as text
    pub program_id: String,
    /// Microseconds it took to `create_program_runtime_environment`
    pub register_syscalls_us: u64,
    /// Microseconds it took to `Executable::<InvokeContext>::load`
    pub load_elf_us: u64,
    /// Microseconds it took to `executable.verify::<RequisiteVerifier>`
    pub verify_code_us: u64,
    /// Microseconds it took to `executable.jit_compile`
    pub jit_compile_us: u64,
}

#[cfg(feature = "metrics")]
impl LoadProgramMetrics {
    pub fn submit_datapoint(&self, timings: &mut ExecuteDetailsTimings) {
        timings.create_executor_register_syscalls_us += self.register_syscalls_us;
        timings.create_executor_load_elf_us += self.load_elf_us;
        timings.create_executor_verify_code_us += self.verify_code_us;
        timings.create_executor_jit_compile_us += self.jit_compile_us;
    }
}

impl<FG: ForkGraph> crate::loaded_programs::ProgramCache<FG> {
    /// Log per-entry statistics for each entry in the global cache.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn output_entry_stats(&self) {
        use {crate::program_cache_entry::ProgramCacheEntryType, std::fmt::Write};
        // The entry stats can become very verbose after some runtime. Rather than dumping them
        // to the log, we'd rather maintain a continuously updated file instead...
        static ENTRY_STAT_PATH: std::sync::LazyLock<Option<std::ffi::OsString>> =
            std::sync::LazyLock::new(|| std::env::var_os("AGAVE_PROGRAM_CACHE_ENTRY_STATS_PATH"));
        let Some(stat_path) = &*ENTRY_STAT_PATH else {
            log::trace!("Set AGAVE_PROGRAM_CACHE_ENTRY_STATS_PATH to write per-entry stats");
            return;
        };
        let mut output = String::new();
        let entries = self.get_flattened_entries_for_tests();
        for (addr, entry) in entries {
            let entry_ty = match &entry.program {
                ProgramCacheEntryType::FailedVerification(_) => "FailedVerification",
                ProgramCacheEntryType::Closed => "Closed",
                ProgramCacheEntryType::DelayVisibility => "DelayVisibility",
                ProgramCacheEntryType::Unloaded(_) => "Unloaded",
                ProgramCacheEntryType::Builtin(_) => "Builtin",
                #[cfg(not(all(not(target_os = "windows"), target_arch = "x86_64")))]
                ProgramCacheEntryType::Loaded(_) => "Loaded",
                #[cfg(all(not(target_os = "windows"), target_arch = "x86_64"))]
                ProgramCacheEntryType::Loaded(executable) => {
                    if executable.get_compiled_program().is_some() {
                        "JitCompiled"
                    } else {
                        "Loaded"
                    }
                }
            };
            let stats = &entry.stats;
            let uses = stats.uses.load(Ordering::Relaxed);
            let compiles = stats.compilations.load(Ordering::Relaxed);
            let comptime = stats.total_compilation_time_us.load(Ordering::Relaxed);
            let comptime_ema = stats.compilation_time_ema.load(Ordering::Relaxed) / EMA_SCALE;
            let invokes = stats.jit_invocations.load(Ordering::Relaxed);
            let jittime = stats.total_jit_execution_time_us.load(Ordering::Relaxed);
            let jittime_ema = stats.jit_execution_time_ema.load(Ordering::Relaxed) / EMA_SCALE;
            let interps = stats.interpreted_invocations.load(Ordering::Relaxed);
            let interptime = stats.total_interpretation_time_us.load(Ordering::Relaxed);
            let interpema = stats.interpretation_time_ema.load(Ordering::Relaxed) / EMA_SCALE;
            let _ = writeln!(
                &mut output,
                "{addr},{entry_ty},{uses},{compiles},{comptime},{comptime_ema},{invokes},\
                 {jittime},{jittime_ema},{interps},{interptime},{interpema}"
            );
        }
        if let Err(e) = std::fs::write(stat_path, output) {
            log::info!("Writing entry stats to {stat_path:?} failed: {e:?}");
        } else {
            log::debug!("Entry stats written to {stat_path:?}");
        }
    }
}
