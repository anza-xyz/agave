use {
    crate::{
        invoke_context::InvokeContext,
        loading_task::LoadingTaskWaiter,
        program_cache_entry::{
            ProgramCacheEntry, ProgramCacheEntryOwner, ProgramCacheEntryType, retention_score,
        },
        program_metrics::{EMA_SCALE, ProgramCacheStats},
    },
    log::error,
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_sbpf::program::BuiltinProgram,
    solana_svm_type_overrides::{
        rand::{Rng, rng},
        sync::{Arc, Mutex, RwLock, atomic::Ordering},
        thread,
    },
    std::{
        collections::{HashMap, hash_map::Entry},
        sync::Weak,
    },
};

#[repr(transparent)]
#[derive(Clone, Debug)]
pub struct ProgramRuntimeEnvironment(Arc<BuiltinProgram<InvokeContext<'static, 'static>>>);
impl std::hash::Hash for ProgramRuntimeEnvironment {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        Arc::<BuiltinProgram<InvokeContext<'static, 'static>>>::as_ptr(&self.0).hash(state);
    }
}
impl PartialEq for ProgramRuntimeEnvironment {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for ProgramRuntimeEnvironment {}
impl std::ops::Deref for ProgramRuntimeEnvironment {
    type Target = Arc<BuiltinProgram<InvokeContext<'static, 'static>>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl ProgramRuntimeEnvironment {
    pub fn from(inner: BuiltinProgram<InvokeContext<'static, 'static>>) -> Self {
        Self(Arc::new(inner))
    }

    pub const fn from_ref<'a>(
        inner: &'a Arc<BuiltinProgram<InvokeContext<'static, 'static>>>,
    ) -> &'a Self {
        // Safety: This wrapper type is transparent and shares the same representation as the underlying type
        unsafe { std::mem::transmute(inner) }
    }
}

/// Paired execution and deployment environments.
///
/// Registered functions within each program runtime environment (syscalls)
/// depend on per-epoch feature gate statuses. In most cases, the list of
/// registered functions in the two environments will be the same. However,
/// it's possible that the effective epoch of deployment could be in the
/// *next epoch*.
pub struct ProgramRuntimeEnvironments {
    /// Environment compiled for the current epoch in which programs are
    /// executing.
    execution: ProgramRuntimeEnvironment,
    /// Environment compiled for the epoch of the next slot at which a program
    /// deployed in the current slot will execute.
    deployment: ProgramRuntimeEnvironment,
}

impl ProgramRuntimeEnvironments {
    /// Create a new ProgramRuntimeEnvironments from an `execution` and
    /// `deployment` environment.
    pub fn new(
        execution: ProgramRuntimeEnvironment,
        deployment: ProgramRuntimeEnvironment,
    ) -> Self {
        Self {
            execution,
            deployment,
        }
    }

    /// Get the program runtime environment for execution.
    pub fn get_env_for_execution(&self) -> &ProgramRuntimeEnvironment {
        &self.execution
    }

    /// Get the program runtime environment for deployment.
    pub fn get_env_for_deployment(&self) -> &ProgramRuntimeEnvironment {
        &self.deployment
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn mock() -> Self {
        Self {
            execution: get_mock_program_runtime_environment(),
            deployment: get_mock_program_runtime_environment(),
        }
    }
}

#[cfg(feature = "dev-context-only-utils")]
pub fn get_mock_program_runtime_environment() -> ProgramRuntimeEnvironment {
    static MOCK_ENVIRONMENT: std::sync::OnceLock<ProgramRuntimeEnvironment> =
        std::sync::OnceLock::<ProgramRuntimeEnvironment>::new();
    MOCK_ENVIRONMENT
        .get_or_init(|| ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock()))
        .clone()
}

pub const MAX_LOADED_ENTRY_COUNT: usize = 1024;
pub const MAX_TOMBSTONE_AGE_IN_SLOTS: u64 = 2250; // 15 Minutes at 400ms slot time

/// A percentage, expected to be in the range `0..=100`.
pub type Percent = u8;

/// The given percentage of [`MAX_LOADED_ENTRY_COUNT`], as an entry count.
/// Equivalent to the former `percentage` crate's
/// `Percentage::from(percent).apply_to(MAX_LOADED_ENTRY_COUNT)`,
/// i.e. floor(MAX_LOADED_ENTRY_COUNT * percent / 100).
fn percent_of_max_entries(percent: Percent) -> usize {
    debug_assert!(percent <= 100, "percent must be <= 100");
    MAX_LOADED_ENTRY_COUNT.saturating_mul(percent as usize) / 100
}

/// Relationship between two fork IDs
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum BlockRelation {
    /// The slot is on the same fork and is an ancestor of the other slot
    Ancestor,
    /// The two slots are equal and are on the same fork
    Equal,
    /// The slot is on the same fork and is a descendant of the other slot
    Descendant,
    /// The slots are on two different forks and may have had a common ancestor at some point
    Unrelated,
    /// Either one or both of the slots are either older than the latest root, or are in future
    Unknown,
}

/// Maps relationship between two slots.
pub trait ForkGraph {
    /// Returns the BlockRelation of A to B
    fn relationship(&self, a: Slot, b: Slot) -> BlockRelation;
}

/// Globally manages the transition between environments at the epoch boundary
#[derive(Debug, Default)]
pub struct EpochBoundaryPreparation {
    /// The epoch of the upcoming_environment
    pub upcoming_epoch: Epoch,
    /// Anticipated replacement for `environments` at the next epoch
    ///
    /// This is `None` during most of an epoch, and only `Some` around the boundaries (at the end and beginning of an epoch).
    /// More precisely, it starts with the cache preparation phase a few hundred slots before the epoch boundary,
    /// and it ends with the first rerooting after the epoch boundary.
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

    /// Before rerooting the blockstore this concludes the epoch boundary preparation
    pub fn reroot(&mut self, epoch: Epoch) -> Option<ProgramRuntimeEnvironment> {
        if epoch == self.upcoming_epoch
            && let Some(upcoming_environment) = self.upcoming_environment.take()
        {
            self.programs_to_recompile.clear();
            return Some(upcoming_environment);
        }

        None
    }
}

/// Input of ProgramCache::extract()
#[derive(Clone, PartialEq, Debug)]
pub struct ProgramToLoad<'a> {
    /// The program address
    pub program_id: &'a Pubkey,
    /// The program loader
    pub loader: ProgramCacheEntryOwner,
    /// The slot the program was (re)deployed in, as reported by the program
    /// account on the caller's own fork.
    ///
    /// For Loader V1/V2 and builtin programs, this is 0.
    pub deployment_slot: Slot,
}

#[derive(Debug)]
pub(crate) enum IndexImplementation {
    /// Fork-graph aware index implementation
    V1 {
        /// A two level index:
        ///
        /// - the first level is for the address at which programs are deployed
        /// - the second level for the slot (and thus also fork), sorted by slot
        ///   number from smallest to largest.
        entries: HashMap<Pubkey, Vec<Arc<ProgramCacheEntry>>>,
        /// The entries that are getting loaded and have not yet finished loading.
        ///
        /// The key is the program address, the value is a tuple of the slot in which the program is
        /// being loaded and the thread ID doing the load.
        ///
        /// It is possible that multiple TX batches from different slots need different versions of a
        /// program. The deployment slot of a program is only known after load tho,
        /// so all loads for a given program key are serialized.
        loading_entries: Mutex<HashMap<Pubkey, (Slot, thread::ThreadId)>>,
    },
}

/// This structure is the global cache of loaded, verified and compiled programs.
///
/// It ...
/// - is validator global and fork graph aware, so it can optimize the commonalities across banks.
/// - handles the visibility rules of un/re/deployments.
/// - stores the usage statistics and verification status of each program.
/// - is elastic and uses a probabilistic eviction strategy based on the usage statistics.
/// - also keeps the compiled executables around, but only for the most used programs.
/// - supports various kinds of tombstones to avoid loading programs which can not be loaded.
/// - cleans up entries on orphan branches when the block store is rerooted.
/// - supports the cache preparation phase before feature activations which can change cached programs.
/// - manages the environments of the programs and upcoming environments for the next epoch.
/// - allows for cooperative loading of TX batches which hit the same missing programs simultaneously.
/// - enforces that all programs used in a batch are eagerly loaded ahead of execution.
/// - is not persisted to disk or a snapshot, so it needs to cold start and warm up first.
pub struct ProgramCache<FG: ForkGraph> {
    /// Index of the cached entries and cooperative loading tasks
    pub(crate) index: IndexImplementation,
    /// The slot of the last rerooting
    pub latest_root_slot: Slot,
    /// Statistics counters
    pub stats: ProgramCacheStats,
    /// Reference to the block store
    pub fork_graph: Option<Weak<RwLock<FG>>>,
    /// Coordinates TX batches waiting for others to complete their task during cooperative loading
    pub loading_task_waiter: Arc<LoadingTaskWaiter>,
}

impl<FG: ForkGraph> std::fmt::Debug for ProgramCache<FG> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProgramCache")
            .field("root slot", &self.latest_root_slot)
            .field("stats", &self.stats)
            .field("index", &self.index)
            .finish()
    }
}

/// Local view into [ProgramCache] which was extracted for a specific TX batch.
///
/// This isolation enables the global [ProgramCache] to continue to evolve (e.g. evictions),
/// while the TX batch is guaranteed it will continue to find all the programs it requires.
/// For program management instructions this also buffers them before they are merged back into the global [ProgramCache].
#[derive(Clone, Debug, Default)]
pub struct ProgramCacheForTxBatch {
    /// Pubkey is the address of a program.
    /// ProgramCacheEntry is the corresponding program entry valid for the slot in which a transaction is being executed.
    entries: HashMap<Pubkey, Arc<ProgramCacheEntry>>,
    /// Program entries modified during the transaction batch.
    modified_entries: HashMap<Pubkey, Arc<ProgramCacheEntry>>,
    slot: Slot,
    pub hit_max_limit: bool,
    pub loaded_missing: bool,
    pub merged_modified: bool,
}

impl ProgramCacheForTxBatch {
    pub fn new(slot: Slot) -> Self {
        Self {
            entries: HashMap::new(),
            modified_entries: HashMap::new(),
            slot,
            hit_max_limit: false,
            loaded_missing: false,
            merged_modified: false,
        }
    }

    /// Refill the cache with a single entry. It's typically called during transaction loading, and
    /// transaction processing (for program management instructions).
    /// It replaces the existing entry (if any) with the provided entry. The return value contains
    /// `true` if an entry existed.
    /// The function also returns the newly inserted value.
    pub fn replenish(
        &mut self,
        key: Pubkey,
        entry: Arc<ProgramCacheEntry>,
    ) -> (bool, Arc<ProgramCacheEntry>) {
        (self.entries.insert(key, entry.clone()).is_some(), entry)
    }

    /// Store an entry in `modified_entries` for a program modified during the
    /// transaction batch.
    pub fn store_modified_entry(&mut self, key: Pubkey, entry: Arc<ProgramCacheEntry>) {
        self.modified_entries.insert(key, entry);
    }

    /// Drain the program cache's modified entries, returning the owned
    /// collection.
    pub fn drain_modified_entries(&mut self) -> HashMap<Pubkey, Arc<ProgramCacheEntry>> {
        std::mem::take(&mut self.modified_entries)
    }

    pub fn find(&self, key: &Pubkey) -> Option<Arc<ProgramCacheEntry>> {
        // First lookup the cache of the programs modified by the current
        // transaction. If not found, lookup the cache of the cache of the
        // programs that are loaded for the transaction batch.
        self.modified_entries
            .get(key)
            .or_else(|| self.entries.get(key))
            .map(|entry| {
                if entry.is_implicit_delay_visibility_tombstone(self.slot) {
                    // Found a program entry on the current fork, but it's not effective
                    // yet. It indicates that the program has delayed visibility. Return
                    // the tombstone to reflect that.
                    Arc::new(ProgramCacheEntry::new_delay_visibility_tombstone(
                        entry.deployment_slot,
                        entry.account_owner,
                        Arc::clone(&entry.stats),
                    ))
                } else {
                    entry.clone()
                }
            })
    }

    pub fn slot(&self) -> Slot {
        self.slot
    }

    pub fn set_slot_for_tests(&mut self, slot: Slot) {
        self.slot = slot;
    }

    pub fn merge(&mut self, modified_entries: &HashMap<Pubkey, Arc<ProgramCacheEntry>>) {
        modified_entries.iter().for_each(|(key, entry)| {
            self.merged_modified = true;
            self.replenish(*key, entry.clone());
        })
    }

    /// Remove an entry from the `entries` list.
    /// Note: DOES NOT remove modified entries!
    pub fn remove_entry(&mut self, key: &Pubkey) {
        self.entries.remove(key);
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<FG: ForkGraph> ProgramCache<FG> {
    pub fn new(root_slot: Slot) -> Self {
        Self {
            index: IndexImplementation::V1 {
                entries: HashMap::new(),
                loading_entries: Mutex::new(HashMap::new()),
            },
            latest_root_slot: root_slot,
            stats: ProgramCacheStats::default(),
            fork_graph: None,
            loading_task_waiter: Arc::new(LoadingTaskWaiter::default()),
        }
    }

    pub fn set_fork_graph(&mut self, fork_graph: Weak<RwLock<FG>>) {
        self.fork_graph = Some(fork_graph);
    }

    /// Insert a single entry. It's typically called during transaction loading,
    /// when the cache doesn't contain the entry corresponding to program `key`.
    pub fn assign_program(
        &mut self,
        program_runtime_environment: &ProgramRuntimeEnvironment,
        key: Pubkey,
        _current_slot: Slot,
        entry: Arc<ProgramCacheEntry>,
    ) -> bool {
        debug_assert!(
            !matches!(&entry.program, ProgramCacheEntryType::DelayVisibility),
            "Unexpected assignment of a DelayVisibility tombstone"
        );
        // This function always returns `true` during normal operation.
        // Only during the cache preparation phase this can return `false`
        // for entries with `upcoming_environment`.
        fn is_current_env(
            program_runtime_environment: &ProgramRuntimeEnvironment,
            env_opt: Option<&ProgramRuntimeEnvironment>,
        ) -> bool {
            env_opt
                .map(|env| env == program_runtime_environment)
                .unwrap_or(true)
        }
        match &mut self.index {
            IndexImplementation::V1 { entries, .. } => {
                let slot_versions = &mut entries.entry(key).or_default();
                let insertion_point = slot_versions.binary_search_by(|at| {
                    at.deployment_slot
                        .cmp(&entry.deployment_slot)
                        .then(at.account_owner.cmp(&entry.account_owner))
                        .then(
                            // This `.then()` has no effect during normal operation.
                            // Only during the cache preparation phase this does allow entries
                            // which only differ in their environment to be interleaved in `slot_versions`.
                            is_current_env(
                                program_runtime_environment,
                                at.program.get_environment(),
                            )
                            .cmp(&is_current_env(
                                program_runtime_environment,
                                entry.program.get_environment(),
                            )),
                        )
                });
                match insertion_point {
                    Ok(index) => {
                        let existing = slot_versions.get_mut(index).unwrap();
                        match (&existing.program, &entry.program) {
                            (
                                ProgramCacheEntryType::Builtin(_),
                                ProgramCacheEntryType::Builtin(_),
                            )
                            | (ProgramCacheEntryType::Closed, ProgramCacheEntryType::Unloaded(_))
                            | (
                                ProgramCacheEntryType::Unloaded(_),
                                ProgramCacheEntryType::Loaded(_),
                            )
                            | (
                                ProgramCacheEntryType::Unloaded(_),
                                ProgramCacheEntryType::FailedVerification(_),
                            ) => {}
                            _ => {
                                // Something is wrong, I can feel it ...
                                error!(
                                    "ProgramCache::assign_program() failed key={key:?} \
                                     existing={slot_versions:?} entry={entry:?}"
                                );
                                debug_assert!(false, "Unexpected replacement of an entry");
                                self.stats.replacements.fetch_add(1, Ordering::Relaxed);
                                return true;
                            }
                        }
                        entry.stats.merge_from(&existing.stats);
                        *existing = Arc::clone(&entry);
                        self.stats.reloads.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(index) => {
                        self.stats.insertions.fetch_add(1, Ordering::Relaxed);
                        slot_versions.insert(index, Arc::clone(&entry));
                    }
                }
                // Remove existing entries in the same deployment slot unless they are for a different
                // environment.
                // This overwrites the current status of a program in program management instructions.
                slot_versions.retain(|existing| {
                    existing.deployment_slot != entry.deployment_slot
                        || existing
                            .program
                            .get_environment()
                            .zip(entry.program.get_environment())
                            .map(|(a, b)| a != b)
                            .unwrap_or(false)
                        || Arc::ptr_eq(existing, &entry)
                });
            }
        }
        false
    }

    pub fn prune_by_deployment_slot(&mut self, slot: Slot) {
        match &mut self.index {
            IndexImplementation::V1 { entries, .. } => {
                for second_level in entries.values_mut() {
                    second_level.retain(|entry| entry.deployment_slot != slot);
                }
                self.remove_programs_with_no_entries();
            }
        }
    }

    /// Before rerooting the blockstore this removes all superfluous entries
    pub fn prune(
        &mut self,
        new_root_slot: Slot,
        new_environment: Option<ProgramRuntimeEnvironment>,
        fork_graph: &FG,
    ) {
        match &mut self.index {
            IndexImplementation::V1 { entries, .. } => {
                let tombstone_slot_cutoff =
                    new_root_slot.saturating_sub(MAX_TOMBSTONE_AGE_IN_SLOTS);
                entries.retain(|_id, second_level| {
                    // Clean up tombstones and unloaded entries
                    if let [candidate] = &second_level[..]
                        && (matches!(candidate.program, ProgramCacheEntryType::Unloaded(_))
                            || candidate.is_tombstone())
                        && candidate.deployment_slot <= self.latest_root_slot
                        && candidate.latest_access_slot.load(Ordering::Relaxed)
                            < tombstone_slot_cutoff
                    {
                        self.stats.prunes_stale.fetch_add(1, Ordering::Relaxed);
                        return false;
                    }
                    // Remove entries un/re/deployed on orphan forks
                    let mut first_ancestor_found = false;
                    let mut seen_environments: Vec<&ProgramRuntimeEnvironment> = Vec::new();
                    *second_level = second_level
                        .iter()
                        .rev()
                        .filter(|entry| {
                            let relation =
                                fork_graph.relationship(entry.deployment_slot, new_root_slot);
                            if entry.deployment_slot >= new_root_slot {
                                let keep = matches!(
                                    relation,
                                    BlockRelation::Equal | BlockRelation::Descendant
                                );
                                if !keep {
                                    self.stats.prunes_orphan.fetch_add(1, Ordering::Relaxed);
                                }
                                keep
                            } else if matches!(relation, BlockRelation::Ancestor)
                                || entry.deployment_slot <= self.latest_root_slot
                            {
                                if !first_ancestor_found {
                                    first_ancestor_found = true;
                                    seen_environments.extend(entry.program.get_environment());
                                    return true;
                                }
                                // Do not prune the entry if the runtime environment of the entry has
                                // not been seen yet among the entries already kept. Different
                                // environment indicates that this entry might belong to an older epoch
                                // that had a different environment (e.g. different feature set). Once
                                // the root moves to the new/current epoch, the entry will get pruned.
                                // But, until then the entry might still be getting used by an older
                                // slot. Only the newest entry on each is needed for that.
                                if let Some(entry_env) = entry.program.get_environment()
                                    && !seen_environments.is_empty()
                                    && !seen_environments.contains(&entry_env)
                                {
                                    seen_environments.push(entry_env);
                                    return true;
                                }
                                self.stats.prunes_orphan.fetch_add(1, Ordering::Relaxed);
                                false
                            } else {
                                self.stats.prunes_orphan.fetch_add(1, Ordering::Relaxed);
                                false
                            }
                        })
                        .filter(|entry| {
                            // Remove outdated environment of previous feature set
                            if let Some(new_environment) = new_environment.as_ref()
                                && !Self::matches_environment(entry, new_environment)
                            {
                                self.stats
                                    .prunes_environment
                                    .fetch_add(1, Ordering::Relaxed);
                                return false;
                            }
                            true
                        })
                        .cloned()
                        .collect();
                    second_level.reverse();
                    true
                });
            }
        }
        self.remove_programs_with_no_entries();
        debug_assert!(self.latest_root_slot <= new_root_slot);
        self.latest_root_slot = new_root_slot;
    }

    fn matches_environment(
        entry: &Arc<ProgramCacheEntry>,
        program_runtime_environment: &ProgramRuntimeEnvironment,
    ) -> bool {
        let Some(environment) = entry.program.get_environment() else {
            return true;
        };
        environment == program_runtime_environment
    }

    /// Extracts a subset of the programs relevant to a transaction batch
    /// and returns which program accounts the accounts DB needs to load.
    pub fn extract(
        &self,
        search_for: &mut Vec<ProgramToLoad>,
        loaded_programs_for_tx_batch: &mut ProgramCacheForTxBatch,
        program_runtime_environment_for_execution: &ProgramRuntimeEnvironment,
        increment_usage_counter: bool,
        count_hits_and_misses: bool,
    ) -> Option<Pubkey> {
        debug_assert!(self.fork_graph.is_some());
        let fork_graph = self.fork_graph.as_ref().unwrap().upgrade().unwrap();
        let locked_fork_graph = fork_graph.read().unwrap();
        let entries_in_batch = loaded_programs_for_tx_batch.entries.len();
        let mut cooperative_loading_task = None;
        match &self.index {
            IndexImplementation::V1 {
                entries,
                loading_entries,
            } => {
                search_for.retain(|program_to_load| {
                    if let Some(second_level) = entries.get(program_to_load.program_id) {
                        for entry in second_level.iter().rev() {
                            // The entry must have been deployed in the slot reported by
                            // the caller's own program account, and by the same loader.
                            if program_to_load.deployment_slot != entry.deployment_slot
                                || program_to_load.loader != entry.account_owner
                            {
                                continue;
                            }

                            // At this point we're sitting on an entry with a matching
                            // deployment slot and owner.
                            //
                            // Fork-graph analysis below this is now redundant, and it
                            // can be removed in follow-up.
                            let entry_in_same_branch = entry.deployment_slot
                                <= self.latest_root_slot
                                || matches!(
                                    locked_fork_graph.relationship(
                                        entry.deployment_slot,
                                        loaded_programs_for_tx_batch.slot
                                    ),
                                    BlockRelation::Equal | BlockRelation::Ancestor
                                );
                            if entry_in_same_branch {
                                let entry_is_effective =
                                    loaded_programs_for_tx_batch.slot >= entry.effective_slot();
                                let entry_to_return = if entry_is_effective {
                                    if !Self::matches_environment(
                                        entry,
                                        program_runtime_environment_for_execution,
                                    ) {
                                        // We found an entry that would work, had its environment
                                        // matched the one we're planning to use for this slot. A
                                        // sibling compiled against that environment may follow.
                                        continue;
                                    }
                                    if let ProgramCacheEntryType::Unloaded(_environment) =
                                        &entry.program
                                    {
                                        break;
                                    }
                                    entry.clone()
                                } else if entry.is_implicit_delay_visibility_tombstone(
                                    loaded_programs_for_tx_batch.slot,
                                ) {
                                    // Found a program entry on the current fork, but it's not effective
                                    // yet. It indicates that the program has delayed visibility. Return
                                    // the tombstone to reflect that.
                                    Arc::new(ProgramCacheEntry::new_delay_visibility_tombstone(
                                        entry.deployment_slot,
                                        entry.account_owner,
                                        Arc::clone(&entry.stats),
                                    ))
                                } else {
                                    continue;
                                };
                                entry.update_access_slot(loaded_programs_for_tx_batch.slot);
                                if increment_usage_counter {
                                    entry_to_return.stats.uses.fetch_add(1, Ordering::Relaxed);
                                }
                                loaded_programs_for_tx_batch
                                    .entries
                                    .insert(*program_to_load.program_id, entry_to_return);
                                return false;
                            }
                        }
                    }
                    if cooperative_loading_task.is_none() {
                        let mut loading_entries = loading_entries.lock().unwrap();
                        let entry = loading_entries.entry(*program_to_load.program_id);
                        if let Entry::Vacant(entry) = entry {
                            entry.insert((
                                loaded_programs_for_tx_batch.slot,
                                thread::current().id(),
                            ));
                            cooperative_loading_task = Some(*program_to_load.program_id);
                        }
                    }
                    true
                });
            }
        }
        drop(locked_fork_graph);
        if count_hits_and_misses {
            let misses = search_for.len() as u64;
            let hits = loaded_programs_for_tx_batch
                .entries
                .len()
                .saturating_sub(entries_in_batch) as u64;
            self.stats.misses.fetch_add(misses, Ordering::Relaxed);
            self.stats.hits.fetch_add(hits, Ordering::Relaxed);
        }
        cooperative_loading_task
    }

    /// Called by Bank::replenish_program_cache() for each program that is done loading.
    pub fn finish_cooperative_loading_task(
        &mut self,
        program_runtime_environment: &ProgramRuntimeEnvironment,
        current_slot: Slot,
        key: Pubkey,
        loaded_program: Arc<ProgramCacheEntry>,
    ) -> bool {
        match &mut self.index {
            IndexImplementation::V1 {
                loading_entries, ..
            } => {
                let loading_thread = loading_entries.get_mut().unwrap().remove(&key);
                debug_assert_eq!(loading_thread, Some((current_slot, thread::current().id())));
                // Check that it will be visible to our own fork once inserted
                if loaded_program.deployment_slot > self.latest_root_slot
                    && !matches!(
                        self.fork_graph
                            .as_ref()
                            .unwrap()
                            .upgrade()
                            .unwrap()
                            .read()
                            .unwrap()
                            .relationship(loaded_program.deployment_slot, current_slot),
                        BlockRelation::Equal | BlockRelation::Ancestor
                    )
                {
                    self.stats.lost_insertions.fetch_add(1, Ordering::Relaxed);
                }
                let was_occupied = self.assign_program(
                    program_runtime_environment,
                    key,
                    current_slot,
                    loaded_program,
                );
                self.loading_task_waiter.notify();
                was_occupied
            }
        }
    }

    pub fn merge(
        &mut self,
        program_runtime_environment: &ProgramRuntimeEnvironment,
        current_slot: Slot,
        modified_entries: &HashMap<Pubkey, Arc<ProgramCacheEntry>>,
    ) {
        modified_entries.iter().for_each(|(key, entry)| {
            self.assign_program(
                program_runtime_environment,
                *key,
                current_slot,
                entry.clone(),
            );
        })
    }

    /// Returns the list of entries which are verified and compiled.
    pub fn get_flattened_entries(&self) -> Vec<(Pubkey, Arc<ProgramCacheEntry>)> {
        match &self.index {
            IndexImplementation::V1 { entries, .. } => entries
                .iter()
                .flat_map(|(id, second_level)| {
                    second_level
                        .iter()
                        .filter_map(move |program| match program.program {
                            ProgramCacheEntryType::Loaded(_) => Some((*id, program.clone())),
                            _ => None,
                        })
                })
                .collect(),
        }
    }

    /// Returns the list of all entries in the cache.
    #[cfg(feature = "dev-context-only-utils")]
    pub fn get_flattened_entries_for_tests(&self) -> Vec<(Pubkey, Arc<ProgramCacheEntry>)> {
        match &self.index {
            IndexImplementation::V1 { entries, .. } => entries
                .iter()
                .flat_map(|(id, second_level)| {
                    second_level.iter().map(|program| (*id, program.clone()))
                })
                .collect(),
        }
    }

    /// Returns the slot versions for the given program id.
    pub fn get_slot_versions_for_tests(&self, key: &Pubkey) -> &[Arc<ProgramCacheEntry>] {
        match &self.index {
            IndexImplementation::V1 { entries, .. } => entries
                .get(key)
                .map(|second_level| second_level.as_ref())
                .unwrap_or(&[]),
        }
    }

    /// Unloads programs which were used infrequently
    pub fn sort_and_unload(&mut self, shrink_to_percent: Percent) {
        let mut sorted_candidates = self.get_flattened_entries();
        sorted_candidates
            .sort_by_cached_key(|(_id, program)| program.stats.uses.load(Ordering::Relaxed));
        let num_to_unload = sorted_candidates
            .len()
            .saturating_sub(percent_of_max_entries(shrink_to_percent));
        for (program, entry) in sorted_candidates.iter().take(num_to_unload) {
            self.unload_program_entry(*program, entry);
        }
    }

    /// Evicts programs using random selection, choosing the worst scoring program out of the
    /// entries sampled.
    ///
    /// The eviction is performed enough number of times to reduce the cache usage to the given
    /// percentage.
    pub fn evict_using_random_selection(&mut self, shrink_to_percent: Percent, now: Slot) {
        let mut candidates = self.get_flattened_entries();
        let mut rng = rng();
        self.stats
            .water_level
            .store(candidates.len() as u64, Ordering::Relaxed);
        let num_to_unload = candidates
            .len()
            .saturating_sub(percent_of_max_entries(shrink_to_percent));
        let mut sample_entry = |candidates: &Vec<(Pubkey, Arc<ProgramCacheEntry>)>| {
            // gen_range is deprecated in favor of random_range in rand>=0.9, but we also get
            // rnd() from shuttle, which doesn't yet support rand 0.9 APIs
            #[cfg(feature = "shuttle-test")]
            let index = rng.gen_range(0..candidates.len());
            #[cfg(not(feature = "shuttle-test"))]
            let index = rng.random_range(0..candidates.len());
            let usage_counter = candidates
                .get(index)
                .expect("Failed to get cached entry")
                .1
                .retention_score();
            (index, usage_counter)
        };

        // Random sampling with just 2 choices can frequently lead to a situation where both
        // entries chosen have relatively high retention scores, having us to pick one out of two
        // poor options. We can tell what a relatively high retention score is, so we can make a
        // few additional samples until we hit some other entry that isn't as highly scoring.
        //
        // Note that the "high enough" compilation time and use count numbers used here are
        // relatively arbitrary.
        const MAX_ADDITIONAL_SAMPLES: usize = 3;
        let avoid_evicting_above_score = retention_score(now, 500 * EMA_SCALE, 500);
        for _ in 0..num_to_unload {
            let (mut index, mut score) = sample_entry(&candidates);
            for _ in 0..MAX_ADDITIONAL_SAMPLES {
                let (sample_index, sample_score) = sample_entry(&candidates);
                if score > sample_score {
                    index = sample_index;
                    score = sample_score;
                }
                if score < avoid_evicting_above_score {
                    break;
                }
            }
            let (id, entry) = candidates.swap_remove(index);
            self.unload_program_entry(id, &entry);
        }
    }

    /// Removes all the entries at the given keys, if they exist
    pub fn remove_programs(&mut self, keys: impl Iterator<Item = Pubkey>) {
        match &mut self.index {
            IndexImplementation::V1 { entries, .. } => {
                for k in keys {
                    entries.remove(&k);
                }
            }
        }
    }

    /// This function removes the given entry for the given program from the cache.
    /// The function expects that the program and entry exists in the cache. Otherwise it'll panic.
    fn unload_program_entry(&mut self, id: Pubkey, remove_entry: &Arc<ProgramCacheEntry>) {
        match &mut self.index {
            IndexImplementation::V1 { entries, .. } => {
                let second_level = entries.get_mut(&id).expect("Cache lookup failed");
                let candidate = second_level
                    .iter_mut()
                    .find(|entry| Arc::ptr_eq(entry, remove_entry))
                    .expect("Program entry not found");

                // Only loaded entries shall be unloaded by eviction.
                if let ProgramCacheEntryType::Loaded(_) = candidate.program
                    && let Some(unloaded) = candidate.to_unloaded()
                {
                    if candidate.stats.uses.load(Ordering::Relaxed) == 1 {
                        self.stats.one_hit_wonders.fetch_add(1, Ordering::Relaxed);
                    }
                    self.stats
                        .evictions
                        .entry(id)
                        .and_modify(|c| *c = c.saturating_add(1))
                        .or_insert(1);
                    *candidate = Arc::new(unloaded);
                }
            }
        }
    }

    fn remove_programs_with_no_entries(&mut self) {
        match &mut self.index {
            IndexImplementation::V1 { entries, .. } => {
                let num_programs_before_removal = entries.len();
                entries.retain(|_key, second_level| !second_level.is_empty());
                if entries.len() < num_programs_before_removal {
                    self.stats.empty_entries.fetch_add(
                        num_programs_before_removal.saturating_sub(entries.len()) as u64,
                        Ordering::Relaxed,
                    );
                }
            }
        }
    }
}

#[cfg(feature = "frozen-abi")]
impl solana_frozen_abi::abi_example::AbiExample for ProgramCacheEntry {
    fn example() -> Self {
        // ProgramCacheEntry isn't serializable by definition.
        Self::default()
    }
}

#[cfg(feature = "frozen-abi")]
impl<FG: ForkGraph> solana_frozen_abi::abi_example::AbiExample for ProgramCache<FG> {
    fn example() -> Self {
        // ProgramCache isn't serializable by definition.
        Self::new(Slot::default())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        crate::{
            loaded_programs::{
                BlockRelation, ForkGraph, IndexImplementation, MAX_TOMBSTONE_AGE_IN_SLOTS, Percent,
                ProgramCache, ProgramCacheForTxBatch, ProgramRuntimeEnvironment, ProgramToLoad,
                get_mock_program_runtime_environment,
            },
            program_cache_entry::{
                ProgramCacheEntry, ProgramCacheEntryOwner, ProgramCacheEntryType,
            },
            program_metrics::ProgramStatistics,
        },
        assert_matches::assert_matches,
        solana_clock::Slot,
        solana_pubkey::Pubkey,
        solana_sbpf::{elf::Executable, program::BuiltinProgram},
        solana_svm_type_overrides::{
            sync::{
                Arc, RwLock,
                atomic::{AtomicU64, Ordering},
            },
            thread,
        },
        std::{fs::File, io::Read, ops::ControlFlow},
        test_case::{test_case, test_matrix},
    };

    fn new_test_entry(deployment_slot: Slot) -> Arc<ProgramCacheEntry> {
        new_test_entry_with_usage(deployment_slot, ProgramStatistics::default())
    }

    fn new_closed_entry(_env: ProgramRuntimeEnvironment) -> ProgramCacheEntryType {
        ProgramCacheEntryType::Closed
    }

    fn new_builtin_entry(_env: ProgramRuntimeEnvironment) -> ProgramCacheEntryType {
        ProgramCacheEntryType::Builtin(BuiltinProgram::new_mock())
    }

    fn new_failed_verification_entry(env: ProgramRuntimeEnvironment) -> ProgramCacheEntryType {
        ProgramCacheEntryType::FailedVerification(env)
    }

    fn new_unloaded_entry(env: ProgramRuntimeEnvironment) -> ProgramCacheEntryType {
        ProgramCacheEntryType::Unloaded(env)
    }

    fn new_loaded_entry(env: ProgramRuntimeEnvironment) -> ProgramCacheEntryType {
        let mut elf = Vec::new();
        File::open("../programs/bpf_loader/test_elfs/out/noop_aligned.so")
            .unwrap()
            .read_to_end(&mut elf)
            .unwrap();
        let executable = Executable::load(&elf, Arc::clone(&*env)).unwrap();
        ProgramCacheEntryType::Loaded(executable)
    }

    fn new_test_entry_with_owner(
        deployment_slot: Slot,
        account_owner: ProgramCacheEntryOwner,
        program: ProgramCacheEntryType,
    ) -> Arc<ProgramCacheEntry> {
        Arc::new(ProgramCacheEntry {
            program,
            account_owner,
            deployment_slot,
            stats: Arc::default(),
            latest_access_slot: AtomicU64::default(),
        })
    }

    fn new_test_cache_with_fork_graph(
        relation: BlockRelation,
    ) -> (ProgramCache<TestForkGraph>, Arc<RwLock<TestForkGraph>>) {
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let fork_graph = Arc::new(RwLock::new(TestForkGraph { relation }));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));
        (cache, fork_graph)
    }

    pub(crate) fn new_test_entry_with_usage(
        deployment_slot: Slot,
        stats: ProgramStatistics,
    ) -> Arc<ProgramCacheEntry> {
        Arc::new(ProgramCacheEntry {
            program: new_loaded_entry(get_mock_program_runtime_environment()),
            account_owner: ProgramCacheEntryOwner::LoaderV2,
            deployment_slot,
            stats: Arc::new(stats),
            latest_access_slot: AtomicU64::new(deployment_slot),
        })
    }

    fn new_test_builtin_entry(deployment_slot: Slot) -> Arc<ProgramCacheEntry> {
        Arc::new(ProgramCacheEntry {
            program: ProgramCacheEntryType::Builtin(BuiltinProgram::new_mock()),
            account_owner: ProgramCacheEntryOwner::NativeLoader,
            deployment_slot,
            stats: Arc::default(),
            latest_access_slot: AtomicU64::default(),
        })
    }

    fn set_failed_verification_tombstone<FG: ForkGraph>(
        cache: &mut ProgramCache<FG>,
        key: Pubkey,
        current_slot: Slot,
        env: ProgramRuntimeEnvironment,
    ) -> Arc<ProgramCacheEntry> {
        let program = Arc::new(ProgramCacheEntry::new_failed_verification_tombstone(
            current_slot,
            ProgramCacheEntryOwner::LoaderV2,
            ProgramRuntimeEnvironment::clone(&env),
        ));
        cache.assign_program(&env, key, current_slot, program.clone());
        program
    }

    fn insert_unloaded_entry<FG: ForkGraph>(
        cache: &mut ProgramCache<FG>,
        key: Pubkey,
        current_slot: Slot,
    ) -> Arc<ProgramCacheEntry> {
        let env = get_mock_program_runtime_environment();
        let loaded = new_test_entry_with_usage(current_slot, ProgramStatistics::default());
        let unloaded = Arc::new(loaded.to_unloaded().expect("Failed to unload the program"));
        cache.assign_program(&env, key, current_slot, unloaded.clone());
        unloaded
    }

    fn num_matching_entries<P, FG>(cache: &ProgramCache<FG>, predicate: P) -> usize
    where
        P: Fn(&ProgramCacheEntryType) -> bool,
        FG: ForkGraph,
    {
        cache
            .get_flattened_entries_for_tests()
            .iter()
            .filter(|(_key, program)| predicate(&program.program))
            .count()
    }

    #[expect(clippy::arithmetic_side_effects)]
    fn program_deploy_test_helper(
        cache: &mut ProgramCache<TestForkGraph>,
        program: Pubkey,
        deployment_slots: Vec<Slot>,
        usage_counters: Vec<u64>,
        programs: &mut Vec<(Pubkey, Slot, u64)>,
    ) {
        let env = get_mock_program_runtime_environment();
        // Add multiple entries for program
        deployment_slots
            .iter()
            .enumerate()
            .for_each(|(i, deployment_slot)| {
                let usage_counter = *usage_counters.get(i).unwrap_or(&0);
                let stats = ProgramStatistics {
                    uses: usage_counter.into(),
                    ..Default::default()
                };
                cache.assign_program(
                    &env,
                    program,
                    *deployment_slot,
                    new_test_entry_with_usage(*deployment_slot, stats),
                );
                programs.push((program, *deployment_slot, usage_counter));
            });

        let next_slot = deployment_slots.iter().max().map_or(0, |slot| slot + 1);

        // Add tombstones entries for program
        let env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        for slot in next_slot..next_slot + 10 {
            set_failed_verification_tombstone(
                cache,
                program,
                slot,
                ProgramRuntimeEnvironment::clone(&env),
            );
        }

        // Add unloaded entries for program
        for slot in next_slot + 10..next_slot + 20 {
            insert_unloaded_entry(cache, program, slot);
        }
    }

    #[test]
    fn test_random_eviction() {
        let mut programs = vec![];
        let mut cache = ProgramCache::<TestForkGraph>::new(0);

        // This test adds different kind of entries to the cache.
        // Tombstones and unloaded entries are expected to not be evicted.
        // It also adds multiple entries for three programs as it tries to create a typical cache instance.

        // Program 1
        program_deploy_test_helper(
            &mut cache,
            Pubkey::new_unique(),
            vec![0, 10, 20, 30, 40],
            vec![4, 5, 25, 35, 12],
            &mut programs,
        );

        // Program 2
        program_deploy_test_helper(
            &mut cache,
            Pubkey::new_unique(),
            vec![5, 11, 21, 24],
            vec![0, 2, 30, 45],
            &mut programs,
        );

        // Program 3
        program_deploy_test_helper(
            &mut cache,
            Pubkey::new_unique(),
            vec![0, 5, 15, 25],
            vec![100, 3, 20, 40],
            &mut programs,
        );

        // 1 for each deployment slot
        let num_loaded_expected = 13;
        // 10 for each program
        let num_unloaded_expected = 30;
        // 10 for each program
        let num_tombstones_expected = 30;

        // Count the number of loaded, unloaded and tombstone entries.
        programs.sort_by_key(|(_id, _slot, usage_count)| *usage_count);
        let num_loaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, ProgramCacheEntryType::Loaded(_))
        });
        let num_unloaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, ProgramCacheEntryType::Unloaded(_))
        });
        let num_tombstones = num_matching_entries(&cache, |program_type| {
            matches!(
                program_type,
                ProgramCacheEntryType::DelayVisibility
                    | ProgramCacheEntryType::FailedVerification(_)
                    | ProgramCacheEntryType::Closed
            )
        });

        // Test that the cache is constructed with the expected number of entries.
        assert_eq!(num_loaded, num_loaded_expected);
        assert_eq!(num_unloaded, num_unloaded_expected);
        assert_eq!(num_tombstones, num_tombstones_expected);

        // Evict entries from the cache
        let eviction_pct: Percent = 1;

        let num_loaded_expected = crate::loaded_programs::percent_of_max_entries(eviction_pct);
        let num_unloaded_expected = num_unloaded_expected + num_loaded - num_loaded_expected;
        cache.evict_using_random_selection(eviction_pct, 21);

        // Count the number of loaded, unloaded and tombstone entries.
        let num_loaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, ProgramCacheEntryType::Loaded(_))
        });
        let num_unloaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, ProgramCacheEntryType::Unloaded(_))
        });
        let num_tombstones = num_matching_entries(&cache, |program_type| {
            matches!(program_type, ProgramCacheEntryType::FailedVerification(_))
        });

        // However many entries are left after the shrink
        assert_eq!(num_loaded, num_loaded_expected);
        // The original unloaded entries + the evicted loaded entries
        assert_eq!(num_unloaded, num_unloaded_expected);
        // The original tombstones are not evicted
        assert_eq!(num_tombstones, num_tombstones_expected);
    }

    #[test]
    fn test_eviction() {
        let mut programs = vec![];
        let mut cache = ProgramCache::<TestForkGraph>::new(0);

        // Program 1
        program_deploy_test_helper(
            &mut cache,
            Pubkey::new_unique(),
            vec![0, 10, 20, 30, 40],
            vec![4, 5, 25, 35, 12],
            &mut programs,
        );

        // Program 2
        program_deploy_test_helper(
            &mut cache,
            Pubkey::new_unique(),
            vec![5, 11, 21, 24],
            vec![0, 2, 30, 45],
            &mut programs,
        );

        // Program 3
        program_deploy_test_helper(
            &mut cache,
            Pubkey::new_unique(),
            vec![0, 5, 15, 25],
            vec![100, 3, 20, 40],
            &mut programs,
        );

        // 1 for each deployment slot
        let num_loaded_expected = 13;
        // 10 for each program
        let num_unloaded_expected = 30;
        // 10 for each program
        let num_tombstones_expected = 30;

        // Count the number of loaded, unloaded and tombstone entries.
        programs.sort_by_key(|(_id, _slot, usage_count)| *usage_count);
        let num_loaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, ProgramCacheEntryType::Loaded(_))
        });
        let num_unloaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, ProgramCacheEntryType::Unloaded(_))
        });
        let num_tombstones = num_matching_entries(&cache, |program_type| {
            matches!(program_type, ProgramCacheEntryType::FailedVerification(_))
        });

        // Test that the cache is constructed with the expected number of entries.
        assert_eq!(num_loaded, num_loaded_expected);
        assert_eq!(num_unloaded, num_unloaded_expected);
        assert_eq!(num_tombstones, num_tombstones_expected);

        // Evict entries from the cache
        let eviction_pct: Percent = 1;

        let num_loaded_expected = crate::loaded_programs::percent_of_max_entries(eviction_pct);
        let num_unloaded_expected = num_unloaded_expected + num_loaded - num_loaded_expected;

        cache.sort_and_unload(eviction_pct);

        // Check that every program is still in the cache.
        let entries = cache.get_flattened_entries_for_tests();
        programs.iter().for_each(|entry| {
            assert!(entries.iter().any(|(key, _entry)| key == &entry.0));
        });

        let unloaded = entries
            .iter()
            .filter_map(|(key, program)| {
                matches!(program.program, ProgramCacheEntryType::Unloaded(_))
                    .then_some((*key, program.stats.uses.load(Ordering::Relaxed)))
            })
            .collect::<Vec<(Pubkey, u64)>>();

        for index in 0..3 {
            let expected = programs.get(index).expect("Missing program");
            assert!(unloaded.contains(&(expected.0, expected.2)));
        }

        // Count the number of loaded, unloaded and tombstone entries.
        let num_loaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, ProgramCacheEntryType::Loaded(_))
        });
        let num_unloaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, ProgramCacheEntryType::Unloaded(_))
        });
        let num_tombstones = num_matching_entries(&cache, |program_type| {
            matches!(
                program_type,
                ProgramCacheEntryType::DelayVisibility
                    | ProgramCacheEntryType::FailedVerification(_)
                    | ProgramCacheEntryType::Closed
            )
        });

        // However many entries are left after the shrink
        assert_eq!(num_loaded, num_loaded_expected);
        // The original unloaded entries + the evicted loaded entries
        assert_eq!(num_unloaded, num_unloaded_expected);
        // The original tombstones are not evicted
        assert_eq!(num_tombstones, num_tombstones_expected);
    }

    #[test]
    fn test_usage_count_of_unloaded_program() {
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let env = get_mock_program_runtime_environment();

        let program = Pubkey::new_unique();
        let evict_to_pct: Percent = 2;
        let cache_capacity_after_shrink =
            crate::loaded_programs::percent_of_max_entries(evict_to_pct);
        // Add enough programs to the cache to trigger 1 eviction after shrinking.
        let num_total_programs = (cache_capacity_after_shrink + 1) as u64;
        (0..num_total_programs).for_each(|i| {
            let stats = ProgramStatistics {
                uses: (i + 10).into(),
                ..Default::default()
            };
            let entry = new_test_entry_with_usage(i, stats);
            cache.assign_program(&env, program, i, entry);
        });

        cache.sort_and_unload(evict_to_pct);

        let num_unloaded = num_matching_entries(&cache, |program_type| {
            matches!(program_type, ProgramCacheEntryType::Unloaded(_))
        });
        assert_eq!(num_unloaded, 1);

        cache
            .get_flattened_entries_for_tests()
            .iter()
            .for_each(|(_key, program)| {
                if matches!(program.program, ProgramCacheEntryType::Unloaded(_)) {
                    // Test that the usage counter is retained for the unloaded program
                    assert_eq!(program.stats.uses.load(Ordering::Relaxed), 10);
                    assert_eq!(program.deployment_slot, 0);
                    assert_eq!(program.effective_slot(), 1);
                }
            });

        // Replenish the program that was just unloaded. Use 0 as the usage counter. This should be
        // updated with the usage counter from the unloaded program.
        cache.assign_program(
            &env,
            program,
            0,
            new_test_entry_with_usage(0, ProgramStatistics::default()),
        );

        cache
            .get_flattened_entries_for_tests()
            .iter()
            .for_each(|(_key, program)| {
                if matches!(program.program, ProgramCacheEntryType::Unloaded(_))
                    && program.deployment_slot == 0
                    && program.effective_slot() == 1
                {
                    // Test that the usage counter was correctly updated.
                    assert_eq!(program.stats.uses.load(Ordering::Relaxed), 10);
                }
            });
    }

    #[test_matrix(
        (
            ProgramCacheEntryType::FailedVerification(get_mock_program_runtime_environment()),
            ProgramCacheEntryType::Closed,
            ProgramCacheEntryType::Unloaded(get_mock_program_runtime_environment()),
            new_loaded_entry(get_mock_program_runtime_environment()),
            ProgramCacheEntryType::Builtin(BuiltinProgram::new_mock()),
        ),
        (false, true)
    )]
    fn test_assign_program_no_second_level(
        program: ProgramCacheEntryType,
        empty_second_level: bool,
    ) {
        // Here we test the scenario where no second_level entry exists for the
        // program. We expect the `second_level.binary_search_by` to return
        // `Err(0)` and we expect the single entry to land in the cache.
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();

        if empty_second_level {
            // Make the entry already exist, but with an empty second level.
            match &mut cache.index {
                IndexImplementation::V1 { entries, .. } => {
                    entries.insert(program_id, Vec::new());
                }
            }
        }

        let entry = Arc::new(ProgramCacheEntry {
            program,
            account_owner: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 10,
            stats: Arc::default(),
            latest_access_slot: AtomicU64::default(),
        });

        cache.assign_program(&env, program_id, 10, Arc::clone(&entry));

        // We should have just the one single entry we just inserted.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 1);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &entry));

        // Stats should be incremented by 1 to exactly 1.
        assert_eq!(cache.stats.insertions.load(Ordering::Relaxed), 1);
    }

    #[test_matrix(
        (
            new_closed_entry,
            new_builtin_entry,
            new_failed_verification_entry,
            new_unloaded_entry,
            new_loaded_entry,
        ),
        ((50, 0), (150, 1), (250, 2), (350, 3))
    )]
    fn test_assign_program_new_insertion_deployment_slot(
        new_program: fn(ProgramRuntimeEnvironment) -> ProgramCacheEntryType,
        case: (Slot, usize),
    ) {
        let (deployment_slot, expected_index) = case;
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();

        // Entries at distinct deployment slots always coexist.
        for slot in [100, 200, 300] {
            cache.assign_program(
                &env,
                program_id,
                slot,
                new_test_entry_with_owner(
                    slot,
                    ProgramCacheEntryOwner::LoaderV3,
                    new_program(env.clone()),
                ),
            );
        }

        // Only the deployment slot differs, so it alone decides the index.
        let entry = new_test_entry_with_owner(
            deployment_slot,
            ProgramCacheEntryOwner::LoaderV3,
            new_program(env.clone()),
        );
        cache.assign_program(&env, program_id, deployment_slot, Arc::clone(&entry));

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 4);
        assert!(Arc::ptr_eq(
            slot_versions.get(expected_index).unwrap(),
            &entry
        ));
        assert_eq!(cache.stats.insertions.load(Ordering::Relaxed), 4);
    }

    #[test_matrix(
        (new_failed_verification_entry, new_unloaded_entry, new_loaded_entry),
        (
            (ProgramCacheEntryOwner::NativeLoader, 0),
            (ProgramCacheEntryOwner::LoaderV2, 1),
            (ProgramCacheEntryOwner::LoaderV4, 2),
        )
    )]
    fn test_assign_program_new_insertion_account_owner(
        new_program: fn(ProgramRuntimeEnvironment) -> ProgramCacheEntryType,
        case: (ProgramCacheEntryOwner, usize),
    ) {
        let (account_owner, expected_index) = case;
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();

        // Entries at the same deployment slot only coexist when their
        // environments differ, so give each one its own.
        for owner in [
            ProgramCacheEntryOwner::LoaderV1,
            ProgramCacheEntryOwner::LoaderV3,
        ] {
            cache.assign_program(
                &env,
                program_id,
                100,
                new_test_entry_with_owner(
                    100,
                    owner,
                    new_program(ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock())),
                ),
            );
        }

        // None of the environments are the current one, so the account owner
        // alone decides the index.
        let entry = new_test_entry_with_owner(
            100,
            account_owner,
            new_program(ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock())),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&entry));

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 3);
        assert!(Arc::ptr_eq(
            slot_versions.get(expected_index).unwrap(),
            &entry
        ));
        assert_eq!(cache.stats.insertions.load(Ordering::Relaxed), 3);
    }

    #[test_matrix(
        (new_failed_verification_entry, new_unloaded_entry, new_loaded_entry),
        (false, true)
    )]
    fn test_assign_program_new_insertion_environment(
        new_program: fn(ProgramRuntimeEnvironment) -> ProgramCacheEntryType,
        entry_uses_current_env: bool,
    ) {
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let env = get_mock_program_runtime_environment();
        let other_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let program_id = Pubkey::new_unique();

        // Deployment slot and account owner are equal, so entries for the
        // current environment sort after those which are not.
        let (existing_env, entry_env, expected_index) = if entry_uses_current_env {
            (other_env, env.clone(), 1)
        } else {
            (env.clone(), other_env, 0)
        };
        cache.assign_program(
            &env,
            program_id,
            100,
            new_test_entry_with_owner(
                100,
                ProgramCacheEntryOwner::LoaderV3,
                new_program(existing_env),
            ),
        );

        let entry = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_program(entry_env),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&entry));

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 2);
        assert!(Arc::ptr_eq(
            slot_versions.get(expected_index).unwrap(),
            &entry
        ));
        assert_eq!(cache.stats.insertions.load(Ordering::Relaxed), 2);
    }

    #[test]
    #[should_panic(expected = "Unexpected assignment of a DelayVisibility tombstone")]
    fn test_assign_program_delay_visibility_tombstone_panics() {
        // A tombstone minted by `extract` only ever lives in the batch cache.
        // Assigning one into the global cache is a caller error.
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let env = get_mock_program_runtime_environment();
        cache.assign_program(
            &env,
            Pubkey::new_unique(),
            100,
            Arc::new(ProgramCacheEntry::new_delay_visibility_tombstone(
                100,
                ProgramCacheEntryOwner::LoaderV3,
                Arc::default(),
            )),
        );
    }

    #[test]
    fn test_fuzz_assign_program_order() {
        use rand::prelude::SliceRandom;
        const EXPECTED_ENTRIES: [(u64, bool); 5] =
            [(1, true), (3, false), (5, true), (9, true), (10, false)];
        let mut rng = rand::rng();
        let program_id = Pubkey::new_unique();
        let env = get_mock_program_runtime_environment();
        for _ in 0..1000 {
            let mut entries = EXPECTED_ENTRIES.to_vec();
            entries.shuffle(&mut rng);
            let mut cache = ProgramCache::<TestForkGraph>::new(0);
            for (deployment_slot, delay_visibility) in entries {
                let entry = Arc::new(if delay_visibility {
                    ProgramCacheEntry {
                        program: new_loaded_entry(ProgramRuntimeEnvironment::from(
                            BuiltinProgram::new_mock(),
                        )), // Assign them different environments
                        account_owner: ProgramCacheEntryOwner::LoaderV2,
                        deployment_slot,
                        stats: Arc::default(),
                        latest_access_slot: AtomicU64::new(deployment_slot),
                    }
                } else {
                    ProgramCacheEntry::new_failed_verification_tombstone(
                        deployment_slot,
                        ProgramCacheEntryOwner::LoaderV2,
                        ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock()), // Assign them different environments
                    )
                });
                assert!(!cache.assign_program(&env, program_id, deployment_slot, entry));
            }
            for ((deployment_slot, delay_visibility), entry) in EXPECTED_ENTRIES
                .iter()
                .zip(cache.get_slot_versions_for_tests(&program_id).iter())
            {
                assert_eq!(entry.deployment_slot, *deployment_slot);
                assert_eq!(
                    entry.effective_slot(),
                    deployment_slot.saturating_add(*delay_visibility as u64)
                );
            }
        }
    }

    #[test_matrix(
        (
            ProgramCacheEntryType::FailedVerification(get_mock_program_runtime_environment()),
            new_loaded_entry(get_mock_program_runtime_environment()),
        ),
        (
            ProgramCacheEntryType::FailedVerification(get_mock_program_runtime_environment()),
            ProgramCacheEntryType::Closed,
            ProgramCacheEntryType::Unloaded(get_mock_program_runtime_environment()),
            new_loaded_entry(get_mock_program_runtime_environment()),
            ProgramCacheEntryType::Builtin(BuiltinProgram::new_mock()),
        )
    )]
    #[test_matrix(
        ProgramCacheEntryType::Closed,
        (
            ProgramCacheEntryType::FailedVerification(get_mock_program_runtime_environment()),
            ProgramCacheEntryType::Closed,
            new_loaded_entry(get_mock_program_runtime_environment()),
            ProgramCacheEntryType::Builtin(BuiltinProgram::new_mock()),
        )
    )]
    #[test_matrix(
        ProgramCacheEntryType::Unloaded(get_mock_program_runtime_environment()),
        (
            ProgramCacheEntryType::Closed,
            ProgramCacheEntryType::Unloaded(get_mock_program_runtime_environment()),
            ProgramCacheEntryType::Builtin(BuiltinProgram::new_mock()),
        )
    )]
    #[test_matrix(
        (ProgramCacheEntryType::Builtin(BuiltinProgram::new_mock()),),
        (
            ProgramCacheEntryType::FailedVerification(get_mock_program_runtime_environment()),
            ProgramCacheEntryType::Closed,
            ProgramCacheEntryType::Unloaded(get_mock_program_runtime_environment()),
            new_loaded_entry(get_mock_program_runtime_environment()),
        )
    )]
    #[should_panic(expected = "Unexpected replacement of an entry")]
    fn test_assign_program_failure(old: ProgramCacheEntryType, new: ProgramCacheEntryType) {
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        assert!(!cache.assign_program(
            &env,
            program_id,
            10,
            Arc::new(ProgramCacheEntry {
                program: old,
                account_owner: ProgramCacheEntryOwner::LoaderV2,
                deployment_slot: 10,
                stats: Arc::default(),
                latest_access_slot: AtomicU64::default(),
            }),
        ));
        cache.assign_program(
            &env,
            program_id,
            10,
            Arc::new(ProgramCacheEntry {
                program: new,
                account_owner: ProgramCacheEntryOwner::LoaderV2,
                deployment_slot: 10,
                stats: Arc::default(),
                latest_access_slot: AtomicU64::default(),
            }),
        );
    }

    #[test_matrix(
        ProgramCacheEntryType::Unloaded(get_mock_program_runtime_environment()),
        (
            new_loaded_entry(get_mock_program_runtime_environment()),
            ProgramCacheEntryType::FailedVerification(get_mock_program_runtime_environment()),
        )
    )]
    #[test_case(
        ProgramCacheEntryType::Closed,
        ProgramCacheEntryType::Unloaded(get_mock_program_runtime_environment())
    )]
    #[test_case(
        ProgramCacheEntryType::Builtin(BuiltinProgram::new_mock()),
        ProgramCacheEntryType::Builtin(BuiltinProgram::new_mock())
    )]
    fn test_assign_program_success(old: ProgramCacheEntryType, new: ProgramCacheEntryType) {
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        assert!(!cache.assign_program(
            &env,
            program_id,
            10,
            Arc::new(ProgramCacheEntry {
                program: old,
                account_owner: ProgramCacheEntryOwner::LoaderV2,
                deployment_slot: 10,
                stats: Arc::default(),
                latest_access_slot: AtomicU64::default(),
            }),
        ));
        assert!(!cache.assign_program(
            &env,
            program_id,
            10,
            Arc::new(ProgramCacheEntry {
                program: new,
                account_owner: ProgramCacheEntryOwner::LoaderV2,
                deployment_slot: 10,
                stats: Arc::default(),
                latest_access_slot: AtomicU64::default(),
            }),
        ));
    }

    #[test]
    fn test_assign_program_removes_entries_in_same_slot() {
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let closed_other_slot = Arc::new(ProgramCacheEntry {
            program: ProgramCacheEntryType::Closed,
            account_owner: ProgramCacheEntryOwner::LoaderV2,
            deployment_slot: 9,
            stats: Arc::default(),
            latest_access_slot: AtomicU64::default(),
        });
        let closed_current_slot = Arc::new(ProgramCacheEntry {
            program: ProgramCacheEntryType::Closed,
            account_owner: ProgramCacheEntryOwner::LoaderV2,
            deployment_slot: 10,
            stats: Arc::default(),
            latest_access_slot: AtomicU64::default(),
        });
        let loaded_entry_current_env = Arc::new(ProgramCacheEntry {
            program: ProgramCacheEntryType::Unloaded(get_mock_program_runtime_environment()),
            account_owner: ProgramCacheEntryOwner::LoaderV2,
            deployment_slot: 10,
            stats: Arc::default(),
            latest_access_slot: AtomicU64::default(),
        });
        let loaded_entry_upcoming_env = Arc::new(ProgramCacheEntry {
            program: ProgramCacheEntryType::Unloaded(ProgramRuntimeEnvironment::from(
                BuiltinProgram::new_mock(),
            )),
            account_owner: ProgramCacheEntryOwner::LoaderV2,
            deployment_slot: 10,
            stats: Arc::default(),
            latest_access_slot: AtomicU64::default(),
        });
        assert!(!cache.assign_program(&env, program_id, 9, closed_other_slot.clone()));
        assert!(!cache.assign_program(&env, program_id, 10, closed_current_slot));
        assert!(!cache.assign_program(&env, program_id, 10, loaded_entry_upcoming_env.clone()));
        assert!(!cache.assign_program(&env, program_id, 10, loaded_entry_current_env.clone()));
        // Only the conflicting entry in the same slot which does not have a different environment is removed
        assert_eq!(
            cache.get_slot_versions_for_tests(&program_id),
            &[
                closed_other_slot,
                loaded_entry_current_env,
                loaded_entry_upcoming_env
            ]
        );
    }

    #[test]
    fn test_assign_program_reload_merges_statistics() {
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();

        let stats = Arc::new(ProgramStatistics {
            uses: 1.into(),
            compilations: 2.into(),
            total_compilation_time_us: 3.into(),
            compilation_time_ema: 100.into(),
            jit_invocations: 4.into(),
            total_jit_execution_time_us: 5.into(),
            jit_execution_time_ema: 200.into(),
            interpreted_invocations: 6.into(),
            total_interpretation_time_us: 7.into(),
            interpretation_time_ema: 300.into(),
        });
        let unloaded = Arc::new(ProgramCacheEntry {
            program: ProgramCacheEntryType::Unloaded(env.clone()),
            account_owner: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
            stats: Arc::clone(&stats),
            latest_access_slot: AtomicU64::default(),
        });
        cache.assign_program(&env, program_id, 100, unloaded);

        // `Unloaded` -> `Loaded` matches the existing entry, so it is a reload.
        let loaded = Arc::new(ProgramCacheEntry {
            program: new_loaded_entry(env.clone()),
            account_owner: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
            stats: Arc::default(), // <-- Empty stats
            latest_access_slot: AtomicU64::default(),
        });
        cache.assign_program(&env, program_id, 100, Arc::clone(&loaded));

        assert_eq!(cache.stats.insertions.load(Ordering::Relaxed), 1);
        assert_eq!(cache.stats.reloads.load(Ordering::Relaxed), 1);

        let merged = &loaded.stats;
        let ord = Ordering::Relaxed;
        assert_eq!(merged.uses.load(ord), stats.uses.load(ord));
        assert_eq!(merged.compilations.load(ord), stats.compilations.load(ord));
        assert_eq!(
            merged.total_compilation_time_us.load(ord),
            stats.total_compilation_time_us.load(ord)
        );
        assert_eq!(
            merged.jit_invocations.load(ord),
            stats.jit_invocations.load(ord)
        );
        assert_eq!(
            merged.total_jit_execution_time_us.load(ord),
            stats.total_jit_execution_time_us.load(ord)
        );
        assert_eq!(
            merged.interpreted_invocations.load(ord),
            stats.interpreted_invocations.load(ord)
        );
        assert_eq!(
            merged.total_interpretation_time_us.load(ord),
            stats.total_interpretation_time_us.load(ord)
        );

        // The moving averages are weighted against the empty ones of the new
        // entry, which halves them.
        const EMA_DIVISOR: u64 = 2;
        assert_eq!(
            merged.compilation_time_ema.load(ord),
            stats
                .compilation_time_ema
                .load(ord)
                .wrapping_div(EMA_DIVISOR)
        );
        assert_eq!(
            merged.jit_execution_time_ema.load(ord),
            stats
                .jit_execution_time_ema
                .load(ord)
                .wrapping_div(EMA_DIVISOR)
        );
        assert_eq!(
            merged.interpretation_time_ema.load(ord),
            stats
                .interpretation_time_ema
                .load(ord)
                .wrapping_div(EMA_DIVISOR)
        );
    }

    #[test]
    fn test_tombstone() {
        let env = get_mock_program_runtime_environment();
        let tombstone = ProgramCacheEntry::new_failed_verification_tombstone(
            0,
            ProgramCacheEntryOwner::LoaderV2,
            env.clone(),
        );
        assert_matches!(
            tombstone.program,
            ProgramCacheEntryType::FailedVerification(_)
        );
        assert!(tombstone.is_tombstone());
        assert_eq!(tombstone.deployment_slot, 0);
        assert_eq!(tombstone.effective_slot(), 0);

        let tombstone =
            ProgramCacheEntry::new_closed_tombstone(100, ProgramCacheEntryOwner::LoaderV2);
        assert_matches!(tombstone.program, ProgramCacheEntryType::Closed);
        assert!(tombstone.is_tombstone());
        assert_eq!(tombstone.deployment_slot, 100);
        assert_eq!(tombstone.effective_slot(), 100);

        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let program1 = Pubkey::new_unique();
        let tombstone = set_failed_verification_tombstone(&mut cache, program1, 10, env.clone());
        let slot_versions = cache.get_slot_versions_for_tests(&program1);
        assert_eq!(slot_versions.len(), 1);
        assert!(slot_versions.first().unwrap().is_tombstone());
        assert_eq!(tombstone.deployment_slot, 10);
        assert_eq!(tombstone.effective_slot(), 10);

        // Add a program at slot 50, and a tombstone for the program at slot 60
        let program2 = Pubkey::new_unique();
        cache.assign_program(&env, program2, 50, new_test_builtin_entry(50));
        let slot_versions = cache.get_slot_versions_for_tests(&program2);
        assert_eq!(slot_versions.len(), 1);
        assert!(!slot_versions.first().unwrap().is_tombstone());

        let tombstone = set_failed_verification_tombstone(&mut cache, program2, 60, env);
        let slot_versions = cache.get_slot_versions_for_tests(&program2);
        assert_eq!(slot_versions.len(), 2);
        assert!(!slot_versions.first().unwrap().is_tombstone());
        assert!(slot_versions.get(1).unwrap().is_tombstone());
        assert!(tombstone.is_tombstone());
        assert_eq!(tombstone.deployment_slot, 60);
        assert_eq!(tombstone.effective_slot(), 60);
    }

    struct TestForkGraph {
        relation: BlockRelation,
    }
    impl ForkGraph for TestForkGraph {
        fn relationship(&self, _a: Slot, _b: Slot) -> BlockRelation {
            self.relation
        }
    }

    #[test]
    fn test_prune_empty() {
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let fork_graph = Arc::new(RwLock::new(TestForkGraph {
            relation: BlockRelation::Unrelated,
        }));

        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        cache.prune(0, None, &fork_graph.read().unwrap());
        assert!(cache.get_flattened_entries_for_tests().is_empty());

        cache.prune(10, None, &fork_graph.read().unwrap());
        assert!(cache.get_flattened_entries_for_tests().is_empty());

        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let fork_graph = Arc::new(RwLock::new(TestForkGraph {
            relation: BlockRelation::Ancestor,
        }));

        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        cache.prune(0, None, &fork_graph.read().unwrap());
        assert!(cache.get_flattened_entries_for_tests().is_empty());

        cache.prune(10, None, &fork_graph.read().unwrap());
        assert!(cache.get_flattened_entries_for_tests().is_empty());

        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let fork_graph = Arc::new(RwLock::new(TestForkGraph {
            relation: BlockRelation::Descendant,
        }));

        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        cache.prune(0, None, &fork_graph.read().unwrap());
        assert!(cache.get_flattened_entries_for_tests().is_empty());

        cache.prune(10, None, &fork_graph.read().unwrap());
        assert!(cache.get_flattened_entries_for_tests().is_empty());

        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let fork_graph = Arc::new(RwLock::new(TestForkGraph {
            relation: BlockRelation::Unknown,
        }));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        cache.prune(0, None, &fork_graph.read().unwrap());
        assert!(cache.get_flattened_entries_for_tests().is_empty());

        cache.prune(10, None, &fork_graph.read().unwrap());
        assert!(cache.get_flattened_entries_for_tests().is_empty());
    }

    #[test]
    fn test_prune_removes_programs_with_no_entries() {
        let (mut cache, fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Unknown);
        let env = get_mock_program_runtime_environment();
        let emptied = Pubkey::new_unique();
        cache.assign_program(
            &env,
            emptied,
            100,
            new_test_entry_with_owner(
                100,
                ProgramCacheEntryOwner::LoaderV3,
                new_loaded_entry(env.clone()),
            ),
        );

        // The entry is dropped, and the key goes with it.
        cache.prune(50, None, &fork_graph.read().unwrap());
        match &cache.index {
            IndexImplementation::V1 { entries, .. } => assert!(!entries.contains_key(&emptied)),
        }
        assert_eq!(cache.stats.empty_entries.load(Ordering::Relaxed), 1);
        assert_eq!(cache.stats.prunes_orphan.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_prune_across_the_root() {
        // Fork graph created for the test
        //                30 - 50 - 70
        //
        // One program with entries on both sides of the new root.
        // Both the ancestor and the descendant are kept.
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[30, 50, 70]);
        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let below = new_test_entry_with_owner(
            30,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        let above = new_test_entry_with_owner(
            70,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 30, Arc::clone(&below));
        cache.assign_program(&env, program_id, 70, Arc::clone(&above));

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 2);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &below));
        assert!(Arc::ptr_eq(slot_versions.get(1).unwrap(), &above));

        cache.prune(50, None, &fork_graph.read().unwrap());

        // Both survive, and in the order they were in.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 2);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &below));
        assert!(Arc::ptr_eq(slot_versions.get(1).unwrap(), &above));
    }

    #[test]
    fn test_prune_across_the_root_ancestors() {
        // Fork graph created for the test
        //                10 - 20 - 30 - 50 - 70
        //
        // Same as above, with more entries deployed before the new root.
        // Only the newest of those is the first ancestor.
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[10, 20, 30, 50, 70]);
        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let oldest = new_test_entry_with_owner(
            10,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        let older = new_test_entry_with_owner(
            20,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        let below = new_test_entry_with_owner(
            30,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        let above = new_test_entry_with_owner(
            70,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 10, Arc::clone(&oldest));
        cache.assign_program(&env, program_id, 20, Arc::clone(&older));
        cache.assign_program(&env, program_id, 30, Arc::clone(&below));
        cache.assign_program(&env, program_id, 70, Arc::clone(&above));

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 4);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &oldest));
        assert!(Arc::ptr_eq(slot_versions.get(1).unwrap(), &older));
        assert!(Arc::ptr_eq(slot_versions.get(2).unwrap(), &below));
        assert!(Arc::ptr_eq(slot_versions.get(3).unwrap(), &above));

        cache.prune(50, None, &fork_graph.read().unwrap());

        // The entries at 10 and 20 have been redeployed over by the one at 30,
        // so they go, and nothing was on another environment to exempt them.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 2);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &below));
        assert!(Arc::ptr_eq(slot_versions.get(1).unwrap(), &above));
        assert_eq!(cache.stats.prunes_orphan.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_prune_entry_older_than_root() {
        // Fork graph created for the test
        //                5  ?  10  ?  20
        //                ^     ^^     ^^
        //                |     |      the new root
        //                |     the old root
        //                the entry is deployed here
        //
        // The graph answers `BlockRelation::Unknown` for every pair, so
        // nothing here is related to anything else.
        //
        // Here we want to test that an entry the graph cannot place on the
        // querying fork is kept anyway, purely because it was deployed before
        // the root. Therefore, nothing is pruned and no orphan is counted.
        let (mut cache, fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Unknown);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let entry = new_test_entry_with_owner(
            5,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 5, Arc::clone(&entry));
        cache.latest_root_slot = 10;

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 1);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &entry));

        cache.prune(20, None, &fork_graph.read().unwrap());

        // `Unknown` means the graph cannot say the entry belongs to this fork,
        // but the `deployment_slot <= latest_root_slot` fallback keeps it
        // regardless.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 1);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &entry));
        assert_eq!(cache.stats.prunes_orphan.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_prune_orphan_newer_than_root() {
        // Fork graph created for the test
        //                50  ?  100
        //                ^^     ^^^
        //                |      the entry is deployed here
        //                the new root
        //
        // Here we want to test the other side of the root from the test above:
        // an entry deployed past it, which the graph cannot place either.
        // Therefore it is pruned, where the one behind the root was kept.
        let (mut cache, fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Unknown);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let orphan = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&orphan));

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 1);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &orphan));

        cache.prune(50, None, &fork_graph.read().unwrap());

        // Past the root there is no `deployment_slot <= latest_root_slot`
        // fallback to keep it, so the entry the graph cannot place goes.
        assert!(cache.get_slot_versions_for_tests(&program_id).is_empty());
        assert_eq!(cache.stats.prunes_orphan.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_prune_tombstones() {
        let env = get_mock_program_runtime_environment();
        let fork_graph = Arc::new(RwLock::new(TestForkGraph {
            relation: BlockRelation::Ancestor,
        }));

        let program1 = Pubkey::new_unique();
        let entries = [
            Arc::new(ProgramCacheEntry::new_unloaded(
                20,
                ProgramCacheEntryOwner::LoaderV3,
                ProgramRuntimeEnvironment::clone(&env),
            )),
            Arc::new(ProgramCacheEntry::new_closed_tombstone(
                20,
                ProgramCacheEntryOwner::LoaderV3,
            )),
            Arc::new(ProgramCacheEntry::new_failed_verification_tombstone(
                20,
                ProgramCacheEntryOwner::LoaderV3,
                ProgramRuntimeEnvironment::clone(&env),
            )),
        ];
        for entry in &entries {
            let mut cache = ProgramCache::<TestForkGraph>::new(0);
            cache.set_fork_graph(Arc::downgrade(&fork_graph));
            // Test that multiple entries prevent pruning
            cache.assign_program(&env, program1, 10, new_test_entry(10));
            cache.assign_program(&env, program1, entry.deployment_slot, Arc::clone(entry));
            cache.prune(
                MAX_TOMBSTONE_AGE_IN_SLOTS,
                None,
                &fork_graph.read().unwrap(),
            );
            let slot_versions = cache.get_slot_versions_for_tests(&program1);
            assert_eq!(slot_versions, std::slice::from_ref(entry));
            // Test that latest_access_slot prevents pruning
            cache.prune(
                MAX_TOMBSTONE_AGE_IN_SLOTS
                    .saturating_add(entry.latest_access_slot.load(Ordering::Relaxed)),
                None,
                &fork_graph.read().unwrap(),
            );
            let slot_versions = cache.get_slot_versions_for_tests(&program1);
            assert_eq!(slot_versions, std::slice::from_ref(entry));
            // Test that exeeding latest_access_slot + MAX_TOMBSTONE_AGE_IN_SLOTS prunes
            cache.prune(
                MAX_TOMBSTONE_AGE_IN_SLOTS
                    .saturating_add(entry.latest_access_slot.load(Ordering::Relaxed))
                    .saturating_add(1),
                None,
                &fork_graph.read().unwrap(),
            );
            assert!(cache.get_flattened_entries_for_tests().is_empty());
        }
    }

    #[test]
    fn test_prune_tombstone_first_ancestor_takes_the_rest() {
        // Fork graph created for the test
        //                50 - 60 - 100 - 200
        //                          ^^^
        //                          the program is closed here
        //
        // Here we want to test that the pruning step correctly sees that a
        // closure - a `Closed` tombstone - is being rooted. Therefore, nothing
        // else should be retained in the cache for this entry.
        let (mut cache, fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let other_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let program_id = Pubkey::new_unique();
        let on_other_env = new_test_entry_with_owner(
            50,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(other_env.clone()),
        );
        let on_env = new_test_entry_with_owner(
            60,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        let closed = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_closed_entry(env.clone()),
        );
        cache.assign_program(&other_env, program_id, 50, Arc::clone(&on_other_env));
        cache.assign_program(&env, program_id, 60, Arc::clone(&on_env));
        cache.assign_program(&env, program_id, 100, Arc::clone(&closed));

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 3);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &on_other_env));
        assert!(Arc::ptr_eq(slot_versions.get(1).unwrap(), &on_env));
        assert!(Arc::ptr_eq(slot_versions.get(2).unwrap(), &closed));

        cache.prune(200, None, &fork_graph.read().unwrap());

        // The newest entry deployed before the root is the tombstone, so
        // `first_ancestor_env` is `None`. The env-based exemption for entries
        // behind the tombstone is unreachable, so they all get pruned.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 1);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &closed));
        assert_eq!(cache.stats.prunes_orphan.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_prune_tombstone_newer_than_root_keeps_the_rest() {
        // Fork graph created for the test
        //                50 - 60 - 100 - 101
        //                                ^^^
        //                                the program is closed here
        //
        // Here we want to test that the pruning step does not see a closure -
        // a `Closed` tombstone - being rooted, since it lands after the new
        // root. Therefore, everything else should be retained in the cache
        // for this program.
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[50, 60, 100, 101]);
        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        let env = get_mock_program_runtime_environment();
        let other_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let program_id = Pubkey::new_unique();
        let on_other_env = new_test_entry_with_owner(
            50,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(other_env.clone()),
        );
        let on_env = new_test_entry_with_owner(
            60,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        let closed = new_test_entry_with_owner(
            101,
            ProgramCacheEntryOwner::LoaderV3,
            new_closed_entry(env.clone()),
        );
        cache.assign_program(&other_env, program_id, 50, Arc::clone(&on_other_env));
        cache.assign_program(&env, program_id, 60, Arc::clone(&on_env));
        cache.assign_program(&env, program_id, 101, Arc::clone(&closed));

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 3);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &on_other_env));
        assert!(Arc::ptr_eq(slot_versions.get(1).unwrap(), &on_env));
        assert!(Arc::ptr_eq(slot_versions.get(2).unwrap(), &closed));

        cache.prune(100, None, &fork_graph.read().unwrap());

        // The tombstone is kept as a descendant of the root, and never reaches
        // the arm which sets `first_ancestor_env`. So the entry at 60 is the
        // first ancestor, the one at 50 is exempt for being on another
        // environment, and nothing is pruned.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 3);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &on_other_env));
        assert!(Arc::ptr_eq(slot_versions.get(1).unwrap(), &on_env));
        assert!(Arc::ptr_eq(slot_versions.get(2).unwrap(), &closed));
        assert_eq!(cache.stats.prunes_orphan.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_prune_with_two_environments_before_epoch_boundary() {
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let env = get_mock_program_runtime_environment();
        let new_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let fork_graph = Arc::new(RwLock::new(TestForkGraph {
            relation: BlockRelation::Ancestor,
        }));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        let program1 = Pubkey::new_unique();
        cache.assign_program(&env, program1, 10, new_test_entry(10));
        let updated_program = Arc::new(ProgramCacheEntry {
            program: new_loaded_entry(new_env.clone()),
            deployment_slot: 20,
            ..Default::default()
        });
        cache.assign_program(&env, program1, 20, updated_program.clone());

        // Test that there are 2 entries for the program
        assert_eq!(cache.get_slot_versions_for_tests(&program1).len(), 2);

        cache.prune(21, None, &fork_graph.read().unwrap());

        // Test that prune didn't remove the entry, since environments are different.
        assert_eq!(cache.get_slot_versions_for_tests(&program1).len(), 2);
    }

    #[test]
    fn test_prune_with_two_environments_after_epoch_boundary() {
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let env = get_mock_program_runtime_environment();
        let new_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let fork_graph = Arc::new(RwLock::new(TestForkGraph {
            relation: BlockRelation::Ancestor,
        }));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));
        let program1 = Pubkey::new_unique();

        let old_program_old_env = Arc::new(ProgramCacheEntry {
            program: new_loaded_entry(env.clone()),
            deployment_slot: 10,
            ..Default::default()
        });
        let old_program_new_env = Arc::new(ProgramCacheEntry {
            program: new_loaded_entry(new_env.clone()),
            deployment_slot: 10,
            ..Default::default()
        });
        let new_program_old_env = Arc::new(ProgramCacheEntry {
            program: new_loaded_entry(env.clone()),
            deployment_slot: 20,
            ..Default::default()
        });
        cache.assign_program(&env, program1, 10, old_program_old_env.clone());
        cache.assign_program(&env, program1, 10, old_program_new_env.clone());
        cache.assign_program(&env, program1, 20, new_program_old_env.clone());
        let slot_versions = cache.get_slot_versions_for_tests(&program1);
        assert_eq!(
            &slot_versions,
            &[
                old_program_new_env.clone(),
                old_program_old_env.clone(),
                new_program_old_env.clone(),
            ]
        );

        cache.prune(21, Some(new_env.clone()), &fork_graph.read().unwrap());
        let slot_versions = cache.get_slot_versions_for_tests(&program1);
        assert_eq!(&slot_versions, &[old_program_new_env]);
        assert!(matches!(
            &slot_versions.first().unwrap().program,
            ProgramCacheEntryType::Loaded(_)
        ));
    }

    #[test_matrix(
        (
            new_closed_entry,
            new_builtin_entry,
            new_failed_verification_entry,
            new_unloaded_entry,
            new_loaded_entry,
        ),
        (false, true)
    )]
    fn test_prune_environment_sweep_by_entry_type(
        new_program: fn(ProgramRuntimeEnvironment) -> ProgramCacheEntryType,
        on_new_environment: bool,
    ) {
        // Fork graph created for the test
        //                40 - 50
        //
        // The entry is deployed after the root the sweep runs at, so the fork
        // graph keeps it and the environment decides the rest.
        //
        // Here we want to test which entry types the sweep can reach.
        // Therefore only one which carries an environment, and not the
        // incoming one, is taken - and taken outright, rather than unloaded.
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[40, 50]);
        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        let env = get_mock_program_runtime_environment();
        let new_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let entry_env = if on_new_environment {
            new_env.clone()
        } else {
            env.clone()
        };
        let program_id = Pubkey::new_unique();
        let entry = new_test_entry_with_owner(
            50,
            ProgramCacheEntryOwner::LoaderV3,
            new_program(entry_env.clone()),
        );
        let carries_an_environment = entry.program.get_environment().is_some();
        cache.assign_program(&entry_env, program_id, 50, Arc::clone(&entry));

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 1);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &entry));

        cache.prune(40, Some(new_env.clone()), &fork_graph.read().unwrap());

        // Only an entry which carries an environment, and one which is not
        // the incoming one, is swept - and it is removed, not unloaded.
        let swept = carries_an_environment && !on_new_environment;
        assert_eq!(
            cache.stats.prunes_environment.load(Ordering::Relaxed),
            u64::from(swept)
        );
        if swept {
            assert!(cache.get_slot_versions_for_tests(&program_id).is_empty());
        } else {
            let slot_versions = cache.get_slot_versions_for_tests(&program_id);
            assert_eq!(slot_versions.len(), 1);
            assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &entry));
        }
    }

    #[test]
    fn test_prune_environment_sweep_keeps_tombstones() {
        // Fork graph created for the test
        //                0 - 40 - 50 - 60 - 70 - 100
        //
        // Every entry is deployed after the root the sweep runs at, so `prune`
        // keeps all of them on the fork graph alone and the environment is the
        // only thing which takes any of them out.
        //
        // Here we want to test that the sweep can only take an entry which
        // carries an environment. Therefore the two built for the outgoing one
        // go, and the tombstone survives because it has none to compare
        // against.
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 40, 50, 60, 70, 100]);
        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));
        let env = get_mock_program_runtime_environment();
        let new_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let program_id = Pubkey::new_unique();
        let closed = new_test_entry_with_owner(
            50,
            ProgramCacheEntryOwner::LoaderV3,
            new_closed_entry(env.clone()),
        );
        let failed_verification = new_test_entry_with_owner(
            60,
            ProgramCacheEntryOwner::LoaderV3,
            // `FailedVerification` carries an environment, so it gets pruned.
            new_failed_verification_entry(env.clone()),
        );
        let loaded = new_test_entry_with_owner(
            70,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 50, Arc::clone(&closed));
        cache.assign_program(&env, program_id, 60, Arc::clone(&failed_verification));
        cache.assign_program(&env, program_id, 70, Arc::clone(&loaded));

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 3);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &closed));
        assert!(Arc::ptr_eq(
            slot_versions.get(1).unwrap(),
            &failed_verification
        ));
        assert!(Arc::ptr_eq(slot_versions.get(2).unwrap(), &loaded));

        // The epoch boundary. Both entries which carry an environment are on
        // the outgoing one and are swept away.
        cache.prune(40, Some(new_env.clone()), &fork_graph.read().unwrap());
        assert_eq!(cache.stats.prunes_environment.load(Ordering::Relaxed), 2);
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 1);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &closed));

        // The `Closed` tombstone survives because it carries no environment.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 50,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(100);
        cache.extract(&mut search_for, &mut extracted, &new_env, true, true);
        assert!(search_for.is_empty());
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &closed
        ));

        // Try the same search again with the outgoing environment. That would
        // not happen in production, since the sweep has just rooted the new
        // one, but exercise it anyway.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 50,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(100);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(search_for.is_empty());
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &closed
        ));
    }

    #[test]
    fn test_prune_dedup_reaches_every_environment() {
        // Six deployments before the root, cycling through three
        // environments.
        let (mut cache, fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let other_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let third_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let program_id = Pubkey::new_unique();
        let mut deploy = |slot, entry_env: &ProgramRuntimeEnvironment| {
            let entry = new_test_entry_with_owner(
                slot,
                ProgramCacheEntryOwner::LoaderV3,
                new_loaded_entry(entry_env.clone()),
            );
            cache.assign_program(entry_env, program_id, slot, Arc::clone(&entry));
            entry
        };
        let _at_10 = deploy(10, &third_env);
        let _at_20 = deploy(20, &other_env);
        let _at_30 = deploy(30, &env);
        let at_40 = deploy(40, &third_env);
        let at_50 = deploy(50, &other_env);
        let at_60 = deploy(60, &env);

        cache.prune(70, None, &fork_graph.read().unwrap());

        // The walk remembers every environment it has kept an entry on, so
        // the newest on each survives and everything below it is dropped as
        // superseded. The entries at 10, 20 and 30 all go.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 3);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &at_40));
        assert!(Arc::ptr_eq(slot_versions.get(1).unwrap(), &at_50));
        assert!(Arc::ptr_eq(slot_versions.get(2).unwrap(), &at_60));
        assert_eq!(cache.stats.prunes_orphan.load(Ordering::Relaxed), 3);
    }

    #[test]
    #[should_panic(expected = "self.latest_root_slot <= new_root_slot")]
    fn test_prune_backwards_panics() {
        let (mut cache, fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        cache.latest_root_slot = 100;

        // The `debug_assert!` guarding this runs after the pruning logic,
        // not before it.
        cache.prune(50, None, &fork_graph.read().unwrap());
    }

    #[derive(Default)]
    struct TestForkGraphSpecific {
        forks: Vec<Vec<Slot>>,
    }

    impl TestForkGraphSpecific {
        fn insert_fork(&mut self, fork: &[Slot]) {
            let mut fork = fork.to_vec();
            fork.sort();
            self.forks.push(fork)
        }
    }

    impl ForkGraph for TestForkGraphSpecific {
        fn relationship(&self, a: Slot, b: Slot) -> BlockRelation {
            match self.forks.iter().try_for_each(|fork| {
                let relation = fork
                    .iter()
                    .position(|x| *x == a)
                    .and_then(|a_pos| {
                        fork.iter().position(|x| *x == b).and_then(|b_pos| {
                            (a_pos == b_pos)
                                .then_some(BlockRelation::Equal)
                                .or_else(|| (a_pos < b_pos).then_some(BlockRelation::Ancestor))
                                .or(Some(BlockRelation::Descendant))
                        })
                    })
                    .unwrap_or(BlockRelation::Unrelated);

                if relation != BlockRelation::Unrelated {
                    return ControlFlow::Break(relation);
                }

                ControlFlow::Continue(())
            }) {
                ControlFlow::Break(relation) => relation,
                _ => BlockRelation::Unrelated,
            }
        }
    }

    fn get_entries_to_load<'a>(
        cache: &ProgramCache<TestForkGraphSpecific>,
        loading_slot: Slot,
        keys: &'a [Pubkey],
    ) -> Vec<ProgramToLoad<'a>> {
        let fork_graph = cache.fork_graph.as_ref().unwrap().upgrade().unwrap();
        let locked_fork_graph = fork_graph.read().unwrap();
        let entries = cache.get_flattened_entries_for_tests();
        keys.iter()
            .filter_map(|key| {
                entries
                    .iter()
                    .rev()
                    .find(|(program_id, entry)| {
                        program_id == key
                            && matches!(
                                locked_fork_graph.relationship(entry.deployment_slot, loading_slot),
                                BlockRelation::Equal | BlockRelation::Ancestor,
                            )
                    })
                    .map(|(_program_id, entry)| ProgramToLoad {
                        program_id: key,
                        loader: entry.account_owner,
                        deployment_slot: entry.deployment_slot,
                    })
            })
            .collect()
    }

    fn match_slot(
        extracted: &ProgramCacheForTxBatch,
        program: &Pubkey,
        deployment_slot: Slot,
        working_slot: Slot,
    ) -> bool {
        assert_eq!(extracted.slot, working_slot);
        extracted
            .entries
            .get(program)
            .map(|entry| entry.deployment_slot == deployment_slot)
            .unwrap_or(false)
    }

    fn match_missing(
        missing: &[ProgramToLoad],
        program_id: &Pubkey,
        expected_result: bool,
    ) -> bool {
        missing.iter().any(|entry| entry.program_id == program_id) == expected_result
    }

    #[test]
    fn test_fork_extract_and_prune() {
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let env = get_mock_program_runtime_environment();

        // Fork graph created for the test
        //                   0
        //                 /   \
        //                10    5
        //                |     |
        //                20    11
        //                |     | \
        //                22   15  25
        //                      |   |
        //                     16  27
        //                      |
        //                     19
        //                      |
        //                     23

        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 10, 20, 22]);
        fork_graph.insert_fork(&[0, 5, 11, 15, 16, 18, 19, 21, 23]);
        fork_graph.insert_fork(&[0, 5, 11, 25, 27]);

        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        let program1 = Pubkey::new_unique();
        cache.assign_program(&env, program1, 0, new_test_entry(0));
        cache.assign_program(&env, program1, 10, new_test_entry(10));
        cache.assign_program(&env, program1, 20, new_test_entry(20));

        let program2 = Pubkey::new_unique();
        cache.assign_program(&env, program2, 5, new_test_entry(5));
        cache.assign_program(&env, program2, 11, new_test_entry(11));

        let program3 = Pubkey::new_unique();
        cache.assign_program(&env, program3, 25, new_test_entry(25));

        let program4 = Pubkey::new_unique();
        cache.assign_program(&env, program4, 0, new_test_entry(0));
        cache.assign_program(&env, program4, 5, new_test_entry(5));
        // The following is a special case, where effective slot is 3 slots in the future
        cache.assign_program(&env, program4, 15, new_test_entry(15));

        // Current fork graph
        //                   0
        //                 /   \
        //                10    5
        //                |     |
        //                20    11
        //                |     | \
        //                22   15  25
        //                      |   |
        //                     16  27
        //                      |
        //                     19
        //                      |
        //                     23

        // Testing fork 0 - 10 - 20 - 22 with current slot at 22
        let keys = &[program1, program2, program3, program4];
        let mut missing = get_entries_to_load(&cache, 22, keys);
        assert!(match_missing(&missing, &program2, false));
        assert!(match_missing(&missing, &program3, false));
        let mut extracted = ProgramCacheForTxBatch::new(22);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 20, 22));
        assert!(match_slot(&extracted, &program4, 0, 22));

        // Testing fork 0 - 5 - 11 - 15 - 16 with current slot at 15
        let mut missing = get_entries_to_load(&cache, 15, keys);
        assert!(match_missing(&missing, &program3, false));
        let mut extracted = ProgramCacheForTxBatch::new(15);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 15));
        assert!(match_slot(&extracted, &program2, 11, 15));
        // The effective slot of program4 deployed in slot 15 is 19. So it should not be usable in slot 16.
        // A delay visibility tombstone should be returned here.
        let tombstone = extracted
            .find(&program4)
            .expect("Failed to find the tombstone");
        assert_matches!(tombstone.program, ProgramCacheEntryType::DelayVisibility);
        assert_eq!(tombstone.deployment_slot, 15);

        // Testing the same fork above, but current slot is now 18 (equal to effective slot of program4).
        let mut missing = get_entries_to_load(&cache, 18, keys);
        assert!(match_missing(&missing, &program3, false));
        let mut extracted = ProgramCacheForTxBatch::new(18);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 18));
        assert!(match_slot(&extracted, &program2, 11, 18));
        // The effective slot of program4 deployed in slot 15 is 18. So it should be usable in slot 18.
        assert!(match_slot(&extracted, &program4, 15, 18));

        // Testing the same fork above, but current slot is now 23 (future slot than effective slot of program4).
        let mut missing = get_entries_to_load(&cache, 23, keys);
        assert!(match_missing(&missing, &program3, false));
        let mut extracted = ProgramCacheForTxBatch::new(23);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 23));
        assert!(match_slot(&extracted, &program2, 11, 23));
        // The effective slot of program4 deployed in slot 15 is 19. So it should be usable in slot 23.
        assert!(match_slot(&extracted, &program4, 15, 23));

        // Testing fork 0 - 5 - 11 - 15 - 16 with current slot at 11
        let mut missing = get_entries_to_load(&cache, 11, keys);
        assert!(match_missing(&missing, &program3, false));
        let mut extracted = ProgramCacheForTxBatch::new(11);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 11));
        // program2 was updated at slot 11, but is not effective till slot 12. The result should contain a tombstone.
        let tombstone = extracted
            .find(&program2)
            .expect("Failed to find the tombstone");
        assert_matches!(tombstone.program, ProgramCacheEntryType::DelayVisibility);
        assert_eq!(tombstone.deployment_slot, 11);
        assert!(match_slot(&extracted, &program4, 5, 11));

        cache.prune(5, None, &fork_graph.read().unwrap());

        // Fork graph after pruning
        //                   0
        //                   |
        //                   5
        //                   |
        //                   11
        //                   | \
        //                  15  25
        //                   |   |
        //                  16  27
        //                   |
        //                  19
        //                   |
        //                  23

        // Testing fork 11 - 15 - 16- 19 - 22 with root at 5 and current slot at 22
        let mut missing = get_entries_to_load(&cache, 21, keys);
        assert!(match_missing(&missing, &program3, false));
        let mut extracted = ProgramCacheForTxBatch::new(21);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        // Since the fork was pruned, we should not find the entry deployed at slot 20.
        assert!(match_slot(&extracted, &program1, 0, 21));
        assert!(match_slot(&extracted, &program2, 11, 21));
        assert!(match_slot(&extracted, &program4, 15, 21));

        // Testing fork 0 - 5 - 11 - 25 - 27 with current slot at 27
        let mut missing = get_entries_to_load(&cache, 27, keys);
        let mut extracted = ProgramCacheForTxBatch::new(27);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 27));
        assert!(match_slot(&extracted, &program2, 11, 27));
        assert!(match_slot(&extracted, &program3, 25, 27));
        assert!(match_slot(&extracted, &program4, 5, 27));

        cache.prune(15, None, &fork_graph.read().unwrap());

        // Fork graph after pruning
        //                  0
        //                  |
        //                  5
        //                  |
        //                  11
        //                  |
        //                  15
        //                  |
        //                  16
        //                  |
        //                  19
        //                  |
        //                  23

        // Testing fork 16, 19, 23, with root at 15, current slot at 23
        let mut missing = get_entries_to_load(&cache, 23, keys);
        assert!(match_missing(&missing, &program3, false));
        let mut extracted = ProgramCacheForTxBatch::new(23);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 23));
        assert!(match_slot(&extracted, &program2, 11, 23));
        assert!(match_slot(&extracted, &program4, 15, 23));
    }

    #[test]
    fn test_extract_using_deployment_slot() {
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let env = get_mock_program_runtime_environment();

        // Fork graph created for the test
        //                   0
        //                 /   \
        //                10    5
        //                |     |
        //                20    11
        //                |     | \
        //                22   15  25
        //                      |   |
        //                     16  27
        //                      |
        //                     19
        //                      |
        //                     23

        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 10, 20, 22]);
        fork_graph.insert_fork(&[0, 5, 11, 12, 15, 16, 18, 19, 21, 23]);
        fork_graph.insert_fork(&[0, 5, 11, 25, 27]);

        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        let program1 = Pubkey::new_unique();
        cache.assign_program(&env, program1, 0, new_test_entry(0));
        cache.assign_program(&env, program1, 20, new_test_entry(20));

        let program2 = Pubkey::new_unique();
        cache.assign_program(&env, program2, 5, new_test_entry(5));
        cache.assign_program(&env, program2, 11, new_test_entry(11));

        let program3 = Pubkey::new_unique();
        cache.assign_program(&env, program3, 25, new_test_entry(25));

        // Testing fork 0 - 5 - 11 - 15 - 16 - 19 - 21 - 23 with current slot at 19
        let keys = &[program1, program2, program3];
        let mut missing = get_entries_to_load(&cache, 12, keys);
        assert!(match_missing(&missing, &program3, false));
        let mut extracted = ProgramCacheForTxBatch::new(12);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 12));
        assert!(match_slot(&extracted, &program2, 11, 12));

        // Now try extractions that previously worked under the "deployed on
        // or after" criteria, but won't work with exact matching.
        let mut missing = get_entries_to_load(&cache, 12, keys);
        // Program 2's newest entry is at slot 11. Asking for 5 doesn't extract
        // the latest (11) anymore. You get 5.
        missing.get_mut(1).unwrap().deployment_slot = 5;
        assert!(match_missing(&missing, &program3, false));
        let mut extracted = ProgramCacheForTxBatch::new(12);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 12));
        assert!(match_slot(&extracted, &program2, 5, 12));
    }

    #[test]
    fn test_extract_rejects_entry_deployed_after_the_requested_slot() {
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let env = get_mock_program_runtime_environment();

        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 5, 11, 12]);
        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        // The only cached entry was deployed in slot 11.
        let program = Pubkey::new_unique();
        cache.assign_program(&env, program, 11, new_test_entry(11));

        // A caller whose account state holds slot 5 must not be handed it.
        let mut missing = vec![ProgramToLoad {
            program_id: &program,
            loader: ProgramCacheEntryOwner::LoaderV2,
            deployment_slot: 5,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(12);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_missing(&missing, &program, true));
        assert!(extracted.find(&program).is_none());

        // The same caller requesting slot 11 gets it.
        let mut missing = vec![ProgramToLoad {
            program_id: &program,
            loader: ProgramCacheEntryOwner::LoaderV2,
            deployment_slot: 11,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(12);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_missing(&missing, &program, false));
        assert!(match_slot(&extracted, &program, 11, 12));
    }

    #[test]
    fn test_extract_unloaded() {
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let env = get_mock_program_runtime_environment();

        // Fork graph created for the test
        //                   0
        //                 /   \
        //                10    5
        //                |     |
        //                20    11
        //                |     | \
        //                22   15  25
        //                      |   |
        //                     16  27
        //                      |
        //                     19
        //                      |
        //                     23

        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 10, 20, 22]);
        fork_graph.insert_fork(&[0, 5, 11, 15, 16, 19, 21, 23]);
        fork_graph.insert_fork(&[0, 5, 11, 25, 27]);

        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        let program1 = Pubkey::new_unique();
        cache.assign_program(&env, program1, 0, new_test_entry(0));
        cache.assign_program(&env, program1, 20, new_test_entry(20));

        let program2 = Pubkey::new_unique();
        cache.assign_program(&env, program2, 5, new_test_entry(5));
        cache.assign_program(&env, program2, 11, new_test_entry(11));

        let program3 = Pubkey::new_unique();
        // Insert an unloaded program with correct/cache's environment at slot 25
        let _ = insert_unloaded_entry(&mut cache, program3, 25);

        // Insert another unloaded program with a different environment at slot 20
        // Since this entry's environment won't match cache's environment, looking up this
        // entry should return missing instead of unloaded entry.
        cache.assign_program(
            &env,
            program3,
            20,
            Arc::new(
                new_test_entry(20)
                    .to_unloaded()
                    .expect("Failed to create unloaded program"),
            ),
        );

        // Testing fork 0 - 5 - 11 - 15 - 16 - 19 - 21 - 23 with current slot at 19
        let keys = &[program1, program2, program3];
        let mut missing = get_entries_to_load(&cache, 19, keys);
        assert!(match_missing(&missing, &program3, false));
        let mut extracted = ProgramCacheForTxBatch::new(19);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 19));
        assert!(match_slot(&extracted, &program2, 11, 19));

        // Testing fork 0 - 5 - 11 - 25 - 27 with current slot at 27
        let mut missing = get_entries_to_load(&cache, 27, keys);
        let mut extracted = ProgramCacheForTxBatch::new(27);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 27));
        assert!(match_slot(&extracted, &program2, 11, 27));
        assert!(match_missing(&missing, &program3, true));

        // Testing fork 0 - 10 - 20 - 22 with current slot at 22
        let mut missing = get_entries_to_load(&cache, 22, keys);
        assert!(match_missing(&missing, &program2, false));
        let mut extracted = ProgramCacheForTxBatch::new(22);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 20, 22));
        assert!(match_missing(&missing, &program3, true));
    }

    #[test]
    fn test_extract_different_environment() {
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let env = get_mock_program_runtime_environment();
        let other_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());

        // Fork graph created for the test
        //                0
        //                |
        //                10
        //                |
        //                20
        //                |
        //                22

        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 10, 20, 22]);

        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        let program1 = Pubkey::new_unique();
        cache.assign_program(
            &env,
            program1,
            10,
            Arc::new(ProgramCacheEntry::new_closed_tombstone(
                10,
                ProgramCacheEntryOwner::LoaderV3,
            )),
        );
        cache.assign_program(&env, program1, 20, new_test_entry(20));

        // Testing fork 0 - 10 - 20 - 22 with current slot at 22
        let keys = &[program1];
        let mut missing = get_entries_to_load(&cache, 22, keys);
        let mut extracted = ProgramCacheForTxBatch::new(22);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 20, 22));

        // Looking for a different environment
        let mut missing = get_entries_to_load(&cache, 22, keys);
        let mut extracted = ProgramCacheForTxBatch::new(22);
        cache.extract(&mut missing, &mut extracted, &other_env, true, true);
        assert!(match_missing(&missing, &program1, true));
    }

    #[test_matrix((false, true))]
    fn test_extract_no_second_level(empty_second_level: bool) {
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        if empty_second_level {
            // Make the entry already exist, but with an empty second level.
            match &mut cache.index {
                IndexImplementation::V1 { entries, .. } => {
                    entries.insert(program_id, Vec::new());
                }
            }
        }

        // There is nothing to iterate either way, so the program is left to be
        // loaded.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 0,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(100);
        let task = cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(search_for.len(), 1);
        assert!(extracted.entries.is_empty());
        assert_eq!(task, Some(program_id));
    }

    #[test]
    fn test_extract_account_owner_mismatch() {
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let owned_by_v2 = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV2,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&owned_by_v2));

        // The only entry has an owner the search does not ask for.
        // Nothing is extracted. The caller must reload.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(search_for.len(), 1);
        assert!(extracted.entries.is_empty());

        // A loader migration, where only the newest entry has the new owner. A
        // search for the old one skips it and takes the entry below it.
        let owned_by_v3 = new_test_entry_with_owner(
            150,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 150, Arc::clone(&owned_by_v3));

        // Here the cache has the original v2 at 100 followed by the v3 at 150.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 2);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &owned_by_v2));
        assert!(Arc::ptr_eq(slot_versions.get(1).unwrap(), &owned_by_v3));

        // Try searching for the v2 version, from some fork that did not see
        // the migration. Assert the v2 entry is returned.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV2,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(search_for.is_empty());
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &owned_by_v2
        ));

        // And a fork which did see the migration finds the v3 entry, so both
        // owners are reachable from the same second level.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 150,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(search_for.is_empty());
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &owned_by_v3
        ));
    }

    #[test]
    fn test_extract_environment_mismatch() {
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let other_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let program_id = Pubkey::new_unique();
        let on_other_env = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(other_env.clone()),
        );
        cache.assign_program(&other_env, program_id, 100, Arc::clone(&on_other_env));

        // The only entry is in the same branch and effective, but it was built
        // for another environment.
        // Nothing is extracted. The caller must reload.
        // This is "reload when in doubt" in its smallest form.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(search_for.len(), 1);
        assert!(extracted.entries.is_empty());

        // The same deployment, compiled for the environment which is asked
        // for. Both are kept, since they differ in env.
        let on_execution_env = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&on_execution_env));

        // Here the cache has the one on the other environment first, since
        // entries for the current one sort last.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 2);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &on_other_env));
        assert!(Arc::ptr_eq(
            slot_versions.get(1).unwrap(),
            &on_execution_env
        ));

        // Try searching for the entry with the current env. Assert it is
        // returned.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(search_for.is_empty());
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &on_execution_env
        ));

        // And searching under the other environment returns the entry built
        // for it, so both are reachable from the same second level.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &other_env, true, true);
        assert!(search_for.is_empty());
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &on_other_env
        ));
    }

    #[test]
    fn test_extract_unloaded_entry() {
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let unloaded = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_unloaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&unloaded));

        // The only entry clears every check documented in the previous test,
        // but its executable has been evicted.
        // Nothing is extracted. The caller must reload.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(search_for.len(), 1);
        assert!(extracted.entries.is_empty());

        // Reloading it is an allowed replacement, so it takes the same place.
        let loaded = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&loaded));
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 1);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &loaded));

        // Extracting now gives the loaded entry. The unloaded is gone.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(search_for.is_empty());
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &loaded
        ));
    }

    #[test_matrix(
        (
            new_closed_entry,
            new_builtin_entry,
            new_failed_verification_entry,
            new_unloaded_entry,
            new_loaded_entry,
        ),
        (100, 101)
    )]
    fn test_extract_effective_slot(
        new_program: fn(ProgramRuntimeEnvironment) -> ProgramCacheEntryType,
        batch_slot: Slot,
    ) {
        // Fork graph created for the test
        //                100 - 101
        //                ^^^   ^^^
        //                |     `Loaded` and `Unloaded` become effective here
        //                the entry is deployed here
        //
        // Only `Loaded` and `Unloaded` have a delay window. The other three
        // are effective in the slot they were deployed in.
        //
        // Here we want to test that for all entry types, inside their
        // designated delay window, we get a tombstone standing in for the
        // entry, and outside it we get the entry itself (unless it is
        // `Unloaded`).
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let entry = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_program(env.clone()),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&entry));

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 1);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &entry));

        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(batch_slot);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);

        if batch_slot < entry.effective_slot() {
            // The entry wasn't effective, so a tombstone was minted to stand
            // in for it. It is built at that moment rather than found, so what
            // it carries is copied across from the entry.
            assert!(search_for.is_empty());
            let tombstone = extracted.entries.get(&program_id).unwrap();
            assert_matches!(tombstone.program, ProgramCacheEntryType::DelayVisibility);
            assert_eq!(tombstone.account_owner, ProgramCacheEntryOwner::LoaderV3);
            assert_eq!(tombstone.deployment_slot, 100);
            assert!(Arc::ptr_eq(&tombstone.stats, &entry.stats)); // <-- Shared, not copied.
            assert_eq!(entry.stats.uses.load(Ordering::Relaxed), 1);

            // The access slot is recorded on the entry the tombstone stands in
            // for. The tombstone's own is never touched, and is thrown away
            // with the batch.
            assert_eq!(entry.latest_access_slot.load(Ordering::Relaxed), batch_slot);
            assert_eq!(tombstone.latest_access_slot.load(Ordering::Relaxed), 0);
        } else if matches!(entry.program, ProgramCacheEntryType::Unloaded(_)) {
            // The entry was effective, but there is no binary behind it, so
            // the search breaks off and the caller is left to reload. Nothing
            // is recorded against the entry.
            assert!(extracted.entries.is_empty());
            assert_eq!(search_for.len(), 1);
            assert_eq!(entry.stats.uses.load(Ordering::Relaxed), 0);
            assert_eq!(entry.latest_access_slot.load(Ordering::Relaxed), 0);
        } else {
            // The entry was effective, so it comes back itself, and the use
            // and access slot are recorded on it.
            assert!(search_for.is_empty());
            assert!(Arc::ptr_eq(
                extracted.entries.get(&program_id).unwrap(),
                &entry
            ));
            assert_eq!(entry.stats.uses.load(Ordering::Relaxed), 1);
            assert_eq!(entry.latest_access_slot.load(Ordering::Relaxed), batch_slot);
        }
    }

    #[test]
    fn test_extract_closed_entry_matches_any_env() {
        // Fork graph created for the test
        //                100
        //                ^^^
        //                the program is closed here
        //
        // Here we want to test that a closed entry carries no environment at
        // all, and that `matches_environment` reads that as a match for any of
        // them. Therefore, the entry is handed out whichever environment is
        // asked for.
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let other_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let program_id = Pubkey::new_unique();
        let closed = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_closed_entry(env.clone()), // <-- Entry is created with `env`.
        );
        assert_eq!(closed.effective_slot(), closed.deployment_slot);
        cache.assign_program(&env, program_id, 100, Arc::clone(&closed));

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 1);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &closed));

        // Ask for the entry with `env`, matching the one used to assign it.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(100);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &closed
        ));
        assert!(search_for.is_empty());

        // Now ask for it again with `other_env`. Still successful.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(100);
        cache.extract(&mut search_for, &mut extracted, &other_env, true, true);
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &closed
        ));
        assert!(search_for.is_empty());
    }

    #[test]
    fn test_extract_delay_visibility_tombstone_interleaved_environments() {
        // Fork graph created for the test
        //                100 - 101
        //                ^^^   ^^^
        //                |     both entries become effective here
        //                both entries are deployed here
        //
        // Two entries at one deployment slot, one per environment. Here we
        // attempt to extract within the delay window (at the deployment slot).
        //
        // This test demonstrates that `DelayVisibility` tombstones - like
        // `Closed` - are indiscriminant about environments. Inside `extract`,
        // the delay visibility arm does not check the environment. Thus, any
        // entry provided to `extract` will see a `DelayVisibility` tombstone
        // if the batch slot falls within the delay window.
        //
        // Such a scenario is only possible if a program is deployed, loaded,
        // recompiled for the upcoming epoch, and extracted all within the same
        // slot. Since deployments insert `Unloaded` entries, this isn't
        // reachable in production today.
        //
        // However, this test serves to document this behavior, since it causes
        // a stats bug for now and could one day become a wider footgun.
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let upcoming_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let program_id = Pubkey::new_unique();
        let on_execution_env = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        let on_upcoming_env = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(upcoming_env.clone()),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&on_execution_env));
        cache.assign_program(&upcoming_env, program_id, 100, Arc::clone(&on_upcoming_env));

        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 2);
        assert!(Arc::ptr_eq(
            slot_versions.first().unwrap(),
            &on_execution_env
        ));
        assert!(Arc::ptr_eq(slot_versions.get(1).unwrap(), &on_upcoming_env));

        // Try the extraction, with the batch slot within the delay window.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(100);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(search_for.is_empty());

        // As expected, we get a `DelayVisibility` tombstone.
        let tombstone = extracted.entries.get(&program_id).unwrap();
        assert_matches!(tombstone.program, ProgramCacheEntryType::DelayVisibility);

        // TODO: Here's the stats bug, though. The entry the batch is actually
        // using (passed to `extract`) is present and reached second. The first
        // one reached is `!is_current_env`. Extraction traverses the second
        // level in reverse.
        //
        // So, in a case like this, we're actually updating the stats on the
        // wrong underlying `Loaded` entry.
        assert!(Arc::ptr_eq(&tombstone.stats, &on_upcoming_env.stats));
        assert_eq!(on_upcoming_env.stats.uses.load(Ordering::Relaxed), 1);
        assert_eq!(on_execution_env.stats.uses.load(Ordering::Relaxed), 0);

        // Now extract one slot later, when the program becomes effective. As
        // we know, here environment *does* matter.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(101);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(search_for.is_empty());
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &on_execution_env
        ));

        // Now we see one use on each, since we just pulled `on_execution_env`.
        assert_eq!(on_upcoming_env.stats.uses.load(Ordering::Relaxed), 1);
        assert_eq!(on_execution_env.stats.uses.load(Ordering::Relaxed), 1);
    }

    #[test_case(false)]
    #[test_case(true)]
    fn test_extract_environment_filter_same_slot(other_env_first: bool) {
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let other_env = ProgramRuntimeEnvironment::from(BuiltinProgram::new_mock());
        let program_id = Pubkey::new_unique();

        // Two entries at one deployment slot, one per environment.
        let on_other_env = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(other_env.clone()),
        );
        let on_execution_env = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        if other_env_first {
            cache.assign_program(&other_env, program_id, 100, Arc::clone(&on_other_env));
            cache.assign_program(&env, program_id, 100, Arc::clone(&on_execution_env));
        } else {
            cache.assign_program(&env, program_id, 100, Arc::clone(&on_execution_env));
            cache.assign_program(&other_env, program_id, 100, Arc::clone(&on_other_env));
        }

        // Each is assigned under its own environment, so whichever came
        // first sits first.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 2);
        let (first, second) = if other_env_first {
            (&on_other_env, &on_execution_env)
        } else {
            (&on_execution_env, &on_other_env)
        };
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), first));
        assert!(Arc::ptr_eq(slot_versions.get(1).unwrap(), second));

        // No matter the ordering, the one on the environment which is asked
        // for is the one returned.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &on_execution_env
        ));

        // And asking for the other environment reaches the other entry.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &other_env, true, true);
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &on_other_env
        ));
    }

    #[test_matrix((false, true))]
    fn test_extract_usage_counter(increment_usage_counter: bool) {
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let entry = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&entry));

        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(
            &mut search_for,
            &mut extracted,
            &env,
            increment_usage_counter,
            true,
        );

        // The access slot moves either way, the usage counter only when asked.
        assert_eq!(entry.latest_access_slot.load(Ordering::Relaxed), 200);
        assert_eq!(
            entry.stats.uses.load(Ordering::Relaxed),
            u64::from(increment_usage_counter)
        );
    }

    #[test]
    fn test_extract_usage_counter_delayed_visibility() {
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let entry = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&entry));

        // Extract at the deployment slot itself, which is inside the delay
        // visibility window, so a `DelayVisibility` tombstone stands in for
        // the entry.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(100);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);

        let tombstone = extracted.entries.get(&program_id).unwrap();
        assert!(matches!(
            tombstone.program,
            ProgramCacheEntryType::DelayVisibility
        ));
        assert!(!Arc::ptr_eq(tombstone, &entry));

        // The access slot lands on the entry the tombstone stands in for. The
        // tombstone's own is never touched, and is dropped with the batch.
        assert_eq!(entry.latest_access_slot.load(Ordering::Relaxed), 100);
        assert_eq!(tombstone.latest_access_slot.load(Ordering::Relaxed), 0);

        // The usage counter reaches the entry either way, through the
        // statistics the two share.
        assert!(Arc::ptr_eq(&tombstone.stats, &entry.stats));
        assert_eq!(entry.stats.uses.load(Ordering::Relaxed), 1);
    }

    #[test_matrix((false, true))]
    fn test_extract_hits_and_misses(count_hits_and_misses: bool) {
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let found = Pubkey::new_unique();
        let missing = Pubkey::new_unique();
        cache.assign_program(
            &env,
            found,
            100,
            new_test_entry_with_owner(
                100,
                ProgramCacheEntryOwner::LoaderV3,
                new_loaded_entry(env.clone()),
            ),
        );

        let mut search_for = vec![
            ProgramToLoad {
                program_id: &found,
                loader: ProgramCacheEntryOwner::LoaderV3,
                deployment_slot: 100,
            },
            ProgramToLoad {
                program_id: &missing,
                loader: ProgramCacheEntryOwner::LoaderV3,
                deployment_slot: 0,
            },
        ];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(
            &mut search_for,
            &mut extracted,
            &env,
            true,
            count_hits_and_misses,
        );

        let expected = u64::from(count_hits_and_misses);
        assert_eq!(cache.stats.hits.load(Ordering::Relaxed), expected);
        assert_eq!(cache.stats.misses.load(Ordering::Relaxed), expected);
    }

    #[test]
    fn test_extract_hits_count_only_this_call() {
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        cache.assign_program(
            &env,
            program_id,
            100,
            new_test_entry_with_owner(
                100,
                ProgramCacheEntryOwner::LoaderV3,
                new_loaded_entry(env.clone()),
            ),
        );

        // Anything already in the batch, such as the builtins it is seeded
        // with.
        let mut extracted = ProgramCacheForTxBatch::new(200);
        extracted.replenish(Pubkey::new_unique(), new_test_builtin_entry(0));

        // One entry is found, and only that one is counted. The entry seeded
        // above is still in the batch, but it was not found by this call.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(extracted.entries.len(), 2);
        assert_eq!(cache.stats.hits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_extract_cooperative_loading_task() {
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let program_ids = [Pubkey::new_unique(), Pubkey::new_unique()];

        // Both are missing, but only the first one becomes a task.
        let mut search_for = program_ids
            .iter()
            .map(|program_id| ProgramToLoad {
                program_id,
                loader: ProgramCacheEntryOwner::LoaderV3,
                deployment_slot: 0,
            })
            .collect();
        let mut extracted = ProgramCacheForTxBatch::new(100);
        let task = cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(search_for.len(), 2);
        assert_eq!(task, program_ids.first().copied());
        match &cache.index {
            IndexImplementation::V1 {
                loading_entries, ..
            } => {
                let loading_entries = loading_entries.lock().unwrap();
                assert_eq!(loading_entries.len(), 1);
                assert_eq!(
                    loading_entries.get(program_ids.first().unwrap()),
                    Some(&(100, thread::current().id()))
                );
            }
        }

        // Asking again for the one which is already loading returns nothing.
        let mut search_for = vec![ProgramToLoad {
            program_id: program_ids.first().unwrap(),
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 0,
        }];
        let task = cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(search_for.len(), 1);
        assert_eq!(task, None);

        // Submitting the finished task notifies whoever is waiting on one.
        let cookie = cache.loading_task_waiter.cookie();
        let loaded = new_test_entry_with_owner(
            50,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.finish_cooperative_loading_task(
            &env,
            100,
            *program_ids.first().unwrap(),
            Arc::clone(&loaded),
        );
        assert_ne!(cache.loading_task_waiter.wait(cookie), cookie);

        // It is no longer loading, and extracting it now finds it.
        match &cache.index {
            IndexImplementation::V1 {
                loading_entries, ..
            } => assert!(loading_entries.lock().unwrap().is_empty()),
        }
        let mut search_for = vec![ProgramToLoad {
            program_id: program_ids.first().unwrap(),
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 50,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(100);
        let task = cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(search_for.is_empty());
        assert_eq!(task, None);
        assert!(Arc::ptr_eq(
            extracted.entries.get(program_ids.first().unwrap()).unwrap(),
            &loaded
        ));
    }

    #[test]
    fn test_extract_cooperative_loading_task_ordering() {
        let (cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let program_ids = [Pubkey::new_unique(), Pubkey::new_unique()];
        let mut extracted = ProgramCacheForTxBatch::new(100);

        // The first one reached becomes the task.
        let mut search_for = program_ids
            .iter()
            .map(|program_id| ProgramToLoad {
                program_id,
                loader: ProgramCacheEntryOwner::LoaderV3,
                deployment_slot: 0,
            })
            .collect();
        let task = cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(task, program_ids.first().copied());

        // Asking again in the reverse order reaches the one which is not
        // loading yet first, so that one becomes a task of its own.
        let mut search_for = program_ids
            .iter()
            .rev()
            .map(|program_id| ProgramToLoad {
                program_id,
                loader: ProgramCacheEntryOwner::LoaderV3,
                deployment_slot: 0,
            })
            .collect();
        let task = cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(task, program_ids.get(1).copied());

        // Both are loading now, by this thread and for this slot.
        match &cache.index {
            IndexImplementation::V1 {
                loading_entries, ..
            } => {
                let loading_entries = loading_entries.lock().unwrap();
                assert_eq!(loading_entries.len(), 2);
                for program_id in &program_ids {
                    assert_eq!(
                        loading_entries.get(program_id),
                        Some(&(100, thread::current().id()))
                    );
                }
            }
        }

        // Neither of them can become a task again.
        let mut search_for = program_ids
            .iter()
            .map(|program_id| ProgramToLoad {
                program_id,
                loader: ProgramCacheEntryOwner::LoaderV3,
                deployment_slot: 0,
            })
            .collect();
        let task = cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(search_for.len(), 2);
        assert_eq!(task, None);
    }

    #[test]
    fn test_extract_entry_not_in_same_branch() {
        // Fork graph created for the test
        //                0
        //              /   \
        //            50     100
        //             |
        //            200
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 50, 200]);
        fork_graph.insert_fork(&[0, 100]);
        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let on_other_fork = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&on_other_fork));

        // The only entry was deployed on a fork the batch is not on, which is
        // still evaluated *in addition to* the exact deployment slot matching.
        //
        // Once fork-tracking is removed from `extract`, `deployment_slot` is
        // assumed to be the slot the caller's account state reports, so naming
        // 100 is what places the entry here.
        //
        // Until then, it cannot be resolved since fork tracking determines it
        // to be on another fork.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(search_for.len(), 1);
        assert!(extracted.entries.is_empty());

        // An older deployment, on the fork the batch is on.
        let on_same_fork = new_test_entry_with_owner(
            50,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 50, Arc::clone(&on_same_fork));

        // Here the cache has the one at 50 followed by the one at 100.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 2);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &on_same_fork));
        assert!(Arc::ptr_eq(slot_versions.get(1).unwrap(), &on_other_fork));

        // Asking for 50 still reaches the entry at 50, whichever fork the
        // one above it is on.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 50,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(search_for.is_empty());
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &on_same_fork
        ));
    }

    #[test]
    fn test_extract_deployment_slot_mismatch() {
        // We keep the cache's `latest_root_slot` at 0 and deploy a loaded
        // program entry for slot 100 to avoid running into the infamous
        // `entry.deployment_slot <= self.latest_root_slot` check.
        //
        // As such, the `entry_in_same_branch` conditional depends exclusively
        // on the fork graph relationship, which we set to `Ancestor` here.
        //
        // Unlike the mismatched owner test above, a mismatched deployment slot
        // is only a genuine miss when the targeted `deployment_slot`
        // is too new.
        //
        // So, we produce a scenario where `entry_in_same_branch`,
        // `entry_is_effective` and `matches_environment` all evaluate to
        // `true`, finally trapping and breaking out on
        // `entry.deployment_slot < program_to_load.deployment_slot`.
        let (mut cache, _fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        assert_eq!(cache.latest_root_slot, 0);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let deployed_at_100 = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&deployed_at_100));

        // The only entry is in the same branch, effective and on the right
        // environment, but it is older than the search demands.
        // Nothing is extracted. The caller must reload.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 150,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(search_for.len(), 1);
        assert!(extracted.entries.is_empty());

        // A redeployment at the slot the search asks for.
        let deployed_at_150 = new_test_entry_with_owner(
            150,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 150, Arc::clone(&deployed_at_150));

        // Here the cache has the original entry at 100 followed by the one at
        // 150.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 2);
        assert!(Arc::ptr_eq(
            slot_versions.first().unwrap(),
            &deployed_at_100
        ));
        assert!(Arc::ptr_eq(slot_versions.get(1).unwrap(), &deployed_at_150));

        // Which is reached first, and is not older than the search demands.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 150,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(search_for.is_empty());
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &deployed_at_150
        ));
    }

    #[test]
    fn test_extract_older_entry_on_the_callers_fork() {
        // Fork graph created for the test
        //                0
        //              /   \
        //            50     150
        //             |
        //            200
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 50, 200]);
        fork_graph.insert_fork(&[0, 150]);
        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let on_other_fork = new_test_entry_with_owner(
            150,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        let on_same_fork = new_test_entry_with_owner(
            50,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 150, Arc::clone(&on_other_fork));
        cache.assign_program(&env, program_id, 50, Arc::clone(&on_same_fork));

        // The account on this fork names 50, so the entry at 150 on the other
        // fork is not what is asked for and the one at 50 is served. There is
        // no fallback involved: the caller named the slot it wanted.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 50,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &on_same_fork
        ));

        // Similar to the case in `test_extract_entry_not_in_same_branch`,
        // because fork tracking is still evaluated *in addition to* the exact
        // deployment slot matching, this entry can't be extracted by a batch
        // in slot 200.
        //
        // Once fork-tracking is removed from `extract`, `deployment_slot` is
        // assumed to be the slot the caller's account state reports, so naming
        // 150 is what places the entry here.
        //
        // Until then, it cannot be resolved since fork tracking determines it
        // to be on another fork.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 150,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(200);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(search_for.len(), 1);
        assert!(extracted.entries.is_empty());
    }

    #[test]
    fn test_extract_below_deployment_slot() {
        let (mut cache, fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let entry = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&entry));

        // Rooting past the entry puts it in the branch without the fork graph
        // being consulted.
        cache.prune(200, None, &fork_graph.read().unwrap());
        assert_eq!(cache.latest_root_slot, 200);

        // It survives that, since the fork graph said it was an `Ancestor`.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 1);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &entry));

        // Overwrite the fork graph to use `BlockRelation::Unknown`, to show
        // only the `entry.deployment_slot <= self.latest_root_slot` check is
        // evaluated here.
        fork_graph.write().unwrap().relation = BlockRelation::Unknown;

        // The batch is below the deployment slot, so the entry is neither
        // effective nor a delay visibility tombstone.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 0,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(50);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert_eq!(search_for.len(), 1);
        assert!(extracted.entries.is_empty());
    }

    #[test]
    fn test_extract_entry_older_than_root() {
        // The same setup as above, extracted from a slot above the entry
        // rather than below it.
        let (mut cache, fork_graph) = new_test_cache_with_fork_graph(BlockRelation::Ancestor);
        let env = get_mock_program_runtime_environment();
        let program_id = Pubkey::new_unique();
        let entry = new_test_entry_with_owner(
            100,
            ProgramCacheEntryOwner::LoaderV3,
            new_loaded_entry(env.clone()),
        );
        cache.assign_program(&env, program_id, 100, Arc::clone(&entry));

        // Rooting past the entry puts it in the branch without the fork graph
        // being consulted.
        cache.prune(200, None, &fork_graph.read().unwrap());
        assert_eq!(cache.latest_root_slot, 200);

        // It survives that, since the fork graph said it was an `Ancestor`.
        let slot_versions = cache.get_slot_versions_for_tests(&program_id);
        assert_eq!(slot_versions.len(), 1);
        assert!(Arc::ptr_eq(slot_versions.first().unwrap(), &entry));

        // Overwrite the fork graph to use `BlockRelation::Unknown`, to show
        // only the `entry.deployment_slot <= self.latest_root_slot` check is
        // evaluated here.
        fork_graph.write().unwrap().relation = BlockRelation::Unknown;

        // That check alone is still enough to serve the entry, and there is
        // still no telling which fork it belongs to. What keeps it correct is
        // that only a caller whose own account names slot 100 can ask for it,
        // and such a caller has that deployment on its fork by definition.
        let mut search_for = vec![ProgramToLoad {
            program_id: &program_id,
            loader: ProgramCacheEntryOwner::LoaderV3,
            deployment_slot: 100,
        }];
        let mut extracted = ProgramCacheForTxBatch::new(300);
        cache.extract(&mut search_for, &mut extracted, &env, true, true);
        assert!(search_for.is_empty());
        assert!(Arc::ptr_eq(
            extracted.entries.get(&program_id).unwrap(),
            &entry
        ));
    }

    #[test]
    fn test_unloaded() {
        let mut cache = ProgramCache::<TestForkGraph>::new(0);
        let env = get_mock_program_runtime_environment();
        for program_cache_entry_type in [
            ProgramCacheEntryType::Closed,
            ProgramCacheEntryType::Builtin(BuiltinProgram::new_mock()),
        ] {
            let entry = Arc::new(ProgramCacheEntry {
                program: program_cache_entry_type,
                account_owner: ProgramCacheEntryOwner::LoaderV2,
                deployment_slot: 0,
                stats: Arc::default(),
                latest_access_slot: AtomicU64::default(),
            });
            assert!(entry.to_unloaded().is_none());

            // Check that unload_program_entry() does nothing for this entry
            let program_id = Pubkey::new_unique();
            cache.assign_program(&env, program_id, entry.deployment_slot, entry.clone());
            cache.unload_program_entry(program_id, &entry);
            assert_eq!(cache.get_slot_versions_for_tests(&program_id).len(), 1);
            assert!(cache.stats.evictions.is_empty());
        }

        let stats = ProgramStatistics {
            uses: 3.into(),
            ..Default::default()
        };
        let entry = new_test_entry_with_usage(1, stats);
        let unloaded_entry = entry.to_unloaded().unwrap();
        assert_eq!(unloaded_entry.deployment_slot, 1);
        assert_eq!(unloaded_entry.effective_slot(), 2);
        assert_eq!(unloaded_entry.latest_access_slot.load(Ordering::Relaxed), 1);
        assert_eq!(unloaded_entry.stats.uses.load(Ordering::Relaxed), 3);

        // Check that unload_program_entry() does its work
        let program_id = Pubkey::new_unique();
        cache.assign_program(&env, program_id, entry.deployment_slot, entry.clone());
        cache.unload_program_entry(program_id, &entry);
        assert!(cache.stats.evictions.contains_key(&program_id));
    }

    #[test]
    fn test_fork_prune_find_first_ancestor() {
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let env = get_mock_program_runtime_environment();

        // Fork graph created for the test
        //                   0
        //                 /   \
        //                10    5
        //                |
        //                20

        // Deploy program on slot 0, and slot 5.
        // Prune the fork that has slot 5. The cache should still have the program
        // deployed at slot 0.
        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 10, 20]);
        fork_graph.insert_fork(&[0, 5]);
        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        let program1 = Pubkey::new_unique();
        cache.assign_program(&env, program1, 0, new_test_entry(0));
        cache.assign_program(&env, program1, 5, new_test_entry(5));

        cache.prune(10, None, &fork_graph.read().unwrap());

        let keys = &[program1];
        let mut missing = get_entries_to_load(&cache, 20, keys);
        let mut extracted = ProgramCacheForTxBatch::new(20);
        cache.extract(&mut missing, &mut extracted, &env, true, true);

        // The cache should have the program deployed at slot 0
        assert_eq!(
            extracted
                .find(&program1)
                .expect("Did not find the program")
                .deployment_slot,
            0
        );
    }

    #[test]
    fn test_prune_by_deployment_slot() {
        let mut cache = ProgramCache::<TestForkGraphSpecific>::new(0);
        let env = get_mock_program_runtime_environment();

        // Fork graph created for the test
        //                   0
        //                 /   \
        //                10    5
        //                |
        //                20

        // Deploy program on slot 0, and slot 5.
        // Prune the fork that has slot 5. The cache should still have the program
        // deployed at slot 0.
        let mut fork_graph = TestForkGraphSpecific::default();
        fork_graph.insert_fork(&[0, 10, 20]);
        fork_graph.insert_fork(&[0, 5, 6]);
        let fork_graph = Arc::new(RwLock::new(fork_graph));
        cache.set_fork_graph(Arc::downgrade(&fork_graph));

        let program1 = Pubkey::new_unique();
        cache.assign_program(&env, program1, 0, new_test_entry(0));
        cache.assign_program(&env, program1, 5, new_test_entry(5));

        let program2 = Pubkey::new_unique();
        cache.assign_program(&env, program2, 10, new_test_entry(10));

        let keys = &[program1, program2];
        let mut missing = get_entries_to_load(&cache, 20, keys);
        let mut extracted = ProgramCacheForTxBatch::new(20);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 20));
        assert!(match_slot(&extracted, &program2, 10, 20));

        let mut missing = get_entries_to_load(&cache, 6, keys);
        assert!(match_missing(&missing, &program2, false));
        let mut extracted = ProgramCacheForTxBatch::new(6);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 5, 6));

        // Pruning slot 5 will remove program1 entry deployed at slot 5.
        // On fork chaining from slot 5, the entry deployed at slot 0 will become visible.
        cache.prune_by_deployment_slot(5);

        let mut missing = get_entries_to_load(&cache, 20, keys);
        let mut extracted = ProgramCacheForTxBatch::new(20);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 20));
        assert!(match_slot(&extracted, &program2, 10, 20));

        let mut missing = get_entries_to_load(&cache, 6, keys);
        assert!(match_missing(&missing, &program2, false));
        let mut extracted = ProgramCacheForTxBatch::new(6);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 6));

        // Pruning slot 10 will remove program2 entry deployed at slot 10.
        // As there is no other entry for program2, extract() will return it as missing.
        cache.prune_by_deployment_slot(10);

        let mut missing = get_entries_to_load(&cache, 20, keys);
        assert!(match_missing(&missing, &program2, false));
        let mut extracted = ProgramCacheForTxBatch::new(20);
        cache.extract(&mut missing, &mut extracted, &env, true, true);
        assert!(match_slot(&extracted, &program1, 0, 20));
    }
}
