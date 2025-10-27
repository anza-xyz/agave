//! Transaction processing glue code, mainly consisting of Object-safe traits
//!
//! [InstalledSchedulerPool] lends one of pooled [InstalledScheduler]s as wrapped in
//! [BankWithScheduler], which can be used by `ReplayStage` and `BankingStage` for transaction
//! execution. After use, the scheduler will be returned to the pool.
//!
//! [InstalledScheduler] can be fed with [SanitizedTransaction]s. Then, it schedules those
//! executions and commits those results into the associated _bank_.
//!
//! It's generally assumed that each [InstalledScheduler] is backed by multiple threads for
//! parallel transaction processing and there are multiple independent schedulers inside a single
//! instance of [InstalledSchedulerPool].
//!
//! Dynamic dispatch was inevitable due to the desire to piggyback on
//! [BankForks](crate::bank_forks::BankForks)'s pruning for scheduler lifecycle management as the
//! common place both for `ReplayStage` and `BankingStage` and the resultant need of invoking
//! actual implementations provided by the dependent crate (`solana-unified-scheduler-pool`, which
//! in turn depends on `solana-ledger`, which in turn depends on `solana-runtime`), avoiding a
//! cyclic dependency.
//!
//! See [InstalledScheduler] for visualized interaction.

use {
    crate::bank::Bank,
    assert_matches::assert_matches,
    log::*,
    solana_clock::Slot,
    solana_hash::Hash,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_svm_timings::ExecuteTimings,
    solana_transaction::sanitized::SanitizedTransaction,
    solana_transaction_error::{TransactionError, TransactionResult as Result},
    solana_unified_scheduler_logic::{OrderedTaskId, SchedulingMode},
    std::{
        fmt::{self, Debug},
        mem,
        ops::Deref,
        sync::{Arc, RwLock},
        thread,
    },
};
#[cfg(feature = "dev-context-only-utils")]
use {mockall::automock, qualifier_attr::qualifiers};

pub fn initialized_result_with_timings() -> ResultWithTimings {
    (Ok(()), ExecuteTimings::default())
}

pub trait InstalledSchedulerPool: Send + Sync + Debug {
    /// A very thin wrapper of [`Self::take_resumed_scheduler`] to take a scheduler from this pool
    /// for a brand-new bank.
    fn take_scheduler(&self, context: SchedulingContext) -> InstalledSchedulerBox {
        self.take_resumed_scheduler(context, initialized_result_with_timings())
    }

    fn take_resumed_scheduler(
        &self,
        context: SchedulingContext,
        result_with_timings: ResultWithTimings,
    ) -> InstalledSchedulerBox;

    /// Registers an opaque timeout listener.
    ///
    /// This method and the passed `struct` called [`TimeoutListener`] are very opaque by purpose.
    /// Specifically, it doesn't provide any way to tell which listener is semantically associated
    /// to which particular scheduler. That's because proper _unregistration_ is omitted at the
    /// timing of scheduler returning to reduce latency of the normal block-verification code-path,
    /// relying on eventual stale listener clean-up by `solScCleaner`.
    fn register_timeout_listener(&self, timeout_listener: TimeoutListener);

    fn uninstalled_from_bank_forks(self: Arc<Self>);
}

#[derive(Debug)]
pub struct SchedulerAborted;
pub type ScheduleResult = std::result::Result<(), SchedulerAborted>;

pub struct TimeoutListener {
    callback: Box<dyn FnOnce(InstalledSchedulerPoolArc) + Sync + Send>,
}

impl TimeoutListener {
    pub(crate) fn new(f: impl FnOnce(InstalledSchedulerPoolArc) + Sync + Send + 'static) -> Self {
        Self {
            callback: Box::new(f),
        }
    }

    pub fn trigger(self, pool: InstalledSchedulerPoolArc) {
        (self.callback)(pool);
    }
}

impl Debug for TimeoutListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TimeoutListener({self:p})")
    }
}

#[cfg_attr(doc, aquamarine::aquamarine)]
/// Schedules, executes, and commits transactions under encapsulated implementation
///
/// The following chart illustrates the ownership/reference interaction between inter-dependent
/// objects across crates:
///
/// ```mermaid
/// graph TD
///     Bank["Arc#lt;Bank#gt;"]
///
///     subgraph solana-runtime[<span style="font-size: 70%">solana-runtime</span>]
///         BankForks;
///         BankWithScheduler;
///         Bank;
///         LoadExecuteAndCommitTransactions([<span style="font-size: 67%">load_execute_and_commit_transactions#lpar;#rpar;</span>]);
///         SchedulingContext;
///         InstalledSchedulerPool{{InstalledSchedulerPool}};
///         InstalledScheduler{{InstalledScheduler}};
///     end
///
///     subgraph solana-unified-scheduler-pool[<span style="font-size: 70%">solana-unified-scheduler-pool</span>]
///         SchedulerPool;
///         PooledScheduler;
///         ScheduleExecution(["schedule_execution()"]);
///     end
///
///     subgraph solana-ledger[<span style="font-size: 60%">solana-ledger</span>]
///         ExecuteBatch(["execute_batch()"]);
///     end
///
///     ScheduleExecution -. calls .-> ExecuteBatch;
///     BankWithScheduler -. dyn-calls .-> ScheduleExecution;
///     ExecuteBatch -. calls .-> LoadExecuteAndCommitTransactions;
///     linkStyle 0,1,2 stroke:gray,color:gray;
///
///     BankForks -- owns --> BankWithScheduler;
///     BankForks -- owns --> InstalledSchedulerPool;
///     BankWithScheduler -- refs --> Bank;
///     BankWithScheduler -- owns --> InstalledScheduler;
///     SchedulingContext -- refs --> Bank;
///     InstalledScheduler -- owns --> SchedulingContext;
///
///     SchedulerPool -- owns --> PooledScheduler;
///     SchedulerPool -. impls .-> InstalledSchedulerPool;
///     PooledScheduler -. impls .-> InstalledScheduler;
///     PooledScheduler -- refs --> SchedulerPool;
/// ```
#[cfg_attr(feature = "dev-context-only-utils", automock)]
// suppress false clippy complaints arising from mockall-derive:
//   warning: `#[must_use]` has no effect when applied to a struct field
#[cfg_attr(feature = "dev-context-only-utils", allow(unused_attributes))]
pub trait InstalledScheduler: Send + Sync + Debug + 'static {
    fn id(&self) -> SchedulerId;
    fn context(&self) -> &SchedulingContext;

    /// Schedule transaction for execution.
    ///
    /// This non-blocking function will return immediately without waiting for actual execution.
    ///
    /// Calling this is illegal as soon as `wait_for_termination()` is called. It would result in
    /// fatal logic error.
    ///
    /// Note that the returned result indicates whether the scheduler has been aborted due to a
    /// previously-scheduled bad transaction, which terminates further block verification. So,
    /// almost always, the returned error isn't due to the merely scheduling of the current
    /// transaction itself. At this point, calling this does nothing anymore while it's still safe
    /// to do. As soon as notified, callers are expected to stop processing upcoming transactions
    /// of the same `SchedulingContext` (i.e. same block). Internally, the aborted scheduler will
    /// be disposed cleanly, not repooled, after `wait_for_termination()` is called like
    /// not-aborted schedulers.
    ///
    /// Caller can acquire the error by calling a separate function called
    /// `recover_error_after_abort()`, which requires `&mut self`, instead of `&self`. This
    /// separation and the convoluted returned value semantics explained above are intentional to
    /// optimize the fast code-path of normal transaction scheduling to be multi-threaded at the
    /// cost of far slower error code-path while giving implementors increased flexibility by
    /// having &mut.
    fn schedule_execution(
        &self,
        transaction: RuntimeTransaction<SanitizedTransaction>,
        task_id: OrderedTaskId,
    ) -> ScheduleResult;

    /// Return the error which caused the scheduler to abort.
    ///
    /// Note that this must not be called until it's observed that `schedule_execution()` has
    /// returned `Err(SchedulerAborted)`. Violating this should `panic!()`.
    ///
    /// That said, calling this multiple times is completely acceptable after the error observation
    /// from `schedule_execution()`. While it's not guaranteed, the same `.clone()`-ed errors of
    /// the first bad transaction are usually returned across invocations.
    fn recover_error_after_abort(&mut self) -> TransactionError;

    /// Wait for a scheduler to terminate after processing.
    ///
    /// This function blocks the current thread while waiting for the scheduler to complete all of
    /// the executions for the scheduled transactions and to return the finalized
    /// `ResultWithTimings`. This function still blocks for short period of time even in the case
    /// of aborted schedulers to gracefully shutdown the scheduler (like thread joining).
    ///
    /// Along with the result being returned, this function also makes the scheduler itself
    /// uninstalled from the bank by transforming the consumed self.
    ///
    /// If no transaction is scheduled, the result and timing will be `Ok(())` and
    /// `ExecuteTimings::default()` respectively.
    fn wait_for_termination(
        self: Box<Self>,
        is_dropped: bool,
    ) -> (ResultWithTimings, UninstalledSchedulerBox);

    /// Pause a scheduler after processing to update bank's recent blockhash.
    ///
    /// This function blocks the current thread like wait_for_termination(). However, the scheduler
    /// won't be consumed. This means the scheduler is responsible to retain the finalized
    /// `ResultWithTimings` internally until it's `wait_for_termination()`-ed to collect the result
    /// later.
    fn pause_for_recent_blockhash(&mut self);

    /// Unpause a block production scheduler, immediately after it's taken from the scheduler pool.
    ///
    /// This is rather a special-purposed method. Such a scheduler is initially paused due to a
    /// race condition between the poh thread and handler threads. So, it needs to be unpaused in
    /// order to start processing transactions by calling this.
    ///
    /// # Panics
    ///
    /// Panics if called on a block verification scheduler.
    fn unpause_after_taken(&self);
}

#[cfg_attr(feature = "dev-context-only-utils", automock)]
pub trait UninstalledScheduler: Send + Sync + Debug + 'static {
    fn return_to_pool(self: Box<Self>);
}

pub type InstalledSchedulerBox = Box<dyn InstalledScheduler>;
pub type UninstalledSchedulerBox = Box<dyn UninstalledScheduler>;

pub type InstalledSchedulerPoolArc = Arc<dyn InstalledSchedulerPool>;

pub type SchedulerId = u64;

/// A small context to propagate a bank and its scheduling mode to the scheduler subsystem.
///
/// Note that this isn't called `SchedulerContext` because the contexts aren't associated with
/// schedulers one by one. A scheduler will use many SchedulingContexts during its lifetime.
/// "Scheduling" part of the context name refers to an abstract slice of time to schedule and
/// execute all transactions for a given bank for block verification or production. A context is
/// expected to be used by a particular scheduler only for that duration of the time and to be
/// disposed by the scheduler. Then, the scheduler may work on different banks with new
/// `SchedulingContext`s.
///
/// There's a special construction only used for scheduler preallocation, which has no bank. Panics
/// will be triggered when tried to be used normally across code-base.
#[derive(Clone, Debug)]
pub struct SchedulingContext {
    mode: SchedulingMode,
    bank: Option<Arc<Bank>>,
}

impl SchedulingContext {
    pub fn for_preallocation() -> Self {
        Self {
            mode: SchedulingMode::BlockProduction,
            bank: None,
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn new_with_mode(mode: SchedulingMode, bank: Arc<Bank>) -> Self {
        Self {
            mode,
            bank: Some(bank),
        }
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn for_verification(bank: Arc<Bank>) -> Self {
        Self::new_with_mode(SchedulingMode::BlockVerification, bank)
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn for_production(bank: Arc<Bank>) -> Self {
        Self::new_with_mode(SchedulingMode::BlockProduction, bank)
    }

    pub fn is_preallocated(&self) -> bool {
        self.bank.is_none()
    }

    pub fn mode(&self) -> SchedulingMode {
        self.mode
    }

    pub fn bank(&self) -> Option<&Arc<Bank>> {
        self.bank.as_ref()
    }

    pub fn slot(&self) -> Option<Slot> {
        self.bank.as_ref().map(|bank| bank.slot())
    }
}

pub type ResultWithTimings = (Result<()>, ExecuteTimings);

/// A hint from the bank about the reason the caller is waiting on its scheduler.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum WaitReason {
    // The bank wants its scheduler to terminate after the completion of transaction execution, in
    // order to freeze itself immediately thereafter. This is by far the most normal wait reason.
    //
    // Note that `wait_for_termination(TerminatedToFreeze)` must explicitly be done prior
    // to Bank::freeze(). This can't be done inside Bank::freeze() implicitly to remain it
    // infallible.
    TerminatedToFreeze,
    // The bank wants its scheduler to terminate just like `TerminatedToFreeze` and indicate that
    // Drop::drop() is the caller.
    DroppedFromBankForks,
    // The bank wants its scheduler to pause after the completion without being returned to the
    // pool. This is to update bank's recent blockhash and to collect scheduler's internally-held
    // `ResultWithTimings` later.
    PausedForRecentBlockhash,
}

impl WaitReason {
    pub fn is_paused(&self) -> bool {
        // Exhaustive `match` is preferred here than `matches!()` to trigger an explicit
        // decision to be made, should we add new variants like `PausedForFooBar`...
        match self {
            WaitReason::PausedForRecentBlockhash => true,
            WaitReason::TerminatedToFreeze | WaitReason::DroppedFromBankForks => false,
        }
    }

    pub fn is_dropped(&self) -> bool {
        // Exhaustive `match` is preferred here than `matches!()` to trigger an explicit
        // decision to be made, should we add new variants like `PausedForFooBar`...
        match self {
            WaitReason::DroppedFromBankForks => true,
            WaitReason::TerminatedToFreeze | WaitReason::PausedForRecentBlockhash => false,
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum SchedulerStatus {
    /// Unified scheduler is disabled or installed scheduler is consumed by
    /// [`InstalledScheduler::wait_for_termination`]. Note that transition to [`Self::Unavailable`]
    /// from {[`Self::Active`], [`Self::Stale`]} is one-way (i.e. one-time) unlike [`Self::Active`]
    /// <=> [`Self::Stale`] below.  Also, this variant is transiently used as a placeholder
    /// internally when transitioning scheduler statuses, which isn't observable unless panic is
    /// happening.
    Unavailable,
    /// Scheduler is installed into a bank; could be running or just be waiting for additional
    /// transactions. This will be transitioned to [`Self::Stale`] after certain time (i.e.
    /// `solana_unified_scheduler_pool::DEFAULT_TIMEOUT_DURATION`) has passed if its bank hasn't
    /// been frozen since installed.
    Active(InstalledSchedulerBox),
    /// Scheduler has yet to freeze its associated bank even after it's taken too long since
    /// installed, resulting in returning the scheduler back to the pool. Later, this can
    /// immediately (i.e. transparently) be transitioned to [`Self::Active`] as soon as there's new
    /// transaction to be executed (= [`BankWithScheduler::schedule_transaction_executions`] is
    /// called, which internally calls [`BankWithSchedulerInner::with_active_scheduler`] to make
    /// the transition happen).
    Stale(InstalledSchedulerPoolArc, ResultWithTimings),
}

impl SchedulerStatus {
    fn new(scheduler: Option<InstalledSchedulerBox>) -> Self {
        match scheduler {
            Some(scheduler) => SchedulerStatus::Active(scheduler),
            None => SchedulerStatus::Unavailable,
        }
    }

    fn transition_from_stale_to_active(
        &mut self,
        f: impl FnOnce(InstalledSchedulerPoolArc, ResultWithTimings) -> InstalledSchedulerBox,
    ) {
        let Self::Stale(pool, result_with_timings) = mem::replace(self, Self::Unavailable) else {
            panic!("transition to Active failed: {self:?}");
        };
        *self = Self::Active(f(pool, result_with_timings));
    }

    fn maybe_transition_from_active_to_stale(
        &mut self,
        f: impl FnOnce(InstalledSchedulerBox) -> (InstalledSchedulerPoolArc, ResultWithTimings),
    ) {
        if !matches!(self, Self::Active(_scheduler)) {
            return;
        }
        let Self::Active(scheduler) = mem::replace(self, Self::Unavailable) else {
            unreachable!("not active: {self:?}");
        };
        let (pool, result_with_timings) = f(scheduler);
        *self = Self::Stale(pool, result_with_timings);
    }

    fn transition_from_active_to_unavailable(&mut self) -> InstalledSchedulerBox {
        let Self::Active(scheduler) = mem::replace(self, Self::Unavailable) else {
            panic!("transition to Unavailable failed: {self:?}");
        };
        scheduler
    }

    fn transition_from_stale_to_unavailable(&mut self) -> ResultWithTimings {
        let Self::Stale(_pool, result_with_timings) = mem::replace(self, Self::Unavailable) else {
            panic!("transition to Unavailable failed: {self:?}");
        };
        result_with_timings
    }

    fn active_scheduler(&self) -> &InstalledSchedulerBox {
        let SchedulerStatus::Active(active_scheduler) = self else {
            panic!("not active: {self:?}");
        };
        active_scheduler
    }
}

/// Very thin wrapper around Arc<Bank>
///
/// It brings type-safety against accidental mixing of bank and scheduler with different slots,
/// which is a pretty dangerous condition. Also, it guarantees to call wait_for_termination() via
/// ::drop() by DropBankService, which receives Vec<BankWithScheduler> from BankForks::set_root()'s
/// pruning, mostly matching to Arc<Bank>'s lifetime by piggybacking on the pruning.
///
/// Semantically, a scheduler is tightly coupled with a particular bank. But scheduler wasn't put
/// into Bank fields to avoid circular-references (a scheduler needs to refer to its accompanied
/// Arc<Bank>). BankWithScheduler behaves almost like Arc<Bank>. It only adds a few of transaction
/// scheduling and scheduler management functions. For this reason, `bank` variable names should be
/// used for `BankWithScheduler` across codebase.
///
/// BankWithScheduler even implements Deref for convenience. And Clone is omitted to implement to
/// avoid ambiguity as to which to clone: BankWithScheduler or Arc<Bank>. Use
/// clone_without_scheduler() for Arc<Bank>. Otherwise, use clone_with_scheduler() (this should be
/// unusual outside scheduler code-path)
#[derive(Debug)]
pub struct BankWithScheduler {
    inner: Arc<BankWithSchedulerInner>,
}

#[derive(Debug)]
pub struct BankWithSchedulerInner {
    bank: Arc<Bank>,
    scheduler: InstalledSchedulerRwLock,
}
pub type InstalledSchedulerRwLock = RwLock<SchedulerStatus>;

impl BankWithScheduler {
    /// Creates a new `BankWithScheduler` from bank and its associated scheduler.
    ///
    /// # Panics
    ///
    /// Panics if `scheduler`'s scheduling context is unmatched to given bank or for scheduler
    /// preallocation.
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn new(bank: Arc<Bank>, scheduler: Option<InstalledSchedulerBox>) -> Self {
        // Avoid the fatal situation in which bank is being associated with a scheduler associated
        // to a different bank!
        if let Some(bank_in_context) = scheduler
            .as_ref()
            .map(|scheduler| scheduler.context().bank().unwrap())
        {
            assert!(Arc::ptr_eq(&bank, bank_in_context));
        }

        Self {
            inner: Arc::new(BankWithSchedulerInner {
                bank,
                scheduler: RwLock::new(SchedulerStatus::new(scheduler)),
            }),
        }
    }

    pub fn new_without_scheduler(bank: Arc<Bank>) -> Self {
        Self::new(bank, None)
    }

    pub fn clone_with_scheduler(&self) -> BankWithScheduler {
        BankWithScheduler {
            inner: self.inner.clone(),
        }
    }

    pub fn clone_without_scheduler(&self) -> Arc<Bank> {
        self.inner.bank.clone()
    }

    pub fn register_tick(&self, hash: &Hash) {
        self.inner.bank.register_tick(hash, &self.inner.scheduler);
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn fill_bank_with_ticks_for_tests(&self) {
        self.do_fill_bank_with_ticks_for_tests(&self.inner.scheduler);
    }

    pub fn has_installed_scheduler(&self) -> bool {
        !matches!(
            &*self.inner.scheduler.read().unwrap(),
            SchedulerStatus::Unavailable
        )
    }

    /// Schedule the transaction as long as the scheduler hasn't been aborted.
    ///
    /// If the scheduler has been aborted, this doesn't schedule the transaction, instead just
    /// return the error of prior scheduled transaction.
    ///
    /// Calling this will panic if the installed scheduler is Unavailable (the bank is
    /// wait_for_termination()-ed or the unified scheduler is disabled in the first place).
    pub fn schedule_transaction_executions(
        &self,
        transaction_with_task_ids: impl ExactSizeIterator<
            Item = (RuntimeTransaction<SanitizedTransaction>, OrderedTaskId),
        >,
    ) -> Result<()> {
        trace!(
            "schedule_transaction_executions(): {} txs",
            transaction_with_task_ids.len()
        );

        let schedule_result: ScheduleResult = self.inner.with_active_scheduler(|scheduler| {
            for (sanitized_transaction, task_id) in transaction_with_task_ids {
                scheduler.schedule_execution(sanitized_transaction, task_id)?;
            }
            Ok(())
        });

        if schedule_result.is_err() {
            // This write lock isn't atomic with the above the read lock. So, another thread
            // could have called .recover_error_after_abort() while we're literally stuck at
            // the gaps of these locks (i.e. this comment in source code wise) under extreme
            // race conditions. Thus, .recover_error_after_abort() is made idempotetnt for that
            // consideration in mind.
            //
            // Lastly, this non-atomic nature is intentional for optimizing the fast code-path
            return Err(self.inner.retrieve_error_after_schedule_failure());
        }

        Ok(())
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub(crate) fn create_timeout_listener(&self) -> TimeoutListener {
        self.inner.do_create_timeout_listener()
    }

    // take needless &mut only to communicate its semantic mutability to humans...
    #[cfg(feature = "dev-context-only-utils")]
    pub fn drop_scheduler(&mut self) {
        self.inner.drop_scheduler();
    }

    pub fn unpause_new_block_production_scheduler(&self) {
        if let SchedulerStatus::Active(scheduler) = &*self.inner.scheduler.read().unwrap() {
            assert_matches!(scheduler.context().mode(), SchedulingMode::BlockProduction);
            scheduler.unpause_after_taken();
        }
    }

    pub(crate) fn wait_for_paused_scheduler(bank: &Bank, scheduler: &InstalledSchedulerRwLock) {
        let maybe_result_with_timings = BankWithSchedulerInner::wait_for_scheduler_termination(
            bank,
            scheduler,
            WaitReason::PausedForRecentBlockhash,
        );
        assert!(
            maybe_result_with_timings.is_none(),
            "Premature result was returned from scheduler after paused (slot: {})",
            bank.slot(),
        );
    }

    #[must_use]
    pub fn wait_for_completed_scheduler(&self) -> Option<ResultWithTimings> {
        BankWithSchedulerInner::wait_for_scheduler_termination(
            &self.inner.bank,
            &self.inner.scheduler,
            WaitReason::TerminatedToFreeze,
        )
    }

    pub const fn no_scheduler_available() -> InstalledSchedulerRwLock {
        RwLock::new(SchedulerStatus::Unavailable)
    }
}

impl BankWithSchedulerInner {
    fn with_active_scheduler(
        self: &Arc<Self>,
        f: impl FnOnce(&InstalledSchedulerBox) -> ScheduleResult,
    ) -> ScheduleResult {
        let scheduler = self.scheduler.read().unwrap();
        match &*scheduler {
            SchedulerStatus::Active(scheduler) => {
                // This is the fast path, needing single read-lock most of time.
                f(scheduler)
            }
            SchedulerStatus::Stale(_pool, (result, _timings)) if result.is_err() => {
                trace!(
                    "with_active_scheduler: bank (slot: {}) has a stale aborted scheduler...",
                    self.bank.slot(),
                );
                Err(SchedulerAborted)
            }
            SchedulerStatus::Stale(pool, _result_with_timings) => {
                let pool = pool.clone();
                drop(scheduler);

                // Schedulers can be stale only if its mode is block-verification. So,
                // unconditional context construction for verification is okay here.
                let context = SchedulingContext::for_verification(self.bank.clone());
                let mut scheduler = self.scheduler.write().unwrap();
                trace!("with_active_scheduler: {scheduler:?}");
                scheduler.transition_from_stale_to_active(|pool, result_with_timings| {
                    let scheduler = pool.take_resumed_scheduler(context, result_with_timings);
                    info!(
                        "with_active_scheduler: bank (slot: {}) got active, taking scheduler (id: \
                         {})",
                        self.bank.slot(),
                        scheduler.id(),
                    );
                    scheduler
                });
                drop(scheduler);

                let scheduler = self.scheduler.read().unwrap();
                // Re-register a new timeout listener only after acquiring the read lock;
                // Otherwise, the listener would again put scheduler into Stale before the read
                // lock under an extremely-rare race condition, causing panic below in
                // active_scheduler().
                pool.register_timeout_listener(self.do_create_timeout_listener());
                f(scheduler.active_scheduler())
            }
            SchedulerStatus::Unavailable => unreachable!("no installed scheduler"),
        }
    }

    fn do_create_timeout_listener(self: &Arc<Self>) -> TimeoutListener {
        let weak_bank = Arc::downgrade(self);
        TimeoutListener::new(move |pool| {
            let Some(bank) = weak_bank.upgrade() else {
                // BankWithSchedulerInner is already dropped, indicating successful and timely
                // `wait_for_termination()` on the bank prior to this triggering of the timeout,
                // rendering this callback invocation no-op.
                return;
            };

            let Ok(mut scheduler) = bank.scheduler.write() else {
                // BankWithScheduler's lock is poisoned...
                return;
            };

            // Reaching here means that it's been awhile since this active scheduler is taken from
            // the pool and yet it has yet to be `wait_for_termination()`-ed. To avoid unbounded
            // thread creation under forky condition, return the scheduler for now, even if the
            // bank could process more transactions later.
            scheduler.maybe_transition_from_active_to_stale(|scheduler| {
                // Return the installed scheduler back to the scheduler pool as soon as the
                // scheduler indicates the completion of all currently-scheduled transaction
                // executions by `solana_unified_scheduler_pool::ThreadManager::end_session()`
                // internally.

                let id = scheduler.id();
                let (result_with_timings, uninstalled_scheduler) =
                    scheduler.wait_for_termination(false);
                uninstalled_scheduler.return_to_pool();
                info!(
                    "timeout_listener: bank (slot: {}) got stale, returning scheduler (id: {})",
                    bank.bank.slot(),
                    id,
                );
                (pool, result_with_timings)
            });
            trace!("timeout_listener: {scheduler:?}");
        })
    }

    /// This must not be called until `Err(SchedulerAborted)` is observed. Violating this should
    /// `panic!()`.
    fn retrieve_error_after_schedule_failure(&self) -> TransactionError {
        let mut scheduler = self.scheduler.write().unwrap();
        match &mut *scheduler {
            SchedulerStatus::Active(scheduler) => scheduler.recover_error_after_abort(),
            SchedulerStatus::Stale(_pool, (result, _timings)) if result.is_err() => {
                result.clone().unwrap_err()
            }
            _ => unreachable!("no error in {:?}", self.scheduler),
        }
    }

    #[must_use]
    fn wait_for_completed_scheduler_from_drop(&self) -> Option<ResultWithTimings> {
        Self::wait_for_scheduler_termination(
            &self.bank,
            &self.scheduler,
            WaitReason::DroppedFromBankForks,
        )
    }

    #[must_use]
    fn wait_for_scheduler_termination(
        bank: &Bank,
        scheduler: &InstalledSchedulerRwLock,
        reason: WaitReason,
    ) -> Option<ResultWithTimings> {
        debug!(
            "wait_for_scheduler_termination(slot: {}, reason: {:?}): started at {:?}...",
            bank.slot(),
            reason,
            thread::current(),
        );

        let mut scheduler = scheduler.write().unwrap();
        let (was_noop, result_with_timings) = match &mut *scheduler {
            SchedulerStatus::Active(scheduler) if reason.is_paused() => {
                scheduler.pause_for_recent_blockhash();
                (false, None)
            }
            SchedulerStatus::Active(_scheduler) => {
                let scheduler = scheduler.transition_from_active_to_unavailable();
                let (result_with_timings, uninstalled_scheduler) =
                    scheduler.wait_for_termination(reason.is_dropped());
                uninstalled_scheduler.return_to_pool();
                (false, Some(result_with_timings))
            }
            SchedulerStatus::Stale(_pool, _result_with_timings) if reason.is_paused() => {
                // Do nothing for pauses because the scheduler termination is guaranteed to be
                // called later.
                (true, None)
            }
            SchedulerStatus::Stale(_pool, _result_with_timings) => {
                let result_with_timings = scheduler.transition_from_stale_to_unavailable();
                (true, Some(result_with_timings))
            }
            SchedulerStatus::Unavailable => (true, None),
        };
        debug!(
            "wait_for_scheduler_termination(slot: {}, reason: {:?}): noop: {:?}, result: {:?} at \
             {:?}...",
            bank.slot(),
            reason,
            was_noop,
            result_with_timings.as_ref().map(|(result, _)| result),
            thread::current(),
        );
        trace!("wait_for_scheduler_termination(result_with_timings: {result_with_timings:?})",);

        result_with_timings
    }

    fn drop_scheduler(&self) {
        if thread::panicking() {
            error!(
                "BankWithSchedulerInner::drop_scheduler(): slot: {} skipping due to already \
                 panicking...",
                self.bank.slot(),
            );
            return;
        }

        // There's no guarantee ResultWithTimings is available or not at all when being dropped.
        if let Some(Err(err)) = self
            .wait_for_completed_scheduler_from_drop()
            .map(|(result, _timings)| result)
        {
            warn!(
                "BankWithSchedulerInner::drop_scheduler(): slot: {} discarding error from \
                 scheduler: {:?}",
                self.bank.slot(),
                err,
            );
        }
    }
}

impl Drop for BankWithSchedulerInner {
    fn drop(&mut self) {
        self.drop_scheduler();
    }
}

impl Deref for BankWithScheduler {
    type Target = Arc<Bank>;

    fn deref(&self) -> &Self::Target {
        &self.inner.bank
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            bank::test_utils::goto_end_of_slot_with_scheduler,
            genesis_utils::{create_genesis_config, GenesisConfigInfo},
        },
        mockall::Sequence,
        solana_system_transaction as system_transaction,
        std::sync::Mutex,
    };

    fn setup_mocked_scheduler_with_extra(
        bank: Arc<Bank>,
        is_dropped_flags: impl Iterator<Item = bool>,
        f: Option<impl Fn(&mut MockInstalledScheduler)>,
    ) -> InstalledSchedulerBox {
        let mut mock = MockInstalledScheduler::new();
        let seq = Arc::new(Mutex::new(Sequence::new()));

        mock.expect_context()
            .times(1)
            .in_sequence(&mut seq.lock().unwrap())
            .return_const(SchedulingContext::for_verification(bank));

        for wait_reason in is_dropped_flags {
            let seq_cloned = seq.clone();
            mock.expect_wait_for_termination()
                .with(mockall::predicate::eq(wait_reason))
                .times(1)
                .in_sequence(&mut seq.lock().unwrap())
                .returning(move |_| {
                    let mut mock_uninstalled = MockUninstalledScheduler::new();
                    mock_uninstalled
                        .expect_return_to_pool()
                        .times(1)
                        .in_sequence(&mut seq_cloned.lock().unwrap())
                        .returning(|| ());
                    (
                        (Ok(()), ExecuteTimings::default()),
                        Box::new(mock_uninstalled),
                    )
                });
        }

        if let Some(f) = f {
            f(&mut mock);
        }

        Box::new(mock)
    }

    fn setup_mocked_scheduler(
        bank: Arc<Bank>,
        is_dropped_flags: impl Iterator<Item = bool>,
    ) -> InstalledSchedulerBox {
        setup_mocked_scheduler_with_extra(
            bank,
            is_dropped_flags,
            None::<fn(&mut MockInstalledScheduler) -> ()>,
        )
    }

    #[test]
    fn test_scheduler_normal_termination() {
        agave_logger::setup();

        let bank = Arc::new(Bank::default_for_tests());
        let bank = BankWithScheduler::new(
            bank.clone(),
            Some(setup_mocked_scheduler(bank, [false].into_iter())),
        );
        assert!(bank.has_installed_scheduler());
        assert_matches!(bank.wait_for_completed_scheduler(), Some(_));

        // Repeating to call wait_for_completed_scheduler() is okay with no ResultWithTimings being
        // returned.
        assert!(!bank.has_installed_scheduler());
        assert_matches!(bank.wait_for_completed_scheduler(), None);
    }

    #[test]
    fn test_no_scheduler_termination() {
        agave_logger::setup();

        let bank = Arc::new(Bank::default_for_tests());
        let bank = BankWithScheduler::new_without_scheduler(bank);

        // Calling wait_for_completed_scheduler() is noop, when no scheduler is installed.
        assert!(!bank.has_installed_scheduler());
        assert_matches!(bank.wait_for_completed_scheduler(), None);
    }

    #[test]
    fn test_scheduler_termination_from_drop() {
        agave_logger::setup();

        let bank = Arc::new(Bank::default_for_tests());
        let bank = BankWithScheduler::new(
            bank.clone(),
            Some(setup_mocked_scheduler(bank, [true].into_iter())),
        );
        drop(bank);
    }

    #[test]
    fn test_scheduler_pause() {
        agave_logger::setup();

        let bank = Arc::new(crate::bank::tests::create_simple_test_bank(42));
        let bank = BankWithScheduler::new(
            bank.clone(),
            Some(setup_mocked_scheduler_with_extra(
                bank,
                [false].into_iter(),
                Some(|mocked: &mut MockInstalledScheduler| {
                    mocked
                        .expect_pause_for_recent_blockhash()
                        .times(1)
                        .returning(|| ());
                }),
            )),
        );
        goto_end_of_slot_with_scheduler(&bank);
        assert_matches!(bank.wait_for_completed_scheduler(), Some(_));
    }

    fn do_test_schedule_execution(should_succeed: bool) {
        agave_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let tx0 = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let mocked_scheduler = setup_mocked_scheduler_with_extra(
            bank.clone(),
            [true].into_iter(),
            Some(|mocked: &mut MockInstalledScheduler| {
                if should_succeed {
                    mocked
                        .expect_schedule_execution()
                        .times(1)
                        .returning(|_, _| Ok(()));
                } else {
                    mocked
                        .expect_schedule_execution()
                        .times(1)
                        .returning(|_, _| Err(SchedulerAborted));
                    mocked
                        .expect_recover_error_after_abort()
                        .times(1)
                        .returning(|| TransactionError::InsufficientFundsForFee);
                }
            }),
        );

        let bank = BankWithScheduler::new(bank, Some(mocked_scheduler));
        let result = bank.schedule_transaction_executions([(tx0, 0)].into_iter());
        if should_succeed {
            assert_matches!(result, Ok(()));
        } else {
            assert_matches!(result, Err(TransactionError::InsufficientFundsForFee));
        }
    }

    #[test]
    fn test_schedule_execution_success() {
        do_test_schedule_execution(true);
    }

    #[test]
    fn test_schedule_execution_failure() {
        do_test_schedule_execution(false);
    }
}
