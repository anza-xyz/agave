//! Transaction scheduling code.
//!
//! This crate implements 3 solana-runtime traits [`InstalledScheduler`], [`UninstalledScheduler`]
//! and [`InstalledSchedulerPool`] to provide a concrete transaction scheduling implementation
//! (including executing txes and committing tx results).
//!
//! At the highest level, this crate takes [`SanitizedTransaction`]s via its
//! [`InstalledScheduler::schedule_execution`] and commits any side-effects (i.e. on-chain state
//! changes) into the associated [`Bank`](solana_runtime::bank::Bank) via `solana-ledger`'s helper
//! function called [`execute_batch`].
//!
//! Refer to [`PooledScheduler`] doc comment for general overview of scheduler state transitions
//! regarding to pooling and the actual use.

#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    agave_banking_stage_ingress_types::{BankingPacketBatch, BankingPacketReceiver},
    assert_matches::assert_matches,
    crossbeam_channel::{self, never, select_biased, Receiver, RecvError, SendError, Sender},
    dashmap::DashMap,
    derive_where::derive_where,
    dyn_clone::{clone_trait_object, DynClone},
    log::*,
    scopeguard::defer,
    solana_clock::Slot,
    solana_cost_model::cost_model::CostModel,
    solana_ledger::blockstore_processor::{
        execute_batch, TransactionBatchWithIndexes, TransactionStatusSender,
    },
    solana_poh::transaction_recorder::{RecordTransactionsSummary, TransactionRecorder},
    solana_pubkey::Pubkey,
    solana_runtime::{
        installed_scheduler_pool::{
            initialized_result_with_timings, InstalledScheduler, InstalledSchedulerBox,
            InstalledSchedulerPool, InstalledSchedulerPoolArc, ResultWithTimings, ScheduleResult,
            SchedulerAborted, SchedulerId, SchedulingContext, TimeoutListener,
            UninstalledScheduler, UninstalledSchedulerBox,
        },
        prioritization_fee_cache::PrioritizationFeeCache,
        vote_sender_types::ReplayVoteSender,
    },
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_svm::transaction_processing_result::ProcessedTransaction,
    solana_timings::ExecuteTimings,
    solana_transaction::sanitized::SanitizedTransaction,
    solana_transaction_error::{TransactionError, TransactionResult as Result},
    solana_unified_scheduler_logic::{
        SchedulingMode::{self, BlockProduction, BlockVerification},
        SchedulingStateMachine, Task, UsageQueue,
    },
    static_assertions::const_assert_eq,
    std::{
        fmt::Debug,
        marker::PhantomData,
        mem,
        ops::DerefMut,
        sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed},
            Arc, Mutex, MutexGuard, OnceLock, Weak,
        },
        thread::{self, sleep, JoinHandle},
        time::{Duration, Instant},
    },
    trait_set::trait_set,
    vec_extract_if_polyfill::MakeExtractIf,
};

mod sleepless_testing;
use crate::sleepless_testing::BuilderTracked;

// dead_code is false positive; these tuple fields are used via Debug.
#[allow(dead_code)]
#[derive(Debug)]
enum CheckPoint<'a> {
    NewTask(usize),
    NewBufferedTask(usize),
    BufferedTask(usize),
    TaskHandled(usize),
    TaskAccumulated(usize, &'a Result<()>),
    SessionEnding,
    SessionFinished(Option<Slot>),
    SchedulerThreadAborted,
    IdleSchedulerCleaned(usize),
    TrashedSchedulerCleaned(usize),
    TimeoutListenerTriggered(usize),
}

type AtomicSchedulerId = AtomicU64;

/// A pool of idling schedulers (usually [`PooledScheduler`]), ready to be taken by bank.
///
/// Also, the pool runs a _cleaner_ thread named as `solScCleaner`. its jobs include:
///
/// - Shrink of pool if there are too many idle schedulers.
/// - Invocation of timeouts registered by [`InstalledSchedulerPool::register_timeout_listener`].
/// - The actual destruction of any retired schedulers including thread termination and the heavy
///   `UsageQueueLoader` drop.
///
/// `SchedulerPool` (and [`PooledScheduler`] in this regard) must be accessed as a dyn trait from
/// `solana-runtime`, because it contains some internal fields, whose types aren't available in
/// `solana-runtime` ( [`TransactionStatusSender`] and [`TransactionRecorder`]). Refer to the doc
/// comment with a diagram at [`solana_runtime::installed_scheduler_pool::InstalledScheduler`] for
/// explanation of this rather complex dyn trait/type hierarchy.
#[derive(Debug)]
pub struct SchedulerPool<S: SpawnableScheduler<TH>, TH: TaskHandler> {
    scheduler_inners: Mutex<Vec<(S::Inner, Instant)>>,
    block_production_scheduler_inner: Mutex<BlockProductionSchedulerInner<S, TH>>,
    trashed_scheduler_inners: Mutex<Vec<S::Inner>>,
    timeout_listeners: Mutex<Vec<(TimeoutListener, Instant)>>,
    handler_count: usize,
    common_handler_context: CommonHandlerContext,
    banking_stage_handler_context: Mutex<Option<BankingStageHandlerContext>>,
    // weak_self could be elided by changing InstalledScheduler::take_scheduler()'s receiver to
    // Arc<Self> from &Self, because SchedulerPool is used as in the form of Arc<SchedulerPool>
    // almost always. But, this would cause wasted and noisy Arc::clone()'s at every call sites.
    //
    // Alternatively, `impl InstalledScheduler for Arc<SchedulerPool>` approach could be explored
    // but it entails its own problems due to rustc's coherence and necessitated newtype with the
    // type graph of InstalledScheduler being quite elaborate.
    //
    // After these considerations, this weak_self approach is chosen at the cost of some additional
    // memory increase.
    weak_self: Weak<Self>,
    next_scheduler_id: AtomicSchedulerId,
    max_usage_queue_count: usize,
    _phantom: PhantomData<TH>,
}

/// A small tri-state [`Option`]-like private helper type to codify the existence of
/// block-production scheduler as a singleton.
///
/// Block-production scheduler should carry its buffered (= not-yet-processed) tasks over sessions
/// (= banks) and `banking_packet_receiver` shouldn't be consumed by multiple schedulers at once.
/// So, it's managed differently from block-verification schedulers.
#[derive(Default, Debug)]
enum BlockProductionSchedulerInner<S: SpawnableScheduler<TH>, TH: TaskHandler> {
    #[default]
    NotSpawned,
    Pooled(S::Inner),
    Taken(SchedulerId),
}

impl<S: SpawnableScheduler<TH>, TH: TaskHandler> BlockProductionSchedulerInner<S, TH> {
    fn can_put(&self, returned: &S::Inner) -> bool {
        match self {
            Self::NotSpawned => false,
            Self::Pooled(inner) => {
                // the given `returned` inner must be a block-verification scheduler if there's
                // already a pooled block-production scheduler inner here. So, return `false` after
                // sanity check to detect double `put` intention with following `assert_ne!()`.
                assert_ne!(inner.id(), returned.id());
                false
            }
            Self::Taken(id) => *id == returned.id(),
        }
    }

    fn put_spawned(&mut self, inner: S::Inner) {
        assert_matches!(mem::replace(self, Self::Pooled(inner)), Self::NotSpawned);
    }

    fn trash_taken(&mut self) {
        assert_matches!(mem::replace(self, Self::NotSpawned), Self::Taken(_));
    }

    fn put_returned(&mut self, inner: S::Inner) {
        let new = inner.id();
        assert_matches!(mem::replace(self, Self::Pooled(inner)), Self::Taken(old) if old == new);
    }

    fn take_pooled(&mut self) -> S::Inner {
        let id = {
            let Self::Pooled(inner) = &self else {
                panic!("cannot take: {:?}", self)
            };
            inner.id()
        };
        let Self::Pooled(inner) = mem::replace(self, Self::Taken(id)) else {
            unreachable!();
        };
        inner
    }
}

#[derive(derive_more::Debug, Clone)]
pub struct HandlerContext {
    log_messages_bytes_limit: Option<usize>,
    transaction_status_sender: Option<TransactionStatusSender>,
    replay_vote_sender: Option<ReplayVoteSender>,
    prioritization_fee_cache: Arc<PrioritizationFeeCache>,
    banking_packet_receiver: BankingPacketReceiver,
    #[debug("{banking_packet_handler:p}")]
    banking_packet_handler: Box<dyn BankingPacketHandler>,
    banking_stage_helper: Option<Arc<BankingStageHelper>>,
    transaction_recorder: Option<TransactionRecorder>,
}

impl HandlerContext {
    fn banking_stage_helper(&self) -> &BankingStageHelper {
        self.banking_stage_helper.as_ref().unwrap()
    }
}

#[derive(Debug, Clone)]
struct CommonHandlerContext {
    log_messages_bytes_limit: Option<usize>,
    transaction_status_sender: Option<TransactionStatusSender>,
    replay_vote_sender: Option<ReplayVoteSender>,
    prioritization_fee_cache: Arc<PrioritizationFeeCache>,
}

impl CommonHandlerContext {
    fn into_handler_context(
        self,
        banking_packet_receiver: BankingPacketReceiver,
        banking_packet_handler: Box<dyn BankingPacketHandler>,
        banking_stage_helper: Option<Arc<BankingStageHelper>>,
        transaction_recorder: Option<TransactionRecorder>,
    ) -> HandlerContext {
        let Self {
            log_messages_bytes_limit,
            transaction_status_sender,
            replay_vote_sender,
            prioritization_fee_cache,
        } = self;

        HandlerContext {
            log_messages_bytes_limit,
            transaction_status_sender,
            replay_vote_sender,
            prioritization_fee_cache,
            banking_packet_receiver,
            banking_packet_handler,
            banking_stage_helper,
            transaction_recorder,
        }
    }
}

#[derive(derive_more::Debug)]
struct BankingStageHandlerContext {
    banking_packet_receiver: BankingPacketReceiver,
    #[debug("{banking_packet_handler:p}")]
    banking_packet_handler: Box<dyn BankingPacketHandler>,
    transaction_recorder: TransactionRecorder,
}

trait_set! {
    pub trait BankingPacketHandler =
        DynClone + FnMut(&BankingStageHelper, BankingPacketBatch) + Send + 'static;
}
// Make this `Clone`-able so that it can easily propagated to all the handler threads.
clone_trait_object!(BankingPacketHandler);

#[derive(Debug)]
pub struct BankingStageHelper {
    usage_queue_loader: UsageQueueLoader,
    next_task_id: AtomicUsize,
    new_task_sender: Sender<NewTaskPayload>,
}

impl BankingStageHelper {
    fn new(new_task_sender: Sender<NewTaskPayload>) -> Self {
        Self {
            usage_queue_loader: UsageQueueLoader::default(),
            next_task_id: AtomicUsize::default(),
            new_task_sender,
        }
    }

    pub fn generate_task_ids(&self, count: usize) -> usize {
        self.next_task_id.fetch_add(count, Relaxed)
    }

    pub fn create_new_task(
        &self,
        transaction: RuntimeTransaction<SanitizedTransaction>,
        index: usize,
    ) -> Task {
        SchedulingStateMachine::create_task(transaction, index, &mut |pubkey| {
            self.usage_queue_loader.load(pubkey)
        })
    }

    fn recreate_task(&self, executed_task: Box<ExecutedTask>) -> Task {
        let new_index = self.generate_task_ids(1);
        let transaction = executed_task.into_transaction();
        self.create_new_task(transaction, new_index)
    }

    pub fn send_new_task(&self, task: Task) {
        self.new_task_sender
            .send(NewTaskPayload::Payload(task))
            .unwrap();
    }
}

pub type DefaultSchedulerPool =
    SchedulerPool<PooledScheduler<DefaultTaskHandler>, DefaultTaskHandler>;

const DEFAULT_POOL_CLEANER_INTERVAL: Duration = Duration::from_secs(10);
const DEFAULT_MAX_POOLING_DURATION: Duration = Duration::from_secs(180);
const DEFAULT_TIMEOUT_DURATION: Duration = Duration::from_secs(12);
// Rough estimate of max UsageQueueLoader size in bytes:
//   UsageFromTask * UsageQueue's capacity * DEFAULT_MAX_USAGE_QUEUE_COUNT
//   16 bytes      * 128 items             * 262_144 entries               == 512 MiB
// It's expected that there will be 2 or 3 pooled schedulers constantly when running against
// mainnnet-beta. That means the total memory consumption for the idle close-to-be-trashed pooled
// schedulers is set to 1.0 ~ 1.5 GiB. This value is chosen to maximize performance under the
// normal cluster condition to avoid memory reallocation as much as possible. That said, it's not
// likely this would allow unbounded memory growth when the cluster is unstable or under some kind
// of attacks. That's because this limit is enforced at every slot and the UsageQueueLoader itself
// is recreated without any entries at first, needing to repopulate by means of actual use to eat
// the memory.
//
// Along the lines, this isn't problematic for the development settings (= solana-test-validator),
// because UsageQueueLoader won't grow that much to begin with.
const DEFAULT_MAX_USAGE_QUEUE_COUNT: usize = 262_144;

impl<S, TH> SchedulerPool<S, TH>
where
    S: SpawnableScheduler<TH>,
    TH: TaskHandler,
{
    // Some internal impl and test code want an actual concrete type, NOT the
    // `dyn InstalledSchedulerPool`. So don't merge this into `Self::new_dyn()`.
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn new(
        handler_count: Option<usize>,
        log_messages_bytes_limit: Option<usize>,
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: Option<ReplayVoteSender>,
        prioritization_fee_cache: Arc<PrioritizationFeeCache>,
    ) -> Arc<Self> {
        Self::do_new(
            handler_count,
            log_messages_bytes_limit,
            transaction_status_sender,
            replay_vote_sender,
            prioritization_fee_cache,
            DEFAULT_POOL_CLEANER_INTERVAL,
            DEFAULT_MAX_POOLING_DURATION,
            DEFAULT_MAX_USAGE_QUEUE_COUNT,
            DEFAULT_TIMEOUT_DURATION,
        )
    }

    fn do_new(
        handler_count: Option<usize>,
        log_messages_bytes_limit: Option<usize>,
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: Option<ReplayVoteSender>,
        prioritization_fee_cache: Arc<PrioritizationFeeCache>,
        pool_cleaner_interval: Duration,
        max_pooling_duration: Duration,
        max_usage_queue_count: usize,
        timeout_duration: Duration,
    ) -> Arc<Self> {
        let handler_count = handler_count.unwrap_or(Self::default_handler_count());
        assert!(handler_count >= 1);

        let scheduler_pool = Arc::new_cyclic(|weak_self| Self {
            scheduler_inners: Mutex::default(),
            block_production_scheduler_inner: Mutex::default(),
            trashed_scheduler_inners: Mutex::default(),
            timeout_listeners: Mutex::default(),
            handler_count,
            common_handler_context: CommonHandlerContext {
                log_messages_bytes_limit,
                transaction_status_sender,
                replay_vote_sender,
                prioritization_fee_cache,
            },
            banking_stage_handler_context: Mutex::default(),
            weak_self: weak_self.clone(),
            next_scheduler_id: AtomicSchedulerId::default(),
            max_usage_queue_count,
            _phantom: PhantomData,
        });

        let cleaner_main_loop = {
            let weak_scheduler_pool = Arc::downgrade(&scheduler_pool);

            move || loop {
                sleep(pool_cleaner_interval);

                let Some(scheduler_pool) = weak_scheduler_pool.upgrade() else {
                    break;
                };

                let now = Instant::now();

                let idle_inner_count = {
                    // Pre-allocate rather large capacity to avoid reallocation inside the lock.
                    let mut idle_inners = Vec::with_capacity(128);

                    let Ok(mut scheduler_inners) = scheduler_pool.scheduler_inners.lock() else {
                        break;
                    };
                    // Use the still-unstable Vec::extract_if() even on stable rust toolchain by
                    // using a polyfill and allowing unstable_name_collisions, because it's
                    // simplest to code and fastest to run (= O(n); single linear pass and no
                    // reallocation).
                    //
                    // Note that this critical section could block the latency-sensitive replay
                    // code-path via ::take_scheduler().
                    idle_inners.extend(MakeExtractIf::extract_if(
                        scheduler_inners.deref_mut(),
                        |(_inner, pooled_at)| now.duration_since(*pooled_at) > max_pooling_duration,
                    ));
                    drop(scheduler_inners);

                    let idle_inner_count = idle_inners.len();
                    drop(idle_inners);
                    idle_inner_count
                };

                let trashed_inner_count = {
                    let Ok(mut trashed_scheduler_inners) =
                        scheduler_pool.trashed_scheduler_inners.lock()
                    else {
                        break;
                    };
                    let trashed_inners: Vec<_> = mem::take(&mut *trashed_scheduler_inners);
                    drop(trashed_scheduler_inners);

                    let trashed_inner_count = trashed_inners.len();
                    drop(trashed_inners);
                    trashed_inner_count
                };

                let triggered_timeout_listener_count = {
                    // Pre-allocate rather large capacity to avoid reallocation inside the lock.
                    let mut expired_listeners = Vec::with_capacity(128);
                    let Ok(mut timeout_listeners) = scheduler_pool.timeout_listeners.lock() else {
                        break;
                    };
                    expired_listeners.extend(MakeExtractIf::extract_if(
                        timeout_listeners.deref_mut(),
                        |(_callback, registered_at)| {
                            now.duration_since(*registered_at) > timeout_duration
                        },
                    ));
                    drop(timeout_listeners);

                    let count = expired_listeners.len();
                    // Now triggers all expired listeners. Usually, triggering timeouts does
                    // nothing because the callbacks will be no-op if already successfully
                    // `wait_for_termination()`-ed.
                    for (timeout_listener, _registered_at) in expired_listeners {
                        timeout_listener.trigger(scheduler_pool.clone());
                    }
                    count
                };

                info!(
                    "Scheduler pool cleaner: dropped {} idle inners, {} trashed inners, triggered {} timeout listeners",
                    idle_inner_count, trashed_inner_count, triggered_timeout_listener_count,
                );
                sleepless_testing::at(CheckPoint::IdleSchedulerCleaned(idle_inner_count));
                sleepless_testing::at(CheckPoint::TrashedSchedulerCleaned(trashed_inner_count));
                sleepless_testing::at(CheckPoint::TimeoutListenerTriggered(
                    triggered_timeout_listener_count,
                ));
            }
        };

        // No need to join; the spawned main loop will gracefully exit.
        thread::Builder::new()
            .name("solScCleaner".to_owned())
            .spawn_tracked(cleaner_main_loop)
            .unwrap();

        scheduler_pool
    }

    // This apparently-meaningless wrapper is handy, because some callers explicitly want
    // `dyn InstalledSchedulerPool` to be returned for type inference convenience.
    pub fn new_dyn(
        handler_count: Option<usize>,
        log_messages_bytes_limit: Option<usize>,
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: Option<ReplayVoteSender>,
        prioritization_fee_cache: Arc<PrioritizationFeeCache>,
    ) -> InstalledSchedulerPoolArc {
        Self::new(
            handler_count,
            log_messages_bytes_limit,
            transaction_status_sender,
            replay_vote_sender,
            prioritization_fee_cache,
        )
    }

    // See a comment at the weak_self field for justification of this method's existence.
    fn self_arc(&self) -> Arc<Self> {
        self.weak_self
            .upgrade()
            .expect("self-referencing Arc-ed pool")
    }

    fn new_scheduler_id(&self) -> SchedulerId {
        self.next_scheduler_id.fetch_add(1, Relaxed)
    }

    // This fn needs to return immediately due to being part of the blocking
    // `::wait_for_termination()` call.
    fn return_scheduler(&self, scheduler: S::Inner) {
        // Refer to the comment in is_aborted() as to the exact definition of the concept of
        // _trashed_ and the interaction among different parts of unified scheduler.
        let should_trash = scheduler.is_trashed();
        let mut block_production_scheduler_inner =
            self.block_production_scheduler_inner.lock().unwrap();

        if should_trash {
            // Maintain the runtime invariant established in register_banking_stage() about
            // the availability of pooled block production scheduler by re-spawning one.
            if block_production_scheduler_inner.can_put(&scheduler) {
                block_production_scheduler_inner.trash_taken();
                // To prevent block-production scheduler from being taken in
                // do_take_resumed_scheduler() by different thread at this very moment, the
                // preceding `.trash_taken()` and following `.put_spawned()` must be done
                // atomically. That's why we pass around MutexGuard into
                // spawn_block_production_scheduler().
                self.spawn_block_production_scheduler(&mut block_production_scheduler_inner);
            }

            // Delay drop()-ing this trashed returned scheduler inner by stashing it in
            // self.trashed_scheduler_inners, which is periodically drained by the `solScCleaner`
            // thread. Dropping it could take long time (in fact,
            // PooledSchedulerInner::usage_queue_loader can contain many entries to drop).
            self.trashed_scheduler_inners
                .lock()
                .expect("not poisoned")
                .push(scheduler);
        } else if block_production_scheduler_inner.can_put(&scheduler) {
            block_production_scheduler_inner.put_returned(scheduler);
        } else {
            self.scheduler_inners
                .lock()
                .expect("not poisoned")
                .push((scheduler, Instant::now()));
        }
        drop(block_production_scheduler_inner);
    }

    #[cfg(test)]
    fn do_take_scheduler(&self, context: SchedulingContext) -> S {
        self.do_take_resumed_scheduler(context, initialized_result_with_timings())
    }

    fn do_take_resumed_scheduler(
        &self,
        context: SchedulingContext,
        result_with_timings: ResultWithTimings,
    ) -> S {
        assert_matches!(result_with_timings, (Ok(_), _));

        match context.mode() {
            BlockVerification => {
                // pop is intentional for filo, expecting relatively warmed-up scheduler due to
                // having been returned recently
                if let Some((inner, _pooled_at)) =
                    self.scheduler_inners.lock().expect("not poisoned").pop()
                {
                    S::from_inner(inner, context, result_with_timings)
                } else {
                    S::spawn(self.self_arc(), context, result_with_timings)
                }
            }
            BlockProduction => {
                // There must be a pooled block-production scheduler at this point because prior
                // register_banking_stage() invocation should have spawned such one.
                assert!(
                    self.banking_stage_handler_context
                        .lock()
                        .expect("not poisoned")
                        .is_some(),
                    "register_banking_stage() isn't called yet",
                );
                let inner = self
                    .block_production_scheduler_inner
                    .lock()
                    .expect("not poisoned")
                    .take_pooled();
                S::from_inner(inner, context, result_with_timings)
            }
        }
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn pooled_scheduler_count(&self) -> usize {
        self.scheduler_inners.lock().expect("not poisoned").len()
    }

    pub fn register_banking_stage(
        &self,
        banking_packet_receiver: BankingPacketReceiver,
        banking_packet_handler: Box<dyn BankingPacketHandler>,
        transaction_recorder: TransactionRecorder,
    ) {
        *self.banking_stage_handler_context.lock().unwrap() = Some(BankingStageHandlerContext {
            banking_packet_receiver,
            banking_packet_handler,
            transaction_recorder,
        });
        // Immediately start a block production scheduler, so that the scheduler can start
        // buffering tasks, which are preprocessed as much as possible.
        self.spawn_block_production_scheduler(
            &mut self.block_production_scheduler_inner.lock().unwrap(),
        );
    }

    fn create_handler_context(
        &self,
        mode: SchedulingMode,
        new_task_sender: &Sender<NewTaskPayload>,
    ) -> HandlerContext {
        let (
            banking_packet_receiver,
            banking_packet_handler,
            banking_stage_helper,
            transaction_recorder,
        ): (
            _,
            Box<dyn BankingPacketHandler>, /* to aid type inference */
            _,
            _,
        ) = match mode {
            BlockVerification => {
                // Return various type-specific no-op values.
                (never(), Box::new(|_, _| {}), None, None)
            }
            BlockProduction => {
                let handler_context = self.banking_stage_handler_context.lock().unwrap();
                let handler_context = handler_context.as_ref().unwrap();

                (
                    handler_context.banking_packet_receiver.clone(),
                    handler_context.banking_packet_handler.clone(),
                    Some(Arc::new(BankingStageHelper::new(new_task_sender.clone()))),
                    Some(handler_context.transaction_recorder.clone()),
                )
            }
        };
        self.common_handler_context.clone().into_handler_context(
            banking_packet_receiver,
            banking_packet_handler,
            banking_stage_helper,
            transaction_recorder,
        )
    }

    fn spawn_block_production_scheduler(
        &self,
        block_production_scheduler_inner: &mut MutexGuard<'_, BlockProductionSchedulerInner<S, TH>>,
    ) {
        let scheduler = S::spawn(
            self.self_arc(),
            SchedulingContext::for_preallocation(),
            initialized_result_with_timings(),
        );
        let ((result, _timings), inner) = scheduler.into_inner();
        assert_matches!(result, Ok(_));
        block_production_scheduler_inner.put_spawned(inner);
    }

    pub fn default_handler_count() -> usize {
        Self::calculate_default_handler_count(
            thread::available_parallelism()
                .ok()
                .map(|non_zero| non_zero.get()),
        )
    }

    pub fn calculate_default_handler_count(detected_cpu_core_count: Option<usize>) -> usize {
        // Divide by 4 just not to consume all available CPUs just with handler threads, sparing for
        // other active forks and other subsystems.
        // Also, if available_parallelism fails (which should be very rare), use 4 threads,
        // as a relatively conservatism assumption of modern multi-core systems ranging from
        // engineers' laptops to production servers.
        detected_cpu_core_count
            .map(|core_count| (core_count / 4).max(1))
            .unwrap_or(4)
    }

    pub fn cli_message() -> &'static str {
        static MESSAGE: OnceLock<String> = OnceLock::new();

        MESSAGE.get_or_init(|| {
            format!(
                "Change the number of the unified scheduler's transaction execution threads \
                 dedicated to each block, otherwise calculated as cpu_cores/4 [default: {}]",
                Self::default_handler_count()
            )
        })
    }
}

impl<S, TH> InstalledSchedulerPool for SchedulerPool<S, TH>
where
    S: SpawnableScheduler<TH>,
    TH: TaskHandler,
{
    fn take_resumed_scheduler(
        &self,
        context: SchedulingContext,
        result_with_timings: ResultWithTimings,
    ) -> InstalledSchedulerBox {
        Box::new(self.do_take_resumed_scheduler(context, result_with_timings))
    }

    fn register_timeout_listener(&self, timeout_listener: TimeoutListener) {
        self.timeout_listeners
            .lock()
            .unwrap()
            .push((timeout_listener, Instant::now()));
    }
}

pub trait TaskHandler: Send + Sync + Debug + Sized + 'static {
    fn handle(
        result: &mut Result<()>,
        timings: &mut ExecuteTimings,
        scheduling_context: &SchedulingContext,
        task: &Task,
        handler_context: &HandlerContext,
    );
}

#[derive(Debug)]
pub struct DefaultTaskHandler;

impl TaskHandler for DefaultTaskHandler {
    fn handle(
        result: &mut Result<()>,
        timings: &mut ExecuteTimings,
        scheduling_context: &SchedulingContext,
        task: &Task,
        handler_context: &HandlerContext,
    ) {
        let bank = scheduling_context.bank().unwrap();
        let transaction = task.transaction();
        let index = task.task_index();

        let batch = match scheduling_context.mode() {
            BlockVerification => {
                // scheduler must properly prevent conflicting tx executions. thus, task handler isn't
                // responsible for locking.
                bank.prepare_unlocked_batch_from_single_tx(transaction)
            }
            BlockProduction => {
                // Due to the probable presence of an independent banking thread (like the jito
                // thread), we are forced to lock the addresses unlike block verification. The
                // scheduling thread isn't appropriate for these kinds of work; so, instead do that
                // by one of handler threads.
                //
                // Assuming the banking thread isn't tightly integrated and thus it's rather opaque
                // to the unified scheduler, there's no proper way to be race-free other than
                // actually locking the accounts.
                //
                // That means there's also no proper signalling mechanism among them after
                // unlocking as well. So, we resort to spin lock here with mercy of slight
                // humbleness (100us sleep per retry).
                //
                // Note that this is quite coarse/suboptimal solution to the above problem.
                // Ideally, this should be handled more gracefully. As for the worst case analysis,
                // this will indeed create a rather noisy lock contention. However, this is already
                // the case as well for the other block producing method (CentralScheduler). So,
                // this could be justified here, hopefully...
                let mut batch;
                let started = Instant::now();
                loop {
                    batch = bank.prepare_locked_batch_from_single_tx(transaction);
                    let lock_result = &batch.lock_results()[0];
                    if let Ok(()) = lock_result {
                        break;
                    }
                    assert_matches!(lock_result, Err(TransactionError::AccountInUse));
                    if started.elapsed() > Duration::from_millis(400) {
                        *result = Err(TransactionError::CommitCancelled);
                        return;
                    }
                    sleep(Duration::from_micros(100));
                }
                batch
            }
        };
        let transaction_indexes = match scheduling_context.mode() {
            BlockVerification => vec![index],
            BlockProduction => {
                // Create a placeholder vec, which will be populated later if
                // transaction_status_sender is Some(_).
                // transaction_status_sender is usually None for staked nodes because it's only
                // used for RPC-related additional data recording. However, a staked node could
                // also be running with rpc functionalities during development. So, we need to
                // correctly support the use case for produced blocks as well, like verified blocks
                // via the replaying stage.
                // Refer `record_token_balances` in `execute_batch()` as this treatment is mirrored
                // from it.
                vec![]
            }
        };
        let batch_with_indexes = TransactionBatchWithIndexes {
            batch,
            transaction_indexes,
        };

        let pre_commit_callback = match scheduling_context.mode() {
            BlockVerification => None,
            BlockProduction => Some(|processing_result: &'_ Result<ProcessedTransaction>| {
                let Ok(processed_transaction) = processing_result else {
                    return Err(processing_result.as_ref().unwrap_err().clone());
                };

                // Now it's the procrastinated time of _optimistic_ provisioning of block cost at the
                // basis of actual executed units!
                // Block cost limits aren't provisioned upfront in block-producing unified
                // scheduler at all to avoid locking cost_tracker twice (for post-execution
                // adjustment) per transaction in almost all task handling. The only exception is
                // the unfortunate ones at the end of slot due to poh or cost limits. For the poh
                // case only, we have to suffer from twice locking, but it's very transitory,
                // considering unified scheduler immediately transitions to buffering, waiting for
                // next fresh bank.
                // In other words, unified scheduler doesn't try to populate the almost-filled bank
                // with next-higher-paying transactions. This behavior isn't perfect for profit
                // maximization; priority adherence is preferred here.
                let cost = CostModel::calculate_cost_for_executed_transaction(
                    transaction,
                    processed_transaction.executed_units(),
                    processed_transaction.loaded_accounts_data_size(),
                    &bank.feature_set,
                );
                // Note that we're about to partially commit side effects to bank in _pre commit_
                // callback. Extra care must be taken in the case of poh failure just below;
                bank.write_cost_tracker().unwrap().try_add(&cost)?;

                let RecordTransactionsSummary {
                    result,
                    starting_transaction_index,
                    ..
                } = handler_context
                    .transaction_recorder
                    .as_ref()
                    .unwrap()
                    .record_transactions(bank.slot(), vec![transaction.to_versioned_transaction()]);
                match result {
                    Ok(()) => Ok(starting_transaction_index),
                    Err(_) => {
                        // Poh failed; need to revert the committed cost state change.
                        bank.write_cost_tracker().unwrap().remove(&cost);
                        Err(TransactionError::CommitCancelled)
                    }
                }
            }),
        };

        *result = execute_batch(
            &batch_with_indexes,
            bank,
            handler_context.transaction_status_sender.as_ref(),
            handler_context.replay_vote_sender.as_ref(),
            timings,
            handler_context.log_messages_bytes_limit,
            &handler_context.prioritization_fee_cache,
            pre_commit_callback,
        );
        sleepless_testing::at(CheckPoint::TaskHandled(index));
    }
}

struct ExecutedTask {
    task: Task,
    result_with_timings: ResultWithTimings,
}

impl ExecutedTask {
    fn new_boxed(task: Task) -> Box<Self> {
        Box::new(Self {
            task,
            result_with_timings: initialized_result_with_timings(),
        })
    }

    fn into_transaction(self) -> RuntimeTransaction<SanitizedTransaction> {
        self.task.into_transaction()
    }
}

// A very tiny generic message type to signal about opening and closing of subchannels, which are
// logically segmented series of Payloads (P1) over a single continuous time-span, potentially
// carrying some subchannel metadata (P2) upon opening a new subchannel.
// Note that the above properties can be upheld only when this is used inside MPSC or SPSC channels
// (i.e. the consumer side needs to be single threaded). For the multiple consumer cases,
// ChainedChannel can be used instead.
enum SubchanneledPayload<P1, P2> {
    Payload(P1),
    OpenSubchannel(P2),
    CloseSubchannel,
}

type NewTaskPayload = SubchanneledPayload<Task, Box<(SchedulingContext, ResultWithTimings)>>;
const_assert_eq!(mem::size_of::<NewTaskPayload>(), 16);

// A tiny generic message type to synchronize multiple threads everytime some contextual data needs
// to be switched (ie. SchedulingContext), just using a single communication channel.
//
// Usually, there's no way to prevent one of those threads from mixing current and next contexts
// while processing messages with a multiple-consumer channel. A condvar or other
// out-of-bound mechanism is needed to notify about switching of contextual data. That's because
// there's no way to block those threads reliably on such a switching event just with a channel.
//
// However, if the number of consumer can be determined, this can be accomplished just over a
// single channel, which even carries an in-bound control meta-message with the contexts. The trick
// is that identical meta-messages as many as the number of threads are sent over the channel,
// along with new channel receivers to be used (hence the name of _chained_). Then, the receiving
// thread drops the old channel and is now blocked on receiving from the new channel. In this way,
// this switching can happen exactly once for each thread.
//
// Overall, this greatly simplifies the code, reduces CAS/syscall overhead per messaging to the
// minimum at the cost of a single channel recreation per switching. Needless to say, such an
// allocation can be amortized to be negligible.
//
// Lastly, there's an auxiliary channel to realize a 2-level priority queue. See comment before
// runnable_task_sender.
mod chained_channel {
    use super::*;

    // hide variants by putting this inside newtype
    enum ChainedChannelPrivate<P, C> {
        Payload(P),
        ContextAndChannels(C, Receiver<ChainedChannel<P, C>>, Receiver<P>),
    }

    pub(super) struct ChainedChannel<P, C>(ChainedChannelPrivate<P, C>);

    impl<P, C> ChainedChannel<P, C> {
        fn chain_to_new_channel(
            context: C,
            receiver: Receiver<Self>,
            aux_receiver: Receiver<P>,
        ) -> Self {
            Self(ChainedChannelPrivate::ContextAndChannels(
                context,
                receiver,
                aux_receiver,
            ))
        }
    }

    pub(super) struct ChainedChannelSender<P, C> {
        sender: Sender<ChainedChannel<P, C>>,
        aux_sender: Sender<P>,
    }

    impl<P, C: Clone> ChainedChannelSender<P, C> {
        fn new(sender: Sender<ChainedChannel<P, C>>, aux_sender: Sender<P>) -> Self {
            Self { sender, aux_sender }
        }

        pub(super) fn send_payload(
            &self,
            payload: P,
        ) -> std::result::Result<(), SendError<ChainedChannel<P, C>>> {
            self.sender
                .send(ChainedChannel(ChainedChannelPrivate::Payload(payload)))
        }

        pub(super) fn send_aux_payload(&self, payload: P) -> std::result::Result<(), SendError<P>> {
            self.aux_sender.send(payload)
        }

        pub(super) fn send_chained_channel(
            &mut self,
            context: &C,
            count: usize,
        ) -> std::result::Result<(), SendError<ChainedChannel<P, C>>> {
            let (chained_sender, chained_receiver) = crossbeam_channel::unbounded();
            let (chained_aux_sender, chained_aux_receiver) = crossbeam_channel::unbounded();
            for _ in 0..count {
                self.sender.send(ChainedChannel::chain_to_new_channel(
                    context.clone(),
                    chained_receiver.clone(),
                    chained_aux_receiver.clone(),
                ))?
            }
            self.sender = chained_sender;
            self.aux_sender = chained_aux_sender;
            Ok(())
        }
    }

    // P doesn't need to be `: Clone`, yet rustc derive can't handle it.
    // see https://github.com/rust-lang/rust/issues/26925
    #[derive_where(Clone)]
    pub(super) struct ChainedChannelReceiver<P, C: Clone> {
        receiver: Receiver<ChainedChannel<P, C>>,
        aux_receiver: Receiver<P>,
        context: C,
    }

    impl<P, C: Clone> ChainedChannelReceiver<P, C> {
        fn new(
            receiver: Receiver<ChainedChannel<P, C>>,
            aux_receiver: Receiver<P>,
            initial_context: C,
        ) -> Self {
            Self {
                receiver,
                aux_receiver,
                context: initial_context,
            }
        }

        pub(super) fn context(&self) -> &C {
            &self.context
        }

        pub(super) fn for_select(&self) -> &Receiver<ChainedChannel<P, C>> {
            &self.receiver
        }

        pub(super) fn aux_for_select(&self) -> &Receiver<P> {
            &self.aux_receiver
        }

        pub(super) fn never_receive_from_aux(&mut self) {
            self.aux_receiver = never();
        }

        pub(super) fn after_select(&mut self, message: ChainedChannel<P, C>) -> Option<P> {
            match message.0 {
                ChainedChannelPrivate::Payload(payload) => Some(payload),
                ChainedChannelPrivate::ContextAndChannels(context, channel, idle_channel) => {
                    self.context = context;
                    self.receiver = channel;
                    self.aux_receiver = idle_channel;
                    None
                }
            }
        }
    }

    pub(super) fn unbounded<P, C: Clone>(
        initial_context: C,
    ) -> (ChainedChannelSender<P, C>, ChainedChannelReceiver<P, C>) {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let (aux_sender, aux_receiver) = crossbeam_channel::unbounded();
        (
            ChainedChannelSender::new(sender, aux_sender),
            ChainedChannelReceiver::new(receiver, aux_receiver, initial_context),
        )
    }
}

/// The primary owner of all [`UsageQueue`]s used for particular [`PooledScheduler`].
///
/// Currently, the simplest implementation. This grows memory usage in unbounded way. Overgrown
/// instance destruction is managed via `solScCleaner`. This struct is here to be put outside
/// `solana-unified-scheduler-logic` for the crate's original intent (separation of concerns from
/// the pure-logic-only crate). Some practical and mundane pruning will be implemented in this type.
#[derive(Default, Debug)]
pub struct UsageQueueLoader {
    usage_queues: DashMap<Pubkey, UsageQueue>,
}

impl UsageQueueLoader {
    pub fn load(&self, address: Pubkey) -> UsageQueue {
        self.usage_queues.entry(address).or_default().clone()
    }

    fn count(&self) -> usize {
        self.usage_queues.len()
    }
}

// (this is slow needing atomic mem reads. However, this can be turned into a lot faster
// optimizer-friendly version as shown in this crossbeam pr:
// https://github.com/crossbeam-rs/crossbeam/pull/1047)
fn disconnected<T>() -> Receiver<T> {
    // drop the sender residing at .0, returning an always-disconnected receiver.
    crossbeam_channel::unbounded().1
}

#[cfg_attr(doc, aquamarine::aquamarine)]
/// The concrete scheduler instance along with 1 scheduler and N handler threads.
///
/// This implements the dyn-compatible [`InstalledScheduler`] trait to be interacted by
/// solana-runtime code as `Box<dyn _>`.  This also implements the [`SpawnableScheduler`] subtrait
/// to be spawned and pooled by [`SchedulerPool`].  When a scheduler is said to be _taken_ from a
/// pool, the Rust's ownership is literally moved from the pool's vec to the particular
/// [`BankWithScheduler`](solana_runtime::installed_scheduler_pool::BankWithScheduler) for
/// type-level protection against double-use by different banks. As soon as the bank is
/// [`is_complete()`](`solana_runtime::bank::Bank::is_complete`) (i.e. ready for freezing), the
/// associated scheduler is immediately _returned_ to the pool via
/// [`InstalledScheduler::wait_for_termination`], to be taken by other banks quickly (usually,
/// child bank).
///
/// Pooling is implemented to avoid repeated thread creation/destruction. Further more, each
/// scheduler should manage its own set of threads, to be independent from other scheduler's
/// threads for concurrent and efficient processing of banks of different forks.
///
/// It's intentionally designed for a start and end of scheduler use by banks not to incur any
/// heavy system resource manipulation to reduce the latency of this per-block bookkeeping as much
/// as possible.
///
/// To complement the above most common situation, there's various erroneous conditions: timeouts
/// and abortions.
///
/// Timeouts are for rare conditions where there are abandoned-yet-unpruned banks in the
/// [`BankForks`](solana_runtime::bank_forks::BankForks) under forky (unsteady rooting) cluster
/// conditions. The pool's background cleaner thread (`solScCleaner`) triggers the timeout-based
/// out-of-pool (i.e. _taken_) scheduler reclaimation with prior coordination of
/// [`BankForks::insert()`](solana_runtime::bank_forks::BankForks::insert) via
/// [`InstalledSchedulerPool::register_timeout_listener`].
///
/// Abortions are for another rate conditions where there's a fatal processing error, marking the
/// given block as dead. In this case, all threads are terminated abruptly as much as possible to
/// avoid any further system resource consumption on this possibly malice block. This error
/// condition can implicitly be signalled to the replay stage on further transaction scheduling or
/// can explicitly be done so on the eventual `wait_for_termination()` by drops or timeouts.
///
/// Lastly, scheduler can finally be _retired_ to be ready for thread termination due to various
/// reasons like [`UsageQueueLoader`] being overgrown or many idling schedulers in the pool, in
/// addition to the obvious reason of aborted scheduler.
///
/// ### Life cycle and ownership movement across crates of a particular scheduler
///
/// ```mermaid
/// stateDiagram-v2
///     [*] --> Active: Spawned (New bank by solReplayStage)
///     state solana-runtime {
///         state if_usable <<choice>>
///         Active --> if_usable: Returned (Bank-freezing by solReplayStage)
///         Active --> if_usable: Dropped (BankForks-pruning by solReplayStage)
///         Aborted --> if_usable: Dropped (BankForks-pruning by solReplayStage)
///         if_usable --> Pooled: IF !overgrown && !aborted
///         Active --> Aborted: Errored on TX execution
///         Aborted --> Stale: !Droppped after TIMEOUT_DURATION since taken
///         Active --> Stale: No new TX after TIMEOUT_DURATION since taken
///         Stale --> if_usable: Returned (Timeout-triggered by solScCleaner)
///         Pooled --> Active: Taken (New bank by solReplayStage)
///     }
///     state solana-unified-scheduler-pool {
///         Pooled --> Idle: !Taken after POOLING_DURATION
///         if_usable --> Trashed: IF overgrown || aborted
///         Idle --> Retired
///         Trashed --> Retired
///     }
///     Retired --> [*]: Terminated (by solScCleaner)
/// ```
#[derive(Debug)]
pub struct PooledScheduler<TH: TaskHandler> {
    inner: PooledSchedulerInner<Self, TH>,
    context: SchedulingContext,
}

#[derive(Debug)]
pub struct PooledSchedulerInner<S: SpawnableScheduler<TH>, TH: TaskHandler> {
    thread_manager: ThreadManager<S, TH>,
    usage_queue_loader: UsageQueueLoader,
}

impl<S, TH> Drop for ThreadManager<S, TH>
where
    S: SpawnableScheduler<TH>,
    TH: TaskHandler,
{
    fn drop(&mut self) {
        trace!("ThreadManager::drop() is called...");

        if self.are_threads_joined() {
            return;
        }
        // If on-stack ThreadManager is being dropped abruptly while panicking, it's likely
        // ::into_inner() isn't called, which is a critical runtime invariant for the following
        // thread shutdown. Also, the state could be corrupt in other ways too, so just skip it
        // altogether.
        if thread::panicking() {
            error!(
                "ThreadManager::drop(): scheduler_id: {} skipping due to already panicking...",
                self.scheduler_id,
            );
            return;
        }

        // assert that this is called after ::into_inner()
        assert_matches!(self.session_result_with_timings, None);

        // Ensure to initiate thread shutdown via disconnected new_task_receiver by replacing the
        // current new_task_sender with a random one...
        self.new_task_sender = crossbeam_channel::unbounded().0;

        self.ensure_join_threads(true);
        assert_matches!(self.session_result_with_timings, Some((Ok(_), _)));
    }
}

impl<S, TH> PooledSchedulerInner<S, TH>
where
    S: SpawnableScheduler<TH>,
    TH: TaskHandler,
{
    fn is_aborted(&self) -> bool {
        // Schedulers can be regarded as being _trashed_ (thereby will be cleaned up later), if
        // threads are joined. Remember that unified scheduler _doesn't normally join threads_ even
        // across different sessions (i.e. different banks) to avoid thread recreation overhead.
        //
        // These unusual thread joining happens after the blocked thread (= the replay stage)'s
        // detection of aborted scheduler thread, which can be interpreted as an immediate signal
        // about the existence of the transaction error.
        //
        // Note that this detection is done internally every time scheduler operations are run
        // (send_task() and end_session(); or schedule_execution() and wait_for_termination() in
        // terms of InstalledScheduler). So, it's ensured that the detection is done at least once
        // for any scheduler which is taken out of the pool.
        //
        // Thus, any transaction errors are always handled without loss of information and
        // the aborted scheduler itself will always be handled as _trashed_ before returning the
        // scheduler to the pool, considering is_aborted() is checked via is_trashed() immediately
        // before that.
        self.thread_manager.are_threads_joined()
    }

    fn is_overgrown(&self) -> bool {
        self.usage_queue_loader.count() > self.thread_manager.pool.max_usage_queue_count
    }
}

// This type manages the OS threads for scheduling and executing transactions. The term
// `session` is consistently used to mean a group of Tasks scoped under a single SchedulingContext.
// This is equivalent to a particular bank for block verification. However, new terms is introduced
// here to mean some continuous time over multiple continuous banks/slots for the block production,
// which is planned to be implemented in the future.
#[derive(Debug)]
struct ThreadManager<S: SpawnableScheduler<TH>, TH: TaskHandler> {
    scheduler_id: SchedulerId,
    pool: Arc<SchedulerPool<S, TH>>,
    new_task_sender: Sender<NewTaskPayload>,
    new_task_receiver: Option<Receiver<NewTaskPayload>>,
    session_result_sender: Sender<ResultWithTimings>,
    session_result_receiver: Receiver<ResultWithTimings>,
    session_result_with_timings: Option<ResultWithTimings>,
    scheduler_thread: Option<JoinHandle<()>>,
    handler_threads: Vec<JoinHandle<()>>,
}

struct HandlerPanicked;
type HandlerResult = std::result::Result<Box<ExecutedTask>, HandlerPanicked>;

impl<S: SpawnableScheduler<TH>, TH: TaskHandler> ThreadManager<S, TH> {
    fn new(pool: Arc<SchedulerPool<S, TH>>) -> Self {
        let (new_task_sender, new_task_receiver) = crossbeam_channel::unbounded();
        let (session_result_sender, session_result_receiver) = crossbeam_channel::unbounded();

        Self {
            scheduler_id: pool.new_scheduler_id(),
            pool,
            new_task_sender,
            new_task_receiver: Some(new_task_receiver),
            session_result_sender,
            session_result_receiver,
            session_result_with_timings: None,
            scheduler_thread: None,
            handler_threads: vec![],
        }
    }

    fn execute_task_with_handler(
        scheduling_context: &SchedulingContext,
        executed_task: &mut Box<ExecutedTask>,
        handler_context: &HandlerContext,
    ) {
        debug!("handling task at {:?}", thread::current());
        TH::handle(
            &mut executed_task.result_with_timings.0,
            &mut executed_task.result_with_timings.1,
            scheduling_context,
            &executed_task.task,
            handler_context,
        );
    }

    fn max_running_task_count(mode: SchedulingMode, handler_count: usize) -> Option<usize> {
        match mode {
            BlockVerification => {
                // Unlike block production, there is no agony for block verification with regards
                // to max_running_task_count. Its responsibility is to execute all transactions by
                // _the pre-determined order_ and no reprioritization or interruption whatsoever.
                // So, just specify no limit and buffer everything as much as possible at the
                // runnable task channel.
                None
            }
            BlockProduction => {
                // Unlike block verification, it's desired for block production to be safely
                // interrupted as soon as possible after session is ending. Thus, don't take
                // unbounded number of non-conflicting tasks out of SchedulingStateMachine, which
                // all needs to be descheduled before finishing session.
                //
                // That said, max_running_task_count shouldn't naively be matched to
                // handler_count (i.e. the number of handler threads). Actually, it should account
                // for some extra queue depth to avoid depletions at the runnable task channel.
                // Otherwise, handler thread could be busy-looping at best or stalled on syscall at
                // worst for the next runnable task to be scheduled by the scheduler thread even
                // though there are actually more runnable tasks at hand in fact. That would
                // significantly hurt concurrency, and eventually throughput. While throughput
                // isn't the primary design target (= low latency) of unified scheduler, this
                // warrants some compromise here. Note that increasing the buffering at crossbeam
                // channels hampers timing sensitivity of task reprioritization on higher-paying
                // transaction arrival at the middle of slot, which is selling point of unified
                // scheduler to recite. This is because there is no efficient way to selectively
                // remove messages from them. Put differently, sent tasks aren't interruptible.
                //
                // So, all in all, we need to strike some nuanced balance here. Currently, this has
                // not rigidly been tested yet; but capping to 2x of handler thread should be
                // enough, because unified scheduler is latency optimized... This means each thread
                // has extra pseudo task queue entry.
                const MAX_RUNNING_TASK_COUNT_FACTOR: usize = 2;

                Some(
                    handler_count
                        .checked_mul(MAX_RUNNING_TASK_COUNT_FACTOR)
                        .unwrap(),
                )
            }
        }
    }

    fn can_receive_unblocked_task(
        session_ending: bool,
        mode: SchedulingMode,
        state_machine: &SchedulingStateMachine,
    ) -> bool {
        match mode {
            BlockVerification => {
                // Always take as much as possible out of SchedulingStateMachine to avoid crossbeam
                // channel internal message buffering depletion with much relaxed condition than
                // block production.
                state_machine.has_unblocked_task()
            }
            BlockProduction => {
                // Much like max_running_task_count() reasonings, stop taking runnable and
                // unblocked tasks out of SchedulingStateMachine as soon as session is ending.
                !session_ending && state_machine.has_runnable_task()
            }
        }
    }

    fn can_finish_session(
        session_ending: bool,
        mode: SchedulingMode,
        state_machine: &SchedulingStateMachine,
    ) -> bool {
        match mode {
            BlockVerification => {
                // It's needed to wait to execute all active tasks without any short-circuiting,
                // even if the session has been signalled for ending; otherwise verification
                // outcome could differ.
                session_ending && state_machine.has_no_active_task()
            }
            BlockProduction => {
                // No need to wait to execute all active tasks unlike block verification. Just wind
                // down all tasks which has already been passed down to handler threads.
                session_ending && state_machine.has_no_running_task()
            }
        }
    }

    /// Returns `true` if the caller should abort.
    #[must_use]
    fn abort_or_accumulate_result_with_timings(
        mode: SchedulingMode,
        (result, timings): &mut ResultWithTimings,
        executed_task: Box<ExecutedTask>,
        state_machine: &mut SchedulingStateMachine,
        session_ending: &mut bool,
        handler_context: &HandlerContext,
    ) -> bool {
        sleepless_testing::at(CheckPoint::TaskAccumulated(
            executed_task.task.task_index(),
            &executed_task.result_with_timings.0,
        ));
        timings.accumulate(&executed_task.result_with_timings.1);

        match mode {
            BlockVerification => match executed_task.result_with_timings.0 {
                Ok(()) => {
                    // The most normal case
                    false
                }
                // This should never be observed because the scheduler thread makes all running
                // tasks are conflict-free
                Err(TransactionError::AccountInUse)
                // These should have been validated by blockstore by now
                | Err(TransactionError::AccountLoadedTwice)
                | Err(TransactionError::TooManyAccountLocks)
                // Block verification should never see this:
                | Err(TransactionError::CommitCancelled) => {
                    unreachable!()
                }
                Err(error) => {
                    error!("error is detected while accumulating....: {error:?}");
                    *result = Err(error);
                    true
                }
            },
            BlockProduction => {
                match executed_task.result_with_timings.0 {
                    Ok(()) => {
                        // The most normal case
                    }
                    Err(TransactionError::CommitCancelled)
                    | Err(TransactionError::WouldExceedMaxBlockCostLimit)
                    | Err(TransactionError::WouldExceedMaxVoteCostLimit)
                    | Err(TransactionError::WouldExceedMaxAccountCostLimit)
                    | Err(TransactionError::WouldExceedAccountDataBlockLimit) => {
                        // Treat these errors as indication of block full signal while retrying the
                        // task at the same time.
                        Self::rebuffer_task_for_next_session(
                            executed_task,
                            state_machine,
                            session_ending,
                            handler_context,
                        );
                    }
                    // This should never be observed because the scheduler thread makes all running
                    // tasks are conflict-free; Furthermore, block producing scheduler should be
                    // prepared for the probable presence of an independent banking thread (like
                    // the jito thread). This is addressed by the TaskHandler::handle().
                    Err(TransactionError::AccountInUse)
                    // These should have been validated by banking_packet_handler by now
                    | Err(TransactionError::AccountLoadedTwice)
                    | Err(TransactionError::TooManyAccountLocks) => {
                        unreachable!();
                    }
                    Err(ref error) => {
                        // The other errors; just discard tasks. These are permanently
                        // not-committable worthless tasks.
                        debug!("error is detected while accumulating....: {error:?}");
                    }
                };
                // Don't abort at all in block production unlike block verification
                false
            }
        }
    }

    fn rebuffer_task_for_next_session(
        executed_task: Box<ExecutedTask>,
        state_machine: &mut SchedulingStateMachine,
        session_ending: &mut bool,
        handler_context: &HandlerContext,
    ) {
        let task = handler_context
            .banking_stage_helper()
            .recreate_task(executed_task);
        state_machine.buffer_task(task);

        // Now, new session is desired, start session_ending.
        sleepless_testing::at(CheckPoint::SessionEnding);
        *session_ending = true;
    }

    fn take_session_result_with_timings(&mut self) -> ResultWithTimings {
        self.session_result_with_timings.take().unwrap()
    }

    fn put_session_result_with_timings(&mut self, result_with_timings: ResultWithTimings) {
        assert_matches!(
            self.session_result_with_timings
                .replace(result_with_timings),
            None
        );
    }

    // This method must take same set of session-related arguments as start_session() to avoid
    // unneeded channel operations to minimize overhead. Starting threads incurs a very high cost
    // already... Also, pre-creating threads isn't desirable as well to avoid `Option`-ed types
    // for type safety.
    fn start_threads(
        &mut self,
        context: SchedulingContext,
        mut result_with_timings: ResultWithTimings,
        handler_context: HandlerContext,
    ) {
        let scheduling_mode = context.mode();
        let mut current_slot = context.slot();
        let (mut is_finished, mut session_ending) = match scheduling_mode {
            BlockVerification => (false, false),
            BlockProduction => {
                assert!(context.is_preallocated());
                (true, true)
            }
        };

        // Firstly, setup bi-directional messaging between the scheduler and handlers to pass
        // around tasks, by creating 2 channels (one for to-be-handled tasks from the scheduler to
        // the handlers and the other for finished tasks from the handlers to the scheduler).
        // Furthermore, this pair of channels is duplicated to work as a primitive 2-level priority
        // queue, totalling 4 channels. Note that the two scheduler-to-handler channels are managed
        // behind chained_channel to avoid race conditions relating to contexts.
        //
        // This quasi-priority-queue arrangement is desired as an optimization to prioritize
        // blocked tasks.
        //
        // As a quick background, SchedulingStateMachine doesn't throttle runnable tasks at all.
        // Thus, it's likely for to-be-handled tasks to be stalled for extended duration due to
        // excessive buffering (commonly known as buffer bloat). Normally, this buffering isn't
        // problematic and actually intentional to fully saturate all the handler threads.
        //
        // However, there's one caveat: task dependencies. It can be hinted with tasks being
        // blocked, that there could be more similarly-blocked tasks in the future. Empirically,
        // clearing these linearized long runs of blocking tasks out of the buffer is delaying bank
        // freezing while only using 1 handler thread or two near the end of slot, deteriorating
        // the overall concurrency.
        //
        // To alleviate the situation, blocked tasks are exchanged via independent communication
        // pathway as a heuristic for expedite processing. Without prioritization of these tasks,
        // progression of clearing these runs would be severely hampered due to interleaved
        // not-blocked tasks (called _idle_ here; typically, voting transactions) in the single
        // buffer.
        //
        // Concurrent priority queue isn't used to avoid penalized throughput due to higher
        // overhead than crossbeam channel, even considering the doubled processing of the
        // crossbeam channel. Fortunately, just 2-level prioritization is enough. Also, sticking to
        // crossbeam was convenient and there was no popular and promising crate for concurrent
        // priority queue as of writing.
        //
        // It's generally harmless for the blocked task buffer to be flooded, stalling the idle
        // tasks completely. Firstly, it's unlikely without malice, considering all blocked tasks
        // must have independently been blocked for each isolated linearized runs. That's because
        // all to-be-handled tasks of the blocked and idle buffers must not be conflicting with
        // each other by definition. Furthermore, handler threads would still be saturated to
        // maximum even under such a block-verification situation, meaning no remotely-controlled
        // performance degradation.
        //
        // Overall, while this is merely a heuristic, it's effective and adaptive while not
        // vulnerable, merely reusing existing information without any additional runtime cost.
        //
        // One known caveat, though, is that this heuristic is employed under a sub-optimal
        // setting, considering scheduling is done in real-time. Namely, prioritization enforcement
        // isn't immediate, in a sense that the first task of a long run is buried in the middle of
        // a large idle task buffer. Prioritization of such a run will be realized only after the
        // first task is handled with the priority of an idle task. To overcome this, some kind of
        // re-prioritization or look-ahead scheduling mechanism would be needed. However, both
        // isn't implemented. The former is due to complex implementation and the later is due to
        // delayed (NOT real-time) processing, which is against the unified scheduler design goal.
        //
        // Alternatively, more faithful prioritization can be realized by checking blocking
        // statuses of all addresses immediately before sending to the handlers. This would prevent
        // false negatives of the heuristics approach (i.e. the last task of a run doesn't need to
        // be handled with the higher priority). Note that this is the only improvement, compared
        // to the heuristics. That's because this underlying information asymmetry between the 2
        // approaches doesn't exist for all other cases, assuming no look-ahead: idle tasks are
        // always unblocked by definition, and other blocked tasks should always be calculated as
        // blocked by the very existence of the last blocked task.
        //
        // The faithful approach incurs a considerable overhead: O(N), where N is the number of
        // locked addresses in a task, adding to the current bare-minimum complexity of O(2*N) for
        // both scheduling and descheduling. This means 1.5x increase. Furthermore, this doesn't
        // nicely work in practice with a real-time streamed scheduler. That's because these
        // linearized runs could be intermittent in the view with little or no look-back, albeit
        // actually forming a far more longer runs in longer time span. These access patterns are
        // very common, considering existence of well-known hot accounts.
        //
        // Thus, intentionally allowing these false-positives by the heuristic approach is actually
        // helping to extend the logical prioritization session for the invisible longer runs, as
        // long as the last task of the current run is being handled by the handlers, hoping yet
        // another blocking new task is arriving to finalize the tentatively extended
        // prioritization further. Consequently, this also contributes to alleviate the known
        // heuristic's caveat for the first task of linearized runs, which is described above.
        let (mut runnable_task_sender, runnable_task_receiver) =
            chained_channel::unbounded::<Task, SchedulingContext>(context);
        // Create two handler-to-scheduler channels to prioritize the finishing of blocked tasks,
        // because it is more likely that a blocked task will have more blocked tasks behind it,
        // which should be scheduled while minimizing the delay to clear buffered linearized runs
        // as fast as possible.
        let (finished_blocked_task_sender, finished_blocked_task_receiver) =
            crossbeam_channel::unbounded::<HandlerResult>();
        let (finished_idle_task_sender, finished_idle_task_receiver) =
            crossbeam_channel::unbounded::<HandlerResult>();

        assert_matches!(self.session_result_with_timings, None);

        // High-level flow of new tasks:
        // 1. the replay stage thread send a new task.
        // 2. the scheduler thread accepts the task.
        // 3. the scheduler thread dispatches the task after proper locking.
        // 4. the handler thread processes the dispatched task.
        // 5. the handler thread reply back to the scheduler thread as an executed task.
        // 6. the scheduler thread post-processes the executed task.
        let scheduler_main_loop = {
            let handler_context = handler_context.clone();
            let handler_count = self.pool.handler_count;
            let session_result_sender = self.session_result_sender.clone();
            // Taking new_task_receiver here is important to ensure there's a single receiver. In
            // this way, the replay stage will get .send() failures reliably, after this scheduler
            // thread died along with the single receiver.
            let new_task_receiver = self
                .new_task_receiver
                .take()
                .expect("no 2nd start_threads()");

            // Now, this is the main loop for the scheduler thread, which is a special beast.
            //
            // That's because it could be the most notable bottleneck of throughput in the future
            // when there are ~100 handler threads. Unified scheduler's overall throughput is
            // largely dependant on its ultra-low latency characteristic, which is the most
            // important design goal of the scheduler in order to reduce the transaction
            // confirmation latency for end users.
            //
            // Firstly, the scheduler thread must handle incoming messages from thread(s) owned by
            // the replay stage or the banking stage. It also must handle incoming messages from
            // the multi-threaded handlers. This heavily-multi-threaded whole processing load must
            // be coped just with the single-threaded scheduler, to attain ideal cpu cache
            // friendliness and main memory bandwidth saturation with its shared-nothing
            // single-threaded account locking implementation. In other words, the per-task
            // processing efficiency of the main loop codifies the upper bound of horizontal
            // scalability of the unified scheduler.
            //
            // Moreover, the scheduler is designed to handle tasks without batching at all in the
            // pursuit of saturating all of the handler threads with maximally-fine-grained
            // concurrency density for throughput as the second design goal. This design goal
            // relies on the assumption that there's no considerable penalty arising from the
            // unbatched manner of processing.
            //
            // Note that this assumption isn't true as of writing. The current code path
            // underneath execute_batch() isn't optimized for unified scheduler's load pattern (ie.
            // batches just with a single transaction) at all. This will be addressed in the
            // future.
            //
            // These two key elements of the design philosophy lead to the rather unforgiving
            // implementation burden: Degraded performance would acutely manifest from an even tiny
            // amount of individual cpu-bound processing delay in the scheduler thread, like when
            // dispatching the next conflicting task after receiving the previous finished one from
            // the handler.
            //
            // Thus, it's fatal for unified scheduler's advertised superiority to squeeze every cpu
            // cycles out of the scheduler thread. Thus, any kinds of unessential overhead sources
            // like syscalls, VDSO, and even memory (de)allocation should be avoided at all costs
            // by design or by means of offloading at the last resort.
            move || {
                let (do_now, dont_now) = (&disconnected::<()>(), &never::<()>());
                let dummy_receiver = |trigger| {
                    if trigger {
                        do_now
                    } else {
                        dont_now
                    }
                };

                let mut state_machine = unsafe {
                    SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling(
                        Self::max_running_task_count(scheduling_mode, handler_count),
                    )
                };

                // The following loop maintains and updates ResultWithTimings as its
                // externally-provided mutable state for each session in this way:
                //
                // 1. Initial result_with_timing is propagated implicitly by the moved variable.
                // 2. Subsequent result_with_timings are propagated explicitly from
                //    the new_task_receiver.recv() invocation located at the end of loop.
                'nonaborted_main_loop: loop {
                    while !is_finished {
                        // ALL recv selectors are eager-evaluated ALWAYS by current crossbeam impl,
                        // which isn't great and is inconsistent with `if`s in the Rust's match
                        // arm. So, eagerly binding the result to a variable unconditionally here
                        // makes no perf. difference...
                        let dummy_unblocked_task_receiver =
                            dummy_receiver(Self::can_receive_unblocked_task(
                                session_ending,
                                scheduling_mode,
                                &state_machine,
                            ));

                        // There's something special called dummy_unblocked_task_receiver here.
                        // This odd pattern was needed to react to newly unblocked tasks from
                        // _not-crossbeam-channel_ event sources, precisely at the specified
                        // precedence among other selectors, while delegating the control flow to
                        // select_biased!.
                        //
                        // In this way, hot looping is avoided and overall control flow is much
                        // consistent. Note that unified scheduler will go
                        // into busy looping to seek lowest latency eventually. However, not now,
                        // to measure _actual_ cpu usage easily with the select approach.
                        select_biased! {
                            recv(finished_blocked_task_receiver) -> receiver_result => {
                                let handler_result = receiver_result.expect("alive handler");
                                let Ok(executed_task) = handler_result else {
                                    break 'nonaborted_main_loop;
                                };
                                state_machine.deschedule_task(&executed_task.task);

                                if Self::abort_or_accumulate_result_with_timings(
                                    scheduling_mode,
                                    &mut result_with_timings,
                                    executed_task,
                                    &mut state_machine,
                                    &mut session_ending,
                                    &handler_context,
                                ) {
                                    break 'nonaborted_main_loop;
                                }
                            },
                            recv(dummy_unblocked_task_receiver) -> dummy => {
                                assert_matches!(dummy, Err(RecvError));

                                let task = state_machine
                                    .schedule_next_unblocked_task()
                                    .expect("unblocked task");
                                runnable_task_sender.send_payload(task).unwrap();
                            },
                            recv(new_task_receiver) -> message => {
                                assert!(scheduling_mode == BlockProduction || !session_ending);

                                match message {
                                    Ok(NewTaskPayload::Payload(task)) => {
                                        let task_index = task.task_index();
                                        sleepless_testing::at(CheckPoint::NewTask(task_index));

                                        if let Some(task) = state_machine.schedule_or_buffer_task(task, session_ending) {
                                            runnable_task_sender.send_aux_payload(task).unwrap();
                                        } else {
                                            sleepless_testing::at(CheckPoint::BufferedTask(task_index));
                                        }
                                    }
                                    Ok(NewTaskPayload::CloseSubchannel) => {
                                        sleepless_testing::at(CheckPoint::SessionEnding);
                                        session_ending = true;
                                    }
                                    Ok(NewTaskPayload::OpenSubchannel(_context_and_result_with_timings)) =>
                                        unreachable!(),
                                    Err(RecvError) => {
                                        // Mostly likely is that this scheduler is dropped for pruned blocks of
                                        // abandoned forks...
                                        // This short-circuiting is tested with test_scheduler_drop_short_circuiting.
                                        break 'nonaborted_main_loop;
                                    }
                                }
                            },
                            recv(finished_idle_task_receiver) -> receiver_result => {
                                let handler_result = receiver_result.expect("alive handler");
                                let Ok(executed_task) = handler_result else {
                                    break 'nonaborted_main_loop;
                                };
                                state_machine.deschedule_task(&executed_task.task);

                                if Self::abort_or_accumulate_result_with_timings(
                                    scheduling_mode,
                                    &mut result_with_timings,
                                    executed_task,
                                    &mut state_machine,
                                    &mut session_ending,
                                    &handler_context,
                                ) {
                                    break 'nonaborted_main_loop;
                                }
                            },
                        };

                        is_finished = Self::can_finish_session(
                            session_ending,
                            scheduling_mode,
                            &state_machine,
                        );
                    }
                    assert!(mem::replace(&mut is_finished, false));

                    sleepless_testing::at(CheckPoint::SessionFinished(current_slot));
                    // Finalize the current session after asserting it's explicitly requested so.
                    // Send result first because this is blocking the replay code-path.
                    session_result_sender
                        .send(result_with_timings)
                        .expect("always outlived receiver");
                    if matches!(scheduling_mode, BlockVerification) {
                        state_machine.reinitialize();
                    }
                    assert!(mem::replace(&mut session_ending, false));

                    loop {
                        // Prepare for the new session.
                        match new_task_receiver.recv() {
                            Ok(NewTaskPayload::Payload(task)) => {
                                sleepless_testing::at(CheckPoint::NewBufferedTask(
                                    task.task_index(),
                                ));
                                assert_eq!(scheduling_mode, BlockProduction);
                                state_machine.buffer_task(task);
                            }
                            Ok(NewTaskPayload::OpenSubchannel(context_and_result_with_timings)) => {
                                let (new_context, new_result_with_timings) =
                                    *context_and_result_with_timings;
                                // We just received subsequent (= not initial) session and about to
                                // enter into the preceding `while(!is_finished) {...}` loop again.
                                // Before that, propagate new SchedulingContext to handler threads
                                assert_eq!(scheduling_mode, new_context.mode());
                                assert!(!new_context.is_preallocated());
                                current_slot = new_context.slot();
                                runnable_task_sender
                                    .send_chained_channel(&new_context, handler_count)
                                    .unwrap();
                                result_with_timings = new_result_with_timings;
                                break;
                            }
                            Ok(NewTaskPayload::CloseSubchannel) => {
                                // This match arm can be hit if context.is_preallocated()
                            }
                            Err(_) => {
                                // This unusual condition must be triggered by ThreadManager::drop().
                                // Initialize result_with_timings with a harmless value...
                                result_with_timings = initialized_result_with_timings();
                                break 'nonaborted_main_loop;
                            }
                        }
                    }
                }

                // There are several code-path reaching here out of the preceding unconditional
                // `loop { ... }` by the use of `break 'nonaborted_main_loop;`. This scheduler
                // thread will now initiate the termination process, indicating an abnormal abortion,
                // in order to be handled gracefully by other threads.

                // Firstly, send result_with_timings as-is, because it's expected for us to put the
                // last result_with_timings into the channel without exception. Usually,
                // result_with_timings will contain the Err variant at this point, indicating the
                // occurrence of transaction error.
                session_result_sender
                    .send(result_with_timings)
                    .expect("always outlived receiver");

                // Next, drop `new_task_receiver`. After that, the paired singleton
                // `new_task_sender` will start to error when called by external threads, resulting
                // in propagation of thread abortion to the external threads.
                drop(new_task_receiver);

                // We will now exit this thread finally... Good bye.
                sleepless_testing::at(CheckPoint::SchedulerThreadAborted);
            }
        };

        let handler_main_loop = || {
            let mut handler_context = handler_context.clone();
            let mut runnable_task_receiver = runnable_task_receiver.clone();
            let finished_blocked_task_sender = finished_blocked_task_sender.clone();
            let finished_idle_task_sender = finished_idle_task_sender.clone();

            // The following loop maintains and updates SchedulingContext as its
            // externally-provided state for each session in this way:
            //
            // 1. Initial context is propagated implicitly by the moved runnable_task_receiver,
            //    which is clone()-d just above for this particular thread.
            // 2. Subsequent contexts are propagated explicitly inside `.after_select()` as part of
            //    `select_biased!`, which are sent from `.send_chained_channel()` in the scheduler
            //    thread for all-but-initial sessions.
            move || {
                loop {
                    let (task, sender) = select_biased! {
                        recv(runnable_task_receiver.for_select()) -> message => {
                            let Ok(message) = message else {
                                break;
                            };
                            if let Some(task) = runnable_task_receiver.after_select(message) {
                                (task, &finished_blocked_task_sender)
                            } else {
                                continue;
                            }
                        },
                        recv(runnable_task_receiver.aux_for_select()) -> task => {
                            if let Ok(task) = task {
                                (task, &finished_idle_task_sender)
                            } else {
                                runnable_task_receiver.never_receive_from_aux();
                                continue;
                            }
                        },
                        recv(handler_context.banking_packet_receiver) -> banking_packet => {
                            let HandlerContext {banking_packet_handler, banking_stage_helper, ..} = &mut handler_context;
                            let banking_stage_helper = banking_stage_helper.as_ref().unwrap();

                            // See solana_core::banking_stage::unified_scheduler module doc as to
                            // justification of this additional work in the handler thread.
                            let Ok(banking_packet) = banking_packet else {
                                info!("disconnected banking_packet_receiver");
                                break;
                            };
                            banking_packet_handler(banking_stage_helper, banking_packet);
                            continue;
                        },
                    };
                    defer! {
                        if !thread::panicking() {
                            return;
                        }

                        // The scheduler thread can't detect panics in handler threads with
                        // disconnected channel errors, unless all of them has died. So, send an
                        // explicit Err promptly.
                        let current_thread = thread::current();
                        error!("handler thread is panicking: {:?}", current_thread);
                        if sender.send(Err(HandlerPanicked)).is_ok() {
                            info!("notified a panic from {:?}", current_thread);
                        } else {
                            // It seems that the scheduler thread has been aborted already...
                            warn!("failed to notify a panic from {:?}", current_thread);
                        }
                    }
                    let mut task = ExecutedTask::new_boxed(task);
                    Self::execute_task_with_handler(
                        runnable_task_receiver.context(),
                        &mut task,
                        &handler_context,
                    );
                    if sender.send(Ok(task)).is_err() {
                        warn!("handler_thread: scheduler thread aborted...");
                        break;
                    }
                }
            }
        };

        self.scheduler_thread = Some(
            thread::Builder::new()
                .name("solScheduler".to_owned())
                .spawn_tracked(scheduler_main_loop)
                .unwrap(),
        );

        self.handler_threads = (0..self.pool.handler_count)
            .map({
                |thx| {
                    thread::Builder::new()
                        .name(format!("solScHandler{:02}", thx))
                        .spawn_tracked(handler_main_loop())
                        .unwrap()
                }
            })
            .collect();
    }

    fn send_task(&self, task: Task) -> ScheduleResult {
        debug!("send_task()");
        self.new_task_sender
            .send(NewTaskPayload::Payload(task))
            .map_err(|_| SchedulerAborted)
    }

    fn ensure_join_threads(&mut self, should_receive_session_result: bool) {
        trace!("ensure_join_threads() is called");

        fn join_with_panic_message(join_handle: JoinHandle<()>) -> thread::Result<()> {
            let thread = join_handle.thread().clone();
            join_handle.join().inspect_err(|e| {
                // Always needs to try both types for .downcast_ref(), according to
                // https://doc.rust-lang.org/1.78.0/std/macro.panic.html:
                //   a panic can be accessed as a &dyn Any + Send, which contains either a &str or
                //   String for regular panic!() invocations. (Whether a particular invocation
                //   contains the payload at type &str or String is unspecified and can change.)
                let panic_message = match (e.downcast_ref::<&str>(), e.downcast_ref::<String>()) {
                    (Some(&s), _) => s,
                    (_, Some(s)) => s,
                    (None, None) => "<No panic info>",
                };
                panic!("{} (From: {:?})", panic_message, thread);
            })
        }

        if let Some(scheduler_thread) = self.scheduler_thread.take() {
            for thread in self.handler_threads.drain(..) {
                debug!("joining...: {:?}", thread);
                () = join_with_panic_message(thread).unwrap();
            }
            () = join_with_panic_message(scheduler_thread).unwrap();

            if should_receive_session_result {
                let result_with_timings = self.session_result_receiver.recv().unwrap();
                debug!("ensure_join_threads(): err: {:?}", result_with_timings.0);
                self.put_session_result_with_timings(result_with_timings);
            }
        } else {
            warn!("ensure_join_threads(): skipping; already joined...");
        };
    }

    fn ensure_join_threads_after_abort(
        &mut self,
        should_receive_aborted_session_result: bool,
    ) -> TransactionError {
        self.ensure_join_threads(should_receive_aborted_session_result);
        self.session_result_with_timings
            .as_mut()
            .unwrap()
            .0
            .clone()
            .unwrap_err()
    }

    fn are_threads_joined(&self) -> bool {
        if self.scheduler_thread.is_none() {
            // Emptying handler_threads must be an atomic operation with scheduler_thread being
            // taken.
            assert!(self.handler_threads.is_empty());
            true
        } else {
            false
        }
    }

    fn end_session(&mut self) {
        if self.are_threads_joined() {
            assert!(self.session_result_with_timings.is_some());
            debug!("end_session(): skipping; already joined the aborted threads..");
            return;
        } else if self.session_result_with_timings.is_some() {
            debug!("end_session(): skipping; already result resides within thread manager..");
            return;
        }
        debug!(
            "end_session(): will end session at {:?}...",
            thread::current(),
        );

        let mut abort_detected = self
            .new_task_sender
            .send(NewTaskPayload::CloseSubchannel)
            .is_err();

        if abort_detected {
            self.ensure_join_threads_after_abort(true);
            return;
        }

        // Even if abort is detected, it's guaranteed that the scheduler thread puts the last
        // message into the session_result_sender before terminating.
        let result_with_timings = self.session_result_receiver.recv().unwrap();
        abort_detected = result_with_timings.0.is_err();
        self.put_session_result_with_timings(result_with_timings);
        if abort_detected {
            self.ensure_join_threads_after_abort(false);
        }
        debug!("end_session(): ended session at {:?}...", thread::current());
    }

    fn start_session(
        &mut self,
        context: SchedulingContext,
        result_with_timings: ResultWithTimings,
    ) {
        assert!(!self.are_threads_joined());
        assert_matches!(self.session_result_with_timings, None);
        self.new_task_sender
            .send(NewTaskPayload::OpenSubchannel(Box::new((
                context,
                result_with_timings,
            ))))
            .expect("no new session after aborted");
    }
}

pub trait SchedulerInner {
    fn id(&self) -> SchedulerId;
    fn is_trashed(&self) -> bool;
}

pub trait SpawnableScheduler<TH: TaskHandler>: InstalledScheduler {
    type Inner: SchedulerInner + Debug + Send + Sync;

    fn into_inner(self) -> (ResultWithTimings, Self::Inner);

    fn from_inner(
        inner: Self::Inner,
        context: SchedulingContext,
        result_with_timings: ResultWithTimings,
    ) -> Self;

    fn spawn(
        pool: Arc<SchedulerPool<Self, TH>>,
        context: SchedulingContext,
        result_with_timings: ResultWithTimings,
    ) -> Self
    where
        Self: Sized;
}

impl<TH: TaskHandler> SpawnableScheduler<TH> for PooledScheduler<TH> {
    type Inner = PooledSchedulerInner<Self, TH>;

    fn into_inner(mut self) -> (ResultWithTimings, Self::Inner) {
        let result_with_timings = {
            let manager = &mut self.inner.thread_manager;
            manager.end_session();
            manager.take_session_result_with_timings()
        };
        (result_with_timings, self.inner)
    }

    fn from_inner(
        mut inner: Self::Inner,
        context: SchedulingContext,
        result_with_timings: ResultWithTimings,
    ) -> Self {
        inner
            .thread_manager
            .start_session(context.clone(), result_with_timings);
        Self { inner, context }
    }

    fn spawn(
        pool: Arc<SchedulerPool<Self, TH>>,
        context: SchedulingContext,
        result_with_timings: ResultWithTimings,
    ) -> Self {
        let mut inner = Self::Inner {
            thread_manager: ThreadManager::new(pool.clone()),
            usage_queue_loader: UsageQueueLoader::default(),
        };
        inner.thread_manager.start_threads(
            context.clone(),
            result_with_timings,
            pool.create_handler_context(context.mode(), &inner.thread_manager.new_task_sender),
        );
        Self { inner, context }
    }
}

impl<TH: TaskHandler> InstalledScheduler for PooledScheduler<TH> {
    fn id(&self) -> SchedulerId {
        self.inner.id()
    }

    fn context(&self) -> &SchedulingContext {
        &self.context
    }

    fn schedule_execution(
        &self,
        transaction: RuntimeTransaction<SanitizedTransaction>,
        index: usize,
    ) -> ScheduleResult {
        let task = SchedulingStateMachine::create_task(transaction, index, &mut |pubkey| {
            self.inner.usage_queue_loader.load(pubkey)
        });
        self.inner.thread_manager.send_task(task)
    }

    fn recover_error_after_abort(&mut self) -> TransactionError {
        self.inner
            .thread_manager
            .ensure_join_threads_after_abort(true)
    }

    fn wait_for_termination(
        self: Box<Self>,
        _is_dropped: bool,
    ) -> (ResultWithTimings, UninstalledSchedulerBox) {
        let (result_with_timings, uninstalled_scheduler) = self.into_inner();
        (result_with_timings, Box::new(uninstalled_scheduler))
    }

    fn pause_for_recent_blockhash(&mut self) {
        self.inner.thread_manager.end_session();
    }
}

impl<S, TH> SchedulerInner for PooledSchedulerInner<S, TH>
where
    S: SpawnableScheduler<TH>,
    TH: TaskHandler,
{
    fn id(&self) -> SchedulerId {
        self.thread_manager.scheduler_id
    }

    fn is_trashed(&self) -> bool {
        self.is_aborted() || self.is_overgrown()
    }
}

impl<S, TH> UninstalledScheduler for PooledSchedulerInner<S, TH>
where
    S: SpawnableScheduler<TH, Inner = Self>,
    TH: TaskHandler,
{
    fn return_to_pool(self: Box<Self>) {
        self.thread_manager.pool.clone().return_scheduler(*self);
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::sleepless_testing,
        assert_matches::assert_matches,
        solana_clock::{Slot, MAX_PROCESSING_AGE},
        solana_hash::Hash,
        solana_keypair::Keypair,
        solana_ledger::{
            blockstore::Blockstore,
            blockstore_processor::{TransactionStatusBatch, TransactionStatusMessage},
            create_new_tmp_ledger_auto_delete,
            leader_schedule_cache::LeaderScheduleCache,
        },
        solana_poh::poh_recorder::create_test_recorder_with_index_tracking,
        solana_pubkey::Pubkey,
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{create_genesis_config, GenesisConfigInfo},
            installed_scheduler_pool::{BankWithScheduler, SchedulingContext},
            prioritization_fee_cache::PrioritizationFeeCache,
        },
        solana_system_transaction as system_transaction,
        solana_timings::ExecuteTimingType,
        solana_transaction::sanitized::SanitizedTransaction,
        solana_transaction_error::TransactionError,
        std::{
            num::Saturating,
            sync::{atomic::Ordering, Arc, RwLock},
            thread::JoinHandle,
        },
        test_case::test_matrix,
    };

    #[derive(Debug)]
    enum TestCheckPoint {
        BeforeNewTask,
        AfterNewBufferedTask,
        AfterBufferedTask,
        AfterTaskHandled,
        AfterTaskAccumulated,
        AfterSessionEnding,
        AfterSessionFinished,
        AfterSchedulerThreadAborted,
        BeforeIdleSchedulerCleaned,
        AfterIdleSchedulerCleaned,
        BeforeTrashedSchedulerCleaned,
        AfterTrashedSchedulerCleaned,
        BeforeTimeoutListenerTriggered,
        AfterTimeoutListenerTriggered,
        BeforeThreadManagerDrop,
        BeforeEndSession,
        AfterSession,
    }

    #[test]
    fn test_scheduler_pool_new() {
        solana_logger::setup();

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new_dyn(None, None, None, None, ignored_prioritization_fee_cache);

        // this indirectly proves that there should be circular link because there's only one Arc
        // at this moment now
        // the 2 weaks are for the weak_self field and the pool cleaner thread.
        assert_eq!((Arc::strong_count(&pool), Arc::weak_count(&pool)), (1, 2));
        let debug = format!("{pool:#?}");
        assert!(!debug.is_empty());
    }

    #[test]
    fn test_scheduler_spawn() {
        solana_logger::setup();

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new_dyn(None, None, None, None, ignored_prioritization_fee_cache);
        let bank = Arc::new(Bank::default_for_tests());
        let context = SchedulingContext::for_verification(bank);
        let scheduler = pool.take_scheduler(context);

        let debug = format!("{scheduler:#?}");
        assert!(!debug.is_empty());
    }

    const SHORTENED_POOL_CLEANER_INTERVAL: Duration = Duration::from_millis(1);
    const SHORTENED_MAX_POOLING_DURATION: Duration = Duration::from_millis(100);

    #[test]
    fn test_scheduler_drop_idle() {
        solana_logger::setup();

        let _progress = sleepless_testing::setup(&[
            &TestCheckPoint::BeforeIdleSchedulerCleaned,
            &CheckPoint::IdleSchedulerCleaned(0),
            &CheckPoint::IdleSchedulerCleaned(1),
            &TestCheckPoint::AfterIdleSchedulerCleaned,
        ]);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool_raw = DefaultSchedulerPool::do_new(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
            SHORTENED_POOL_CLEANER_INTERVAL,
            SHORTENED_MAX_POOLING_DURATION,
            DEFAULT_MAX_USAGE_QUEUE_COUNT,
            DEFAULT_TIMEOUT_DURATION,
        );
        let pool = pool_raw.clone();
        let bank = Arc::new(Bank::default_for_tests());
        let context1 = SchedulingContext::for_verification(bank);
        let context2 = context1.clone();

        let old_scheduler = pool.do_take_scheduler(context1);
        let new_scheduler = pool.do_take_scheduler(context2);
        let new_scheduler_id = new_scheduler.id();
        Box::new(old_scheduler.into_inner().1).return_to_pool();

        // sleepless_testing can't be used; wait a bit here to see real progress of wall time...
        sleep(SHORTENED_MAX_POOLING_DURATION * 10);
        Box::new(new_scheduler.into_inner().1).return_to_pool();

        // Block solScCleaner until we see returned schedlers...
        assert_eq!(pool_raw.scheduler_inners.lock().unwrap().len(), 2);
        sleepless_testing::at(TestCheckPoint::BeforeIdleSchedulerCleaned);

        // See the old (= idle) scheduler gone only after solScCleaner did its job...
        sleepless_testing::at(&TestCheckPoint::AfterIdleSchedulerCleaned);

        // The following assertion is racy.
        //
        // We need to make sure new_scheduler isn't treated as idle up to now since being returned
        // to the pool after sleep(SHORTENED_MAX_POOLING_DURATION * 10).
        // Removing only old_scheduler is the expected behavior. So, make
        // SHORTENED_MAX_POOLING_DURATION rather long...
        assert_eq!(pool_raw.scheduler_inners.lock().unwrap().len(), 1);
        assert_eq!(
            pool_raw
                .scheduler_inners
                .lock()
                .unwrap()
                .first()
                .as_ref()
                .map(|(inner, _pooled_at)| inner.id())
                .unwrap(),
            new_scheduler_id
        );
    }

    #[test]
    fn test_scheduler_drop_overgrown() {
        solana_logger::setup();

        let _progress = sleepless_testing::setup(&[
            &TestCheckPoint::BeforeTrashedSchedulerCleaned,
            &CheckPoint::TrashedSchedulerCleaned(0),
            &CheckPoint::TrashedSchedulerCleaned(1),
            &TestCheckPoint::AfterTrashedSchedulerCleaned,
        ]);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        const REDUCED_MAX_USAGE_QUEUE_COUNT: usize = 1;
        let pool_raw = DefaultSchedulerPool::do_new(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
            SHORTENED_POOL_CLEANER_INTERVAL,
            DEFAULT_MAX_POOLING_DURATION,
            REDUCED_MAX_USAGE_QUEUE_COUNT,
            DEFAULT_TIMEOUT_DURATION,
        );
        let pool = pool_raw.clone();
        let bank = Arc::new(Bank::default_for_tests());
        let context1 = SchedulingContext::for_verification(bank);
        let context2 = context1.clone();

        let small_scheduler = pool.do_take_scheduler(context1);
        let small_scheduler_id = small_scheduler.id();
        for _ in 0..REDUCED_MAX_USAGE_QUEUE_COUNT {
            small_scheduler
                .inner
                .usage_queue_loader
                .load(Pubkey::new_unique());
        }
        let big_scheduler = pool.do_take_scheduler(context2);
        for _ in 0..REDUCED_MAX_USAGE_QUEUE_COUNT + 1 {
            big_scheduler
                .inner
                .usage_queue_loader
                .load(Pubkey::new_unique());
        }

        assert_eq!(pool_raw.scheduler_inners.lock().unwrap().len(), 0);
        assert_eq!(pool_raw.trashed_scheduler_inners.lock().unwrap().len(), 0);
        Box::new(small_scheduler.into_inner().1).return_to_pool();
        Box::new(big_scheduler.into_inner().1).return_to_pool();

        // Block solScCleaner until we see trashed schedler...
        assert_eq!(pool_raw.scheduler_inners.lock().unwrap().len(), 1);
        assert_eq!(pool_raw.trashed_scheduler_inners.lock().unwrap().len(), 1);
        sleepless_testing::at(TestCheckPoint::BeforeTrashedSchedulerCleaned);

        // See the trashed scheduler gone only after solScCleaner did its job...
        sleepless_testing::at(&TestCheckPoint::AfterTrashedSchedulerCleaned);
        assert_eq!(pool_raw.scheduler_inners.lock().unwrap().len(), 1);
        assert_eq!(pool_raw.trashed_scheduler_inners.lock().unwrap().len(), 0);
        assert_eq!(
            pool_raw
                .scheduler_inners
                .lock()
                .unwrap()
                .first()
                .as_ref()
                .map(|(inner, _pooled_at)| inner.id())
                .unwrap(),
            small_scheduler_id
        );
    }

    const SHORTENED_TIMEOUT_DURATION: Duration = Duration::from_millis(1);

    #[test]
    fn test_scheduler_drop_stale() {
        solana_logger::setup();

        let _progress = sleepless_testing::setup(&[
            &TestCheckPoint::BeforeTimeoutListenerTriggered,
            &CheckPoint::TimeoutListenerTriggered(0),
            &CheckPoint::TimeoutListenerTriggered(1),
            &TestCheckPoint::AfterTimeoutListenerTriggered,
            &CheckPoint::IdleSchedulerCleaned(1),
            &TestCheckPoint::AfterIdleSchedulerCleaned,
        ]);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool_raw = DefaultSchedulerPool::do_new(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
            SHORTENED_POOL_CLEANER_INTERVAL,
            SHORTENED_MAX_POOLING_DURATION,
            DEFAULT_MAX_USAGE_QUEUE_COUNT,
            SHORTENED_TIMEOUT_DURATION,
        );
        let pool = pool_raw.clone();
        let bank = Arc::new(Bank::default_for_tests());
        let context = SchedulingContext::for_verification(bank.clone());
        let scheduler = pool.take_scheduler(context);
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        pool.register_timeout_listener(bank.create_timeout_listener());
        assert_eq!(pool_raw.scheduler_inners.lock().unwrap().len(), 0);
        assert_eq!(pool_raw.trashed_scheduler_inners.lock().unwrap().len(), 0);
        sleepless_testing::at(TestCheckPoint::BeforeTimeoutListenerTriggered);

        sleepless_testing::at(TestCheckPoint::AfterTimeoutListenerTriggered);
        assert_eq!(pool_raw.scheduler_inners.lock().unwrap().len(), 1);
        assert_eq!(pool_raw.trashed_scheduler_inners.lock().unwrap().len(), 0);
        assert_matches!(bank.wait_for_completed_scheduler(), Some((Ok(()), _)));

        // See the stale scheduler gone only after solScCleaner did its job...
        sleepless_testing::at(&TestCheckPoint::AfterIdleSchedulerCleaned);
        assert_eq!(pool_raw.scheduler_inners.lock().unwrap().len(), 0);
        assert_eq!(pool_raw.trashed_scheduler_inners.lock().unwrap().len(), 0);
    }

    #[test]
    fn test_scheduler_active_after_stale() {
        solana_logger::setup();

        let _progress = sleepless_testing::setup(&[
            &TestCheckPoint::BeforeTimeoutListenerTriggered,
            &CheckPoint::TimeoutListenerTriggered(0),
            &CheckPoint::TimeoutListenerTriggered(1),
            &TestCheckPoint::AfterTimeoutListenerTriggered,
            &TestCheckPoint::BeforeTimeoutListenerTriggered,
            &CheckPoint::TimeoutListenerTriggered(0),
            &CheckPoint::TimeoutListenerTriggered(1),
            &TestCheckPoint::AfterTimeoutListenerTriggered,
        ]);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool_raw = SchedulerPool::<PooledScheduler<ExecuteTimingCounter>, _>::do_new(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
            SHORTENED_POOL_CLEANER_INTERVAL,
            DEFAULT_MAX_POOLING_DURATION,
            DEFAULT_MAX_USAGE_QUEUE_COUNT,
            SHORTENED_TIMEOUT_DURATION,
        );

        #[derive(Debug)]
        struct ExecuteTimingCounter;
        impl TaskHandler for ExecuteTimingCounter {
            fn handle(
                _result: &mut Result<()>,
                timings: &mut ExecuteTimings,
                _bank: &SchedulingContext,
                _task: &Task,
                _handler_context: &HandlerContext,
            ) {
                timings.metrics[ExecuteTimingType::CheckUs] += 123;
            }
        }
        let pool = pool_raw.clone();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);

        let context = SchedulingContext::for_verification(bank.clone());

        let scheduler = pool.take_scheduler(context);
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        pool.register_timeout_listener(bank.create_timeout_listener());

        let tx_before_stale =
            RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_pubkey::new_rand(),
                2,
                genesis_config.hash(),
            ));
        bank.schedule_transaction_executions([(tx_before_stale, 0)].into_iter())
            .unwrap();
        sleepless_testing::at(TestCheckPoint::BeforeTimeoutListenerTriggered);

        sleepless_testing::at(TestCheckPoint::AfterTimeoutListenerTriggered);
        let tx_after_stale =
            RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_pubkey::new_rand(),
                2,
                genesis_config.hash(),
            ));
        bank.schedule_transaction_executions([(tx_after_stale, 1)].into_iter())
            .unwrap();

        // Observe second occurrence of TimeoutListenerTriggered(1), which indicates a new timeout
        // lister is registered correctly again for reactivated scheduler.
        sleepless_testing::at(TestCheckPoint::BeforeTimeoutListenerTriggered);
        sleepless_testing::at(TestCheckPoint::AfterTimeoutListenerTriggered);

        let (result, timings) = bank.wait_for_completed_scheduler().unwrap();
        assert_matches!(result, Ok(()));
        // ResultWithTimings should be carried over across active=>stale=>active transitions.
        assert_eq!(timings.metrics[ExecuteTimingType::CheckUs].0, 246);
    }

    #[test]
    fn test_scheduler_pause_after_stale() {
        solana_logger::setup();

        let _progress = sleepless_testing::setup(&[
            &TestCheckPoint::BeforeTimeoutListenerTriggered,
            &CheckPoint::TimeoutListenerTriggered(0),
            &CheckPoint::TimeoutListenerTriggered(1),
            &TestCheckPoint::AfterTimeoutListenerTriggered,
        ]);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool_raw = DefaultSchedulerPool::do_new(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
            SHORTENED_POOL_CLEANER_INTERVAL,
            DEFAULT_MAX_POOLING_DURATION,
            DEFAULT_MAX_USAGE_QUEUE_COUNT,
            SHORTENED_TIMEOUT_DURATION,
        );
        let pool = pool_raw.clone();

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);

        let context = SchedulingContext::for_verification(bank.clone());

        let scheduler = pool.take_scheduler(context);
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        pool.register_timeout_listener(bank.create_timeout_listener());

        sleepless_testing::at(TestCheckPoint::BeforeTimeoutListenerTriggered);
        sleepless_testing::at(TestCheckPoint::AfterTimeoutListenerTriggered);

        // This calls register_recent_blockhash() internally, which in turn calls
        // BankWithScheduler::wait_for_paused_scheduler().
        bank.fill_bank_with_ticks_for_tests();
        let (result, _timings) = bank.wait_for_completed_scheduler().unwrap();
        assert_matches!(result, Ok(()));
    }

    #[test]
    fn test_scheduler_remain_stale_after_error() {
        solana_logger::setup();

        let _progress = sleepless_testing::setup(&[
            &TestCheckPoint::BeforeTimeoutListenerTriggered,
            &CheckPoint::TimeoutListenerTriggered(0),
            &CheckPoint::SchedulerThreadAborted,
            &TestCheckPoint::AfterSchedulerThreadAborted,
            &CheckPoint::TimeoutListenerTriggered(1),
            &TestCheckPoint::AfterTimeoutListenerTriggered,
        ]);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool_raw = SchedulerPool::<PooledScheduler<FaultyHandler>, _>::do_new(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
            SHORTENED_POOL_CLEANER_INTERVAL,
            DEFAULT_MAX_POOLING_DURATION,
            DEFAULT_MAX_USAGE_QUEUE_COUNT,
            SHORTENED_TIMEOUT_DURATION,
        );

        let pool = pool_raw.clone();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);

        let context = SchedulingContext::for_verification(bank.clone());

        let scheduler = pool.take_scheduler(context);
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        pool.register_timeout_listener(bank.create_timeout_listener());

        let tx_before_stale =
            RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_pubkey::new_rand(),
                2,
                genesis_config.hash(),
            ));
        bank.schedule_transaction_executions([(tx_before_stale, 0)].into_iter())
            .unwrap();
        sleepless_testing::at(TestCheckPoint::BeforeTimeoutListenerTriggered);
        sleepless_testing::at(TestCheckPoint::AfterSchedulerThreadAborted);

        sleepless_testing::at(TestCheckPoint::AfterTimeoutListenerTriggered);
        let tx_after_stale =
            RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_pubkey::new_rand(),
                2,
                genesis_config.hash(),
            ));
        let result = bank.schedule_transaction_executions([(tx_after_stale, 1)].into_iter());
        assert_matches!(result, Err(TransactionError::AccountNotFound));

        let (result, _timings) = bank.wait_for_completed_scheduler().unwrap();
        assert_matches!(result, Err(TransactionError::AccountNotFound));
    }

    enum AbortCase {
        Unhandled,
        UnhandledWhilePanicking,
        Handled,
    }

    #[derive(Debug)]
    struct FaultyHandler;
    impl TaskHandler for FaultyHandler {
        fn handle(
            result: &mut Result<()>,
            _timings: &mut ExecuteTimings,
            _bank: &SchedulingContext,
            _task: &Task,
            _handler_context: &HandlerContext,
        ) {
            *result = Err(TransactionError::AccountNotFound);
        }
    }

    fn do_test_scheduler_drop_abort(abort_case: AbortCase) {
        solana_logger::setup();

        let _progress = sleepless_testing::setup(match abort_case {
            AbortCase::Unhandled => &[
                &CheckPoint::SchedulerThreadAborted,
                &TestCheckPoint::AfterSchedulerThreadAborted,
            ],
            _ => &[],
        });

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);

        let tx = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));

        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool = SchedulerPool::<PooledScheduler<FaultyHandler>, _>::new(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
        );
        let context = SchedulingContext::for_verification(bank.clone());
        let scheduler = pool.do_take_scheduler(context);
        scheduler.schedule_execution(tx, 0).unwrap();

        match abort_case {
            AbortCase::Unhandled => {
                sleepless_testing::at(TestCheckPoint::AfterSchedulerThreadAborted);
                // Directly dropping PooledScheduler is illegal unless panicking already, especially
                // after being aborted. It must be converted to PooledSchedulerInner via
                // ::into_inner();
                drop::<PooledScheduler<_>>(scheduler);
            }
            AbortCase::UnhandledWhilePanicking => {
                // no sleepless_testing::at(); panicking special-casing isn't racy
                panic!("ThreadManager::drop() should be skipped...");
            }
            AbortCase::Handled => {
                // no sleepless_testing::at(); ::into_inner() isn't racy
                let ((result, _), mut scheduler_inner) = scheduler.into_inner();
                assert_matches!(result, Err(TransactionError::AccountNotFound));

                // Calling ensure_join_threads() repeatedly should be safe.
                let dummy_flag = true; // doesn't matter because it's skipped anyway
                scheduler_inner
                    .thread_manager
                    .ensure_join_threads(dummy_flag);

                drop::<PooledSchedulerInner<_, _>>(scheduler_inner);
            }
        }
    }

    #[test]
    #[should_panic(expected = "does not match `Some((Ok(_), _))")]
    fn test_scheduler_drop_abort_unhandled() {
        do_test_scheduler_drop_abort(AbortCase::Unhandled);
    }

    #[test]
    #[should_panic(expected = "ThreadManager::drop() should be skipped...")]
    fn test_scheduler_drop_abort_unhandled_while_panicking() {
        do_test_scheduler_drop_abort(AbortCase::UnhandledWhilePanicking);
    }

    #[test]
    fn test_scheduler_drop_abort_handled() {
        do_test_scheduler_drop_abort(AbortCase::Handled);
    }

    #[test]
    fn test_scheduler_drop_short_circuiting() {
        solana_logger::setup();

        let _progress = sleepless_testing::setup(&[
            &TestCheckPoint::BeforeThreadManagerDrop,
            &CheckPoint::NewTask(0),
            &CheckPoint::SchedulerThreadAborted,
            &TestCheckPoint::AfterSchedulerThreadAborted,
        ]);

        static TASK_COUNT: Mutex<usize> = Mutex::new(0);

        #[derive(Debug)]
        struct CountingHandler;
        impl TaskHandler for CountingHandler {
            fn handle(
                _result: &mut Result<()>,
                _timings: &mut ExecuteTimings,
                _bank: &SchedulingContext,
                _task: &Task,
                _handler_context: &HandlerContext,
            ) {
                *TASK_COUNT.lock().unwrap() += 1;
            }
        }

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);

        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool = SchedulerPool::<PooledScheduler<CountingHandler>, _>::new(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
        );
        let context = SchedulingContext::for_verification(bank.clone());
        let scheduler = pool.do_take_scheduler(context);

        // This test is racy.
        //
        // That's because the scheduler needs to be aborted quickly as an expected behavior,
        // leaving some readily-available work untouched. So, schedule rather large number of tasks
        // to make the short-cutting abort code-path win the race easily.
        const MAX_TASK_COUNT: usize = 100;

        for i in 0..MAX_TASK_COUNT {
            let tx = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_pubkey::new_rand(),
                2,
                genesis_config.hash(),
            ));
            scheduler.schedule_execution(tx, i).unwrap();
        }

        // Make sure ThreadManager::drop() is properly short-circuiting for non-aborting scheduler.
        sleepless_testing::at(TestCheckPoint::BeforeThreadManagerDrop);
        drop::<PooledScheduler<_>>(scheduler);
        sleepless_testing::at(TestCheckPoint::AfterSchedulerThreadAborted);
        // All of handler threads should have been aborted before processing MAX_TASK_COUNT tasks.
        assert!(*TASK_COUNT.lock().unwrap() < MAX_TASK_COUNT);
    }

    #[test]
    fn test_scheduler_pool_filo() {
        solana_logger::setup();

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new(None, None, None, None, ignored_prioritization_fee_cache);
        let bank = Arc::new(Bank::default_for_tests());
        let context = &SchedulingContext::for_verification(bank);

        let scheduler1 = pool.do_take_scheduler(context.clone());
        let scheduler_id1 = scheduler1.id();
        let scheduler2 = pool.do_take_scheduler(context.clone());
        let scheduler_id2 = scheduler2.id();
        assert_ne!(scheduler_id1, scheduler_id2);

        let (result_with_timings, scheduler1) = scheduler1.into_inner();
        assert_matches!(result_with_timings, (Ok(()), _));
        pool.return_scheduler(scheduler1);
        let (result_with_timings, scheduler2) = scheduler2.into_inner();
        assert_matches!(result_with_timings, (Ok(()), _));
        pool.return_scheduler(scheduler2);

        let scheduler3 = pool.do_take_scheduler(context.clone());
        assert_eq!(scheduler_id2, scheduler3.id());
        let scheduler4 = pool.do_take_scheduler(context.clone());
        assert_eq!(scheduler_id1, scheduler4.id());
    }

    #[test]
    fn test_scheduler_pool_context_drop_unless_reinitialized() {
        solana_logger::setup();

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new(None, None, None, None, ignored_prioritization_fee_cache);
        let bank = Arc::new(Bank::default_for_tests());
        let context = &SchedulingContext::for_verification(bank);
        let mut scheduler = pool.do_take_scheduler(context.clone());

        // should never panic.
        scheduler.pause_for_recent_blockhash();
        assert_matches!(
            Box::new(scheduler).wait_for_termination(false),
            ((Ok(()), _), _)
        );
    }

    #[test]
    fn test_scheduler_pool_context_replace() {
        solana_logger::setup();

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new(None, None, None, None, ignored_prioritization_fee_cache);
        let old_bank = &Arc::new(Bank::default_for_tests());
        let new_bank = &Arc::new(Bank::default_for_tests());
        assert!(!Arc::ptr_eq(old_bank, new_bank));

        let old_context = &SchedulingContext::for_verification(old_bank.clone());
        let new_context = &SchedulingContext::for_verification(new_bank.clone());

        let scheduler = pool.do_take_scheduler(old_context.clone());
        let scheduler_id = scheduler.id();
        pool.return_scheduler(scheduler.into_inner().1);

        let scheduler = pool.take_scheduler(new_context.clone());
        assert_eq!(scheduler_id, scheduler.id());
        assert!(Arc::ptr_eq(scheduler.context().bank().unwrap(), new_bank));
    }

    #[test]
    fn test_scheduler_pool_install_into_bank_forks() {
        solana_logger::setup();

        let bank = Bank::default_for_tests();
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut bank_forks = bank_forks.write().unwrap();
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new_dyn(None, None, None, None, ignored_prioritization_fee_cache);
        bank_forks.install_scheduler_pool(pool);
    }

    #[test]
    fn test_scheduler_install_into_bank() {
        solana_logger::setup();

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Arc::new(Bank::new_for_tests(&genesis_config));
        let child_bank = Bank::new_from_parent(bank, &Pubkey::default(), 1);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new_dyn(None, None, None, None, ignored_prioritization_fee_cache);

        let bank = Bank::default_for_tests();
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut bank_forks = bank_forks.write().unwrap();

        // existing banks in bank_forks shouldn't process transactions anymore in general, so
        // shouldn't be touched
        assert!(!bank_forks
            .working_bank_with_scheduler()
            .has_installed_scheduler());
        bank_forks.install_scheduler_pool(pool);
        assert!(!bank_forks
            .working_bank_with_scheduler()
            .has_installed_scheduler());

        let mut child_bank = bank_forks.insert(child_bank);
        assert!(child_bank.has_installed_scheduler());
        bank_forks.remove(child_bank.slot());
        child_bank.drop_scheduler();
        assert!(!child_bank.has_installed_scheduler());
    }

    fn setup_dummy_fork_graph(bank: Bank) -> (Arc<Bank>, Arc<RwLock<BankForks>>) {
        let slot = bank.slot();
        let bank_fork = BankForks::new_rw_arc(bank);
        let bank = bank_fork.read().unwrap().get(slot).unwrap();
        bank.set_fork_graph_in_program_cache(Arc::downgrade(&bank_fork));
        (bank, bank_fork)
    }

    #[test]
    fn test_scheduler_schedule_execution_success() {
        solana_logger::setup();

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
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new_dyn(None, None, None, None, ignored_prioritization_fee_cache);
        let context = SchedulingContext::for_verification(bank.clone());

        assert_eq!(bank.transaction_count(), 0);
        let scheduler = pool.take_scheduler(context);
        scheduler.schedule_execution(tx0, 0).unwrap();
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        assert_matches!(bank.wait_for_completed_scheduler(), Some((Ok(()), _)));
        assert_eq!(bank.transaction_count(), 1);
    }

    fn do_test_scheduler_schedule_execution_failure(extra_tx_after_failure: bool) {
        solana_logger::setup();

        let _progress = sleepless_testing::setup(&[
            &CheckPoint::TaskHandled(0),
            &TestCheckPoint::AfterTaskHandled,
            &CheckPoint::SchedulerThreadAborted,
            &TestCheckPoint::AfterSchedulerThreadAborted,
            &TestCheckPoint::BeforeTrashedSchedulerCleaned,
            &CheckPoint::TrashedSchedulerCleaned(0),
            &CheckPoint::TrashedSchedulerCleaned(1),
            &TestCheckPoint::AfterTrashedSchedulerCleaned,
        ]);

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool_raw = DefaultSchedulerPool::do_new(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
            SHORTENED_POOL_CLEANER_INTERVAL,
            DEFAULT_MAX_POOLING_DURATION,
            DEFAULT_MAX_USAGE_QUEUE_COUNT,
            DEFAULT_TIMEOUT_DURATION,
        );
        let pool = pool_raw.clone();
        let context = SchedulingContext::for_verification(bank.clone());
        let scheduler = pool.take_scheduler(context);

        let unfunded_keypair = Keypair::new();
        let bad_tx = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
            &unfunded_keypair,
            &solana_pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));
        assert_eq!(bank.transaction_count(), 0);
        scheduler.schedule_execution(bad_tx, 0).unwrap();
        sleepless_testing::at(TestCheckPoint::AfterTaskHandled);
        assert_eq!(bank.transaction_count(), 0);

        let good_tx_after_bad_tx =
            RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_pubkey::new_rand(),
                3,
                genesis_config.hash(),
            ));
        // make sure this tx is really a good one to execute.
        assert_matches!(
            bank.simulate_transaction_unchecked(&good_tx_after_bad_tx, false)
                .result,
            Ok(_)
        );
        sleepless_testing::at(TestCheckPoint::AfterSchedulerThreadAborted);
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        if extra_tx_after_failure {
            assert_matches!(
                bank.schedule_transaction_executions([(good_tx_after_bad_tx, 1)].into_iter()),
                Err(TransactionError::AccountNotFound)
            );
        }
        // transaction_count should remain same as scheduler should be bailing out.
        // That's because we're testing the serialized failing execution case in this test.
        // Also note that bank.transaction_count() is generally racy by nature, because
        // blockstore_processor and unified_scheduler both tend to process non-conflicting batches
        // in parallel as part of the normal operation.
        assert_eq!(bank.transaction_count(), 0);

        assert_eq!(pool_raw.trashed_scheduler_inners.lock().unwrap().len(), 0);
        assert_matches!(
            bank.wait_for_completed_scheduler(),
            Some((Err(TransactionError::AccountNotFound), _timings))
        );

        // Block solScCleaner until we see trashed schedler...
        assert_eq!(pool_raw.trashed_scheduler_inners.lock().unwrap().len(), 1);
        sleepless_testing::at(TestCheckPoint::BeforeTrashedSchedulerCleaned);

        // See the trashed scheduler gone only after solScCleaner did its job...
        sleepless_testing::at(TestCheckPoint::AfterTrashedSchedulerCleaned);
        assert_eq!(pool_raw.trashed_scheduler_inners.lock().unwrap().len(), 0);
    }

    #[test]
    fn test_scheduler_schedule_execution_failure_with_extra_tx() {
        do_test_scheduler_schedule_execution_failure(true);
    }

    #[test]
    fn test_scheduler_schedule_execution_failure_without_extra_tx() {
        do_test_scheduler_schedule_execution_failure(false);
    }

    #[test]
    #[should_panic(expected = "This panic should be propagated. (From: ")]
    fn test_scheduler_schedule_execution_panic() {
        solana_logger::setup();

        #[derive(Debug)]
        enum PanickingHanlderCheckPoint {
            BeforeNotifiedPanic,
            BeforeIgnoredPanic,
        }

        let progress = sleepless_testing::setup(&[
            &TestCheckPoint::BeforeNewTask,
            &CheckPoint::NewTask(0),
            &PanickingHanlderCheckPoint::BeforeNotifiedPanic,
            &CheckPoint::SchedulerThreadAborted,
            &PanickingHanlderCheckPoint::BeforeIgnoredPanic,
            &TestCheckPoint::BeforeEndSession,
        ]);

        #[derive(Debug)]
        struct PanickingHandler;
        impl TaskHandler for PanickingHandler {
            fn handle(
                _result: &mut Result<()>,
                _timings: &mut ExecuteTimings,
                _bank: &SchedulingContext,
                task: &Task,
                _handler_context: &HandlerContext,
            ) {
                let index = task.task_index();
                if index == 0 {
                    sleepless_testing::at(PanickingHanlderCheckPoint::BeforeNotifiedPanic);
                } else if index == 1 {
                    sleepless_testing::at(PanickingHanlderCheckPoint::BeforeIgnoredPanic);
                } else {
                    unreachable!();
                }
                panic!("This panic should be propagated.");
            }
        }

        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);

        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);

        // Use 2 transactions with different timings to deliberately cover the two code paths of
        // notifying panics in the handler threads, taken conditionally depending on whether the
        // scheduler thread has been aborted already or not.
        const TX_COUNT: usize = 2;

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool = SchedulerPool::<PooledScheduler<PanickingHandler>, _>::new_dyn(
            Some(TX_COUNT), // fix to use exactly 2 handlers
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
        );
        let context = SchedulingContext::for_verification(bank.clone());

        let scheduler = pool.take_scheduler(context);

        for index in 0..TX_COUNT {
            // Use 2 non-conflicting txes to exercise the channel disconnected case as well.
            let tx = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                &Keypair::new(),
                &solana_pubkey::new_rand(),
                1,
                genesis_config.hash(),
            ));
            scheduler.schedule_execution(tx, index).unwrap();
        }
        // finally unblock the scheduler thread; otherwise the above schedule_execution could
        // return SchedulerAborted...
        sleepless_testing::at(TestCheckPoint::BeforeNewTask);

        sleepless_testing::at(TestCheckPoint::BeforeEndSession);
        let bank = BankWithScheduler::new(bank, Some(scheduler));

        // the outer .unwrap() will panic. so, drop progress now.
        drop(progress);
        bank.wait_for_completed_scheduler().unwrap().0.unwrap();
    }

    #[test]
    fn test_scheduler_execution_failure_short_circuiting() {
        solana_logger::setup();

        let _progress = sleepless_testing::setup(&[
            &TestCheckPoint::BeforeNewTask,
            &CheckPoint::NewTask(0),
            &CheckPoint::TaskHandled(0),
            &CheckPoint::SchedulerThreadAborted,
            &TestCheckPoint::AfterSchedulerThreadAborted,
        ]);

        static TASK_COUNT: Mutex<usize> = Mutex::new(0);

        #[derive(Debug)]
        struct CountingFaultyHandler;
        impl TaskHandler for CountingFaultyHandler {
            fn handle(
                result: &mut Result<()>,
                _timings: &mut ExecuteTimings,
                _bank: &SchedulingContext,
                task: &Task,
                _handler_context: &HandlerContext,
            ) {
                let index = task.task_index();
                *TASK_COUNT.lock().unwrap() += 1;
                if index == 1 {
                    *result = Err(TransactionError::AccountNotFound);
                }
                sleepless_testing::at(CheckPoint::TaskHandled(index));
            }
        }

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);

        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool = SchedulerPool::<PooledScheduler<CountingFaultyHandler>, _>::new(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
        );
        let context = SchedulingContext::for_verification(bank.clone());
        let scheduler = pool.do_take_scheduler(context);

        for i in 0..10 {
            let tx = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_pubkey::new_rand(),
                2,
                genesis_config.hash(),
            ));
            scheduler.schedule_execution(tx, i).unwrap();
        }
        // finally unblock the scheduler thread; otherwise the above schedule_execution could
        // return SchedulerAborted...
        sleepless_testing::at(TestCheckPoint::BeforeNewTask);

        // Make sure bank.wait_for_completed_scheduler() is properly short-circuiting for aborting scheduler.
        let bank = BankWithScheduler::new(bank, Some(Box::new(scheduler)));
        assert_matches!(
            bank.wait_for_completed_scheduler(),
            Some((Err(TransactionError::AccountNotFound), _timings))
        );
        sleepless_testing::at(TestCheckPoint::AfterSchedulerThreadAborted);
        assert!(*TASK_COUNT.lock().unwrap() < 10);
    }

    fn create_genesis_config_for_block_production(lamports: u64) -> GenesisConfigInfo {
        // The in-scope create_genesis_config(), which is imported from the `solana-runtime`,
        // doesn't properly setup leader schedule, causing the following panic if used for poh
        // recorder, so use the one from the `solana-ledger` crate:
        //
        //   thread 'tests::...' panicked at ledger/src/leader_schedule.rs:LL:CC:
        //   called `Result::unwrap()` on an `Err` value: NoItem
        solana_ledger::genesis_utils::create_genesis_config(lamports)
    }

    #[test_matrix(
        [BlockVerification, BlockProduction]
    )]
    fn test_scheduler_schedule_execution_blocked_at_session_ending(
        scheduling_mode: SchedulingMode,
    ) {
        solana_logger::setup();

        const STALLED_TRANSACTION_INDEX: usize = 0;
        const BLOCKED_TRANSACTION_INDEX: usize = 1;

        let _progress = sleepless_testing::setup(&[
            &CheckPoint::BufferedTask(BLOCKED_TRANSACTION_INDEX),
            &TestCheckPoint::AfterBufferedTask,
            &CheckPoint::SessionEnding,
            &TestCheckPoint::AfterSessionEnding,
        ]);

        #[derive(Debug)]
        struct StallingHandler;
        impl TaskHandler for StallingHandler {
            fn handle(
                result: &mut Result<()>,
                timings: &mut ExecuteTimings,
                scheduling_context: &SchedulingContext,
                task: &Task,
                handler_context: &HandlerContext,
            ) {
                let index = task.task_index();
                match index {
                    STALLED_TRANSACTION_INDEX => {
                        sleepless_testing::at(TestCheckPoint::AfterSessionEnding);
                    }
                    BLOCKED_TRANSACTION_INDEX => {}
                    _ => unreachable!(),
                };
                // Always execute transactions with a faked context with the fixed block
                // verification mode; Otherwise, the block production test variant will be
                // encountered with CommitCancelled error from the poh recorder, which is fed with
                // a dummy bank.
                let faked_context = &SchedulingContext::new_with_mode(
                    BlockVerification,
                    scheduling_context.bank().unwrap().clone(),
                );
                DefaultTaskHandler::handle(result, timings, faked_context, task, handler_context);
            }
        }

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_for_block_production(10_000);

        // tx0 and tx1 is definitely conflicting to write-lock the mint address
        let tx0 = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));
        let tx1 = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));

        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool = SchedulerPool::<PooledScheduler<StallingHandler>, _>::new(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
        );
        let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let (_banking_packet_sender, banking_packet_receiver) = crossbeam_channel::unbounded();
        let (exit, _poh_recorder, transaction_recorder, poh_service, _signal_receiver) = {
            // Create a dummy bank to prevent it from being frozen; otherwise, the following panic
            // will happen:
            //    thread 'solPohTickProd' panicked at runtime/src/bank.rs:LL:CC:
            //    register_tick() working on a bank that is already frozen or is undergoing freezing!
            let dummy_bank = Bank::new_for_tests(&genesis_config);
            let (dummy_bank, _bank_forks) = setup_dummy_fork_graph(dummy_bank);
            create_test_recorder_with_index_tracking(
                dummy_bank,
                blockstore.clone(),
                None,
                Some(leader_schedule_cache),
            )
        };

        if matches!(scheduling_mode, BlockProduction) {
            pool.register_banking_stage(
                banking_packet_receiver,
                Box::new(|_, _| unreachable!()),
                transaction_recorder,
            );
        }

        // This variable tracks the cumulative count of transactions since genesis, which is
        // incremented as test is progressed.
        let mut expected_transaction_count = Saturating(0);
        assert_eq!(bank.transaction_count(), expected_transaction_count.0);

        let context = SchedulingContext::new_with_mode(scheduling_mode, bank.clone());
        let scheduler = pool.take_scheduler(context);
        let old_scheduler_id = scheduler.id();

        scheduler
            .schedule_execution(tx0, STALLED_TRANSACTION_INDEX)
            .unwrap();
        scheduler
            .schedule_execution(tx1, BLOCKED_TRANSACTION_INDEX)
            .unwrap();

        let bank = BankWithScheduler::new(bank, Some(scheduler));

        sleepless_testing::at(TestCheckPoint::AfterBufferedTask);

        assert_matches!(bank.wait_for_completed_scheduler(), Some((Ok(()), _)));

        expected_transaction_count += 1;
        if matches!(scheduling_mode, BlockVerification) {
            // Block verification scheduler should fully clear its blocked transactions before
            // finishing.
            expected_transaction_count += 1;
        }
        assert_eq!(bank.transaction_count(), expected_transaction_count.0);

        // Create new bank to observe behavior difference around session ending
        let bank = Arc::new(Bank::new_from_parent(
            bank.clone_without_scheduler(),
            &Pubkey::default(),
            bank.slot().checked_add(1).unwrap(),
        ));
        assert_eq!(bank.transaction_count(), expected_transaction_count.0);

        let context = SchedulingContext::new_with_mode(scheduling_mode, bank.clone());
        let scheduler = pool.take_scheduler(context);
        // make sure the same scheduler is used to test its internal cross-session behavior
        // regardless scheduling_mode.
        assert_eq!(scheduler.id(), old_scheduler_id);

        let bank = BankWithScheduler::new(bank, Some(scheduler));
        assert_matches!(bank.wait_for_completed_scheduler(), Some((Ok(()), _)));

        if matches!(scheduling_mode, BlockProduction) {
            // Block production scheduler should carry over transactions from previous bank
            expected_transaction_count += 1;
        }
        assert_eq!(bank.transaction_count(), expected_transaction_count.0);

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }

    #[test]
    fn test_block_production_scheduler_schedule_execution_retry() {
        solana_logger::setup();

        const ORIGINAL_TRANSACTION_INDEX: usize = 999;
        // This is 0 because it's the first task id assigned internally by BankingStageHelper
        const RETRIED_TRANSACTION_INDEX: usize = 0;
        const FULL_BLOCK_SLOT: Slot = 1;

        let _progress = sleepless_testing::setup(&[
            &CheckPoint::TaskAccumulated(
                ORIGINAL_TRANSACTION_INDEX,
                &Err(TransactionError::WouldExceedMaxBlockCostLimit),
            ),
            &CheckPoint::SessionFinished(Some(FULL_BLOCK_SLOT)),
            &TestCheckPoint::AfterSessionFinished,
            &TestCheckPoint::AfterSession,
            &CheckPoint::TaskAccumulated(RETRIED_TRANSACTION_INDEX, &Ok(())),
            &TestCheckPoint::AfterTaskAccumulated,
        ]);

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_for_block_production(10_000);

        let tx = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));
        let bank = Bank::new_for_tests(&genesis_config);

        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new(None, None, None, None, ignored_prioritization_fee_cache);
        let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let (_banking_packet_sender, banking_packet_receiver) = crossbeam_channel::unbounded();
        let (exit, poh_recorder, transaction_recorder, poh_service, _signal_receiver) = {
            create_test_recorder_with_index_tracking(
                bank.clone(),
                blockstore.clone(),
                None,
                Some(leader_schedule_cache),
            )
        };
        poh_recorder
            .write()
            .unwrap()
            .reset(bank.clone(), Some((bank.slot(), bank.slot() + 1)));

        pool.register_banking_stage(
            banking_packet_receiver,
            Box::new(|_, _| unreachable!()),
            transaction_recorder,
        );

        let bank = Arc::new(Bank::new_from_parent(
            bank.clone(),
            &Pubkey::default(),
            bank.slot().checked_add(1).unwrap(),
        ));
        // Immediately trigger WouldExceedMaxBlockCostLimit by setting all cost limits to 0
        bank.write_cost_tracker().unwrap().set_limits(0, 0, 0);

        let context = SchedulingContext::for_production(bank.clone());
        let scheduler = pool.take_scheduler(context);
        let old_scheduler_id = scheduler.id();
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        poh_recorder
            .write()
            .unwrap()
            .set_bank(bank.clone_with_scheduler(), true);
        bank.schedule_transaction_executions([(tx, ORIGINAL_TRANSACTION_INDEX)].into_iter())
            .unwrap();

        // Calling wait_for_completed_scheduler() for block production scheduler causes it to be
        // interrupted immediately; so need to wait for failed landing of the original task.
        sleepless_testing::at(&TestCheckPoint::AfterSessionFinished);
        assert_matches!(bank.wait_for_completed_scheduler(), Some((Ok(()), _)));
        sleepless_testing::at(&TestCheckPoint::AfterSession);
        // There should be no executed transaction yet.
        assert_eq!(bank.transaction_count(), 0);

        // Create new bank to observe behavior difference around session ending
        poh_recorder
            .write()
            .unwrap()
            .reset(bank.clone(), Some((bank.slot(), bank.slot() + 1)));
        let bank = Arc::new(Bank::new_from_parent(
            bank.clone_without_scheduler(),
            &Pubkey::default(),
            bank.slot().checked_add(1).unwrap(),
        ));
        // Revert the block cost limit
        bank.write_cost_tracker()
            .unwrap()
            .set_limits(u64::MAX, u64::MAX, u64::MAX);

        // Taking poh_recorder is needed to avoid race conditions between poh and scheduler, which
        // already has readily-runnable tx in the buffer, unlike the previous bank;
        // This will be addressed in the next pr...
        let mut poh_recorder = poh_recorder.write().unwrap();

        let context = SchedulingContext::for_production(bank.clone());
        let scheduler = pool.take_scheduler(context);
        // Make sure the same scheduler is used to test its internal cross-session behavior
        assert_eq!(scheduler.id(), old_scheduler_id);
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        poh_recorder.set_bank(bank.clone_with_scheduler(), true);
        drop(poh_recorder);

        // Calling wait_for_completed_scheduler() for block production scheduler causes it to be
        // interrupted immediately; so need to wait for successful landing of the retried task.
        sleepless_testing::at(&TestCheckPoint::AfterTaskAccumulated);
        assert_matches!(bank.wait_for_completed_scheduler(), Some((Ok(()), _)));
        // Block production scheduler should carry over the temporarily-failed transaction itself
        // and the transaction should now have been executed.
        assert_eq!(bank.transaction_count(), 1);

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }

    #[test]
    fn test_scheduler_mismatched_scheduling_context_race() {
        solana_logger::setup();

        #[derive(Debug)]
        struct TaskAndContextChecker;
        impl TaskHandler for TaskAndContextChecker {
            fn handle(
                _result: &mut Result<()>,
                _timings: &mut ExecuteTimings,
                context: &SchedulingContext,
                task: &Task,
                _handler_context: &HandlerContext,
            ) {
                // The task index must always be matched to the slot.
                assert_eq!(task.task_index() as Slot, context.slot().unwrap());
            }
        }

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);

        // Create two banks for two contexts
        let bank0 = Bank::new_for_tests(&genesis_config);
        let bank0 = setup_dummy_fork_graph(bank0).0;
        let bank1 = Arc::new(Bank::new_from_parent(
            bank0.clone(),
            &Pubkey::default(),
            bank0.slot().checked_add(1).unwrap(),
        ));

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool = SchedulerPool::<PooledScheduler<TaskAndContextChecker>, _>::new(
            Some(4), // spawn 4 threads
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
        );

        // Create a dummy tx and two contexts
        let dummy_tx =
            RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_pubkey::new_rand(),
                2,
                genesis_config.hash(),
            ));
        let context0 = &SchedulingContext::for_verification(bank0.clone());
        let context1 = &SchedulingContext::for_verification(bank1.clone());

        // Exercise the scheduler by busy-looping to expose the race condition
        for (context, index) in [(context0, 0), (context1, 1)]
            .into_iter()
            .cycle()
            .take(10000)
        {
            let scheduler = pool.take_scheduler(context.clone());
            scheduler
                .schedule_execution(dummy_tx.clone(), index)
                .unwrap();
            scheduler.wait_for_termination(false).1.return_to_pool();
        }
    }

    #[derive(Debug)]
    struct AsyncScheduler<const TRIGGER_RACE_CONDITION: bool>(
        Mutex<ResultWithTimings>,
        Mutex<Vec<JoinHandle<ResultWithTimings>>>,
        SchedulingContext,
        Arc<SchedulerPool<Self, DefaultTaskHandler>>,
    );

    impl<const TRIGGER_RACE_CONDITION: bool> AsyncScheduler<TRIGGER_RACE_CONDITION> {
        fn do_wait(&self) {
            let mut overall_result = Ok(());
            let mut overall_timings = ExecuteTimings::default();
            for handle in self.1.lock().unwrap().drain(..) {
                let (result, timings) = handle.join().unwrap();
                match result {
                    Ok(()) => {}
                    Err(e) => overall_result = Err(e),
                }
                overall_timings.accumulate(&timings);
            }
            *self.0.lock().unwrap() = (overall_result, overall_timings);
        }
    }

    impl<const TRIGGER_RACE_CONDITION: bool> InstalledScheduler
        for AsyncScheduler<TRIGGER_RACE_CONDITION>
    {
        fn id(&self) -> SchedulerId {
            unimplemented!();
        }

        fn context(&self) -> &SchedulingContext {
            &self.2
        }

        fn schedule_execution(
            &self,
            transaction: RuntimeTransaction<SanitizedTransaction>,
            index: usize,
        ) -> ScheduleResult {
            let context = self.context().clone();
            let pool = self.3.clone();

            self.1.lock().unwrap().push(std::thread::spawn(move || {
                // intentionally sleep to simulate race condition where register_recent_blockhash
                // is handle before finishing executing scheduled transactions
                std::thread::sleep(std::time::Duration::from_secs(1));

                let mut result = Ok(());
                let mut timings = ExecuteTimings::default();

                let task = SchedulingStateMachine::create_task(transaction, index, &mut |_| {
                    UsageQueue::default()
                });

                <DefaultTaskHandler as TaskHandler>::handle(
                    &mut result,
                    &mut timings,
                    &context,
                    &task,
                    &pool.create_handler_context(
                        BlockVerification,
                        &crossbeam_channel::unbounded().0,
                    ),
                );
                (result, timings)
            }));

            Ok(())
        }

        fn recover_error_after_abort(&mut self) -> TransactionError {
            unimplemented!();
        }

        fn wait_for_termination(
            self: Box<Self>,
            _is_dropped: bool,
        ) -> (ResultWithTimings, UninstalledSchedulerBox) {
            self.do_wait();
            let result_with_timings = std::mem::replace(
                &mut *self.0.lock().unwrap(),
                initialized_result_with_timings(),
            );
            (result_with_timings, self)
        }

        fn pause_for_recent_blockhash(&mut self) {
            if TRIGGER_RACE_CONDITION {
                // this is equivalent to NOT calling wait_for_paused_scheduler() in
                // register_recent_blockhash().
                return;
            }
            self.do_wait();
        }
    }

    impl<const TRIGGER_RACE_CONDITION: bool> SchedulerInner for AsyncScheduler<TRIGGER_RACE_CONDITION> {
        fn id(&self) -> SchedulerId {
            42
        }

        fn is_trashed(&self) -> bool {
            false
        }
    }

    impl<const TRIGGER_RACE_CONDITION: bool> UninstalledScheduler
        for AsyncScheduler<TRIGGER_RACE_CONDITION>
    {
        fn return_to_pool(self: Box<Self>) {
            self.3.clone().return_scheduler(*self)
        }
    }

    impl<const TRIGGER_RACE_CONDITION: bool> SpawnableScheduler<DefaultTaskHandler>
        for AsyncScheduler<TRIGGER_RACE_CONDITION>
    {
        // well, i wish i can use ! (never type).....
        type Inner = Self;

        fn into_inner(self) -> (ResultWithTimings, Self::Inner) {
            unimplemented!();
        }

        fn from_inner(
            _inner: Self::Inner,
            _context: SchedulingContext,
            _result_with_timings: ResultWithTimings,
        ) -> Self {
            unimplemented!();
        }

        fn spawn(
            pool: Arc<SchedulerPool<Self, DefaultTaskHandler>>,
            context: SchedulingContext,
            _result_with_timings: ResultWithTimings,
        ) -> Self {
            AsyncScheduler::<TRIGGER_RACE_CONDITION>(
                Mutex::new(initialized_result_with_timings()),
                Mutex::new(vec![]),
                context,
                pool,
            )
        }
    }

    fn do_test_scheduler_schedule_execution_recent_blockhash_edge_case<
        const TRIGGER_RACE_CONDITION: bool,
    >() {
        solana_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let very_old_valid_tx =
            RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_pubkey::new_rand(),
                2,
                genesis_config.hash(),
            ));
        let mut bank = Bank::new_for_tests(&genesis_config);
        for _ in 0..MAX_PROCESSING_AGE {
            bank.fill_bank_with_ticks_for_tests();
            bank.freeze();
            let slot = bank.slot();
            bank = Bank::new_from_parent(
                Arc::new(bank),
                &Pubkey::default(),
                slot.checked_add(1).unwrap(),
            );
        }
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);
        let context = SchedulingContext::for_verification(bank.clone());

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            SchedulerPool::<AsyncScheduler<TRIGGER_RACE_CONDITION>, DefaultTaskHandler>::new_dyn(
                None,
                None,
                None,
                None,
                ignored_prioritization_fee_cache,
            );
        let scheduler = pool.take_scheduler(context);

        let bank = BankWithScheduler::new(bank, Some(scheduler));
        assert_eq!(bank.transaction_count(), 0);

        // schedule but not immediately execute transaction
        bank.schedule_transaction_executions([(very_old_valid_tx, 0)].into_iter())
            .unwrap();
        // this calls register_recent_blockhash internally
        bank.fill_bank_with_ticks_for_tests();

        if TRIGGER_RACE_CONDITION {
            // very_old_valid_tx is wrongly handled as expired!
            assert_matches!(
                bank.wait_for_completed_scheduler(),
                Some((Err(TransactionError::BlockhashNotFound), _))
            );
            assert_eq!(bank.transaction_count(), 0);
        } else {
            assert_matches!(bank.wait_for_completed_scheduler(), Some((Ok(()), _)));
            assert_eq!(bank.transaction_count(), 1);
        }
    }

    #[test]
    fn test_scheduler_schedule_execution_recent_blockhash_edge_case_with_race() {
        do_test_scheduler_schedule_execution_recent_blockhash_edge_case::<true>();
    }

    #[test]
    fn test_scheduler_schedule_execution_recent_blockhash_edge_case_without_race() {
        do_test_scheduler_schedule_execution_recent_blockhash_edge_case::<false>();
    }

    #[test]
    fn test_default_handler_count() {
        for (detected, expected) in [(32, 8), (4, 1), (2, 1)] {
            assert_eq!(
                DefaultSchedulerPool::calculate_default_handler_count(Some(detected)),
                expected
            );
        }
        assert_eq!(
            DefaultSchedulerPool::calculate_default_handler_count(None),
            4
        );
    }

    // See comment in SchedulingStateMachine::create_task() for the justification of this test
    #[test]
    fn test_enfoced_get_account_locks_validation() {
        solana_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            ref mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = &setup_dummy_fork_graph(bank);

        let mut tx = system_transaction::transfer(
            mint_keypair,
            &solana_pubkey::new_rand(),
            2,
            genesis_config.hash(),
        );
        // mangle the transfer tx to try to lock fee_payer (= mint_keypair) address twice!
        tx.message.account_keys.push(tx.message.account_keys[0]);
        let tx = RuntimeTransaction::from_transaction_for_tests(tx);

        // this internally should call SanitizedTransaction::get_account_locks().
        let result = &mut Ok(());
        let timings = &mut ExecuteTimings::default();
        let prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let scheduling_context = &SchedulingContext::for_verification(bank.clone());
        let handler_context = &HandlerContext {
            log_messages_bytes_limit: None,
            transaction_status_sender: None,
            replay_vote_sender: None,
            prioritization_fee_cache,
            banking_packet_receiver: never(),
            banking_packet_handler: Box::new(|_, _| {}),
            banking_stage_helper: None,
            transaction_recorder: None,
        };

        let task = SchedulingStateMachine::create_task(tx, 0, &mut |_| UsageQueue::default());
        DefaultTaskHandler::handle(result, timings, scheduling_context, &task, handler_context);
        assert_matches!(result, Err(TransactionError::AccountLoadedTwice));
    }

    enum TxResult {
        ExecutedWithSuccess,
        ExecutedWithFailure,
        NotExecuted,
    }

    #[test_matrix(
        [TxResult::ExecutedWithSuccess, TxResult::ExecutedWithFailure, TxResult::NotExecuted],
        [false, true]
    )]
    fn test_task_handler_poh_recording(tx_result: TxResult, should_succeed_to_record_to_poh: bool) {
        solana_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            ref mint_keypair,
            ..
        } = create_genesis_config_for_block_production(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let bank = bank_forks.read().unwrap().working_bank_with_scheduler();

        let (tx, expected_tx_result) = match tx_result {
            TxResult::ExecutedWithSuccess => (
                system_transaction::transfer(
                    mint_keypair,
                    &solana_pubkey::new_rand(),
                    1,
                    genesis_config.hash(),
                ),
                Ok(()),
            ),
            TxResult::ExecutedWithFailure => (
                system_transaction::transfer(
                    mint_keypair,
                    &solana_pubkey::new_rand(),
                    1_000_000,
                    genesis_config.hash(),
                ),
                Ok(()),
            ),
            TxResult::NotExecuted => (
                system_transaction::transfer(
                    mint_keypair,
                    &solana_pubkey::new_rand(),
                    1,
                    Hash::default(),
                ),
                Err(TransactionError::BlockhashNotFound),
            ),
        };
        let tx = RuntimeTransaction::from_transaction_for_tests(tx);

        let result = &mut Ok(());
        let timings = &mut ExecuteTimings::default();
        let prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let scheduling_context = &SchedulingContext::for_production(bank.clone());
        let (sender, receiver) = crossbeam_channel::unbounded();
        let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let (exit, poh_recorder, transaction_recorder, poh_service, signal_receiver) =
            create_test_recorder_with_index_tracking(
                bank.clone(),
                blockstore.clone(),
                None,
                Some(leader_schedule_cache),
            );
        let handler_context = &HandlerContext {
            log_messages_bytes_limit: None,
            transaction_status_sender: Some(TransactionStatusSender { sender }),
            replay_vote_sender: None,
            prioritization_fee_cache,
            banking_packet_receiver: never(),
            banking_packet_handler: Box::new(|_, _| {}),
            banking_stage_helper: None,
            transaction_recorder: Some(transaction_recorder),
        };

        let task =
            SchedulingStateMachine::create_task(tx.clone(), 0, &mut |_| UsageQueue::default());

        // wait until the poh's working bank is cleared.
        // also flush signal_receiver after that.
        if !should_succeed_to_record_to_poh {
            while poh_recorder.read().unwrap().bank().is_some() {
                sleep(Duration::from_millis(100));
            }
            while signal_receiver.try_recv().is_ok() {}
        }

        assert_eq!(bank.transaction_count(), 0);
        assert_eq!(bank.transaction_error_count(), 0);
        DefaultTaskHandler::handle(result, timings, scheduling_context, &task, handler_context);

        if should_succeed_to_record_to_poh {
            if expected_tx_result.is_ok() {
                assert_matches!(result, Ok(()));
                assert_eq!(bank.transaction_count(), 1);
                if matches!(tx_result, TxResult::ExecutedWithFailure) {
                    assert_eq!(bank.transaction_error_count(), 1);
                } else {
                    assert_eq!(bank.transaction_error_count(), 0);
                }
                assert_matches!(
                    receiver.try_recv(),
                    Ok(TransactionStatusMessage::Batch(
                        TransactionStatusBatch { .. }
                    ))
                );
                assert_matches!(
                    signal_receiver.try_recv(),
                    Ok((_, (solana_entry::entry::Entry {transactions, ..} , _)))
                        if transactions == vec![tx.to_versioned_transaction()]
                );
            } else {
                assert_eq!(result, &expected_tx_result);
                assert_eq!(bank.transaction_count(), 0);
                assert_eq!(bank.transaction_error_count(), 0);
                assert_matches!(receiver.try_recv(), Err(_));
                assert_matches!(signal_receiver.try_recv(), Err(_));
            }
        } else {
            if expected_tx_result.is_ok() {
                assert_matches!(result, Err(TransactionError::CommitCancelled));
            } else {
                assert_eq!(result, &expected_tx_result);
            }

            assert_eq!(bank.transaction_count(), 0);
            assert_matches!(receiver.try_recv(), Err(_));
            assert_matches!(signal_receiver.try_recv(), Err(_));
        }

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }

    #[test]
    fn test_block_production_scheduler_schedule_execution_success() {
        solana_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_for_block_production(10_000);

        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new(None, None, None, None, ignored_prioritization_fee_cache);

        let (_banking_packet_sender, banking_packet_receiver) = crossbeam_channel::unbounded();
        let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let (exit, _poh_recorder, transaction_recorder, poh_service, _signal_receiver) =
            create_test_recorder_with_index_tracking(
                bank.clone(),
                blockstore.clone(),
                None,
                Some(leader_schedule_cache),
            );
        pool.register_banking_stage(
            banking_packet_receiver,
            // we don't use the banking packet channel in this test. so, pass panicking handler.
            Box::new(|_, _| unreachable!()),
            transaction_recorder,
        );

        assert_eq!(bank.transaction_count(), 0);
        let context = SchedulingContext::for_production(bank.clone());
        let scheduler = pool.take_scheduler(context);
        let tx0 = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));
        scheduler.schedule_execution(tx0, 0).unwrap();
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        assert_matches!(bank.wait_for_completed_scheduler(), Some((Ok(()), _)));
        assert_eq!(bank.transaction_count(), 1);

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }

    #[test]
    fn test_block_production_scheduler_buffering_on_spawn() {
        solana_logger::setup();

        let _progress = sleepless_testing::setup(&[
            &CheckPoint::NewBufferedTask(17),
            &TestCheckPoint::AfterNewBufferedTask,
        ]);

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_for_block_production(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new(None, None, None, None, ignored_prioritization_fee_cache);

        let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let (exit, _poh_recorder, transaction_recorder, poh_service, _signal_receiver) =
            create_test_recorder_with_index_tracking(
                bank.clone(),
                blockstore.clone(),
                None,
                Some(leader_schedule_cache),
            );

        // send fake packet batch to trigger banking_packet_handler
        let (banking_packet_sender, banking_packet_receiver) = crossbeam_channel::unbounded();
        banking_packet_sender
            .send(BankingPacketBatch::default())
            .unwrap();
        assert_eq!(banking_packet_sender.len(), 1);

        // Create a dummy handler which unconditionally sends tx0 back to the scheduler thread
        let tx0 = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));
        let fixed_banking_packet_handler =
            Box::new(move |helper: &BankingStageHelper, _banking_packet| {
                helper.send_new_task(helper.create_new_task(tx0.clone(), 17))
            });
        pool.register_banking_stage(
            banking_packet_receiver,
            fixed_banking_packet_handler,
            transaction_recorder,
        );

        // Confirm the banking packet channel is cleared, even before taking scheduler
        sleepless_testing::at(TestCheckPoint::AfterNewBufferedTask);
        assert_eq!(banking_packet_sender.len(), 0);

        assert_eq!(bank.transaction_count(), 0);
        let context = SchedulingContext::for_production(bank.clone());
        let scheduler = pool.take_scheduler(context);
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        assert_matches!(bank.wait_for_completed_scheduler(), Some((Ok(()), _)));
        assert_eq!(bank.transaction_count(), 1);

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }

    #[test]
    fn test_block_production_scheduler_buffering_before_new_session() {
        solana_logger::setup();

        let _progress = sleepless_testing::setup(&[
            &CheckPoint::NewBufferedTask(18),
            &TestCheckPoint::AfterNewBufferedTask,
        ]);

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config_for_block_production(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new(None, None, None, None, ignored_prioritization_fee_cache);

        let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let (exit, _poh_recorder, transaction_recorder, poh_service, _signal_receiver) =
            create_test_recorder_with_index_tracking(
                bank.clone(),
                blockstore.clone(),
                None,
                Some(leader_schedule_cache),
            );

        // Create a dummy handler which unconditionally sends tx0 back to the scheduler thread
        let tx0 = RuntimeTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &solana_pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));
        let fixed_banking_packet_handler =
            Box::new(move |helper: &BankingStageHelper, _banking_packet| {
                helper.send_new_task(helper.create_new_task(tx0.clone(), 18))
            });

        let (banking_packet_sender, banking_packet_receiver) = crossbeam_channel::unbounded();
        pool.register_banking_stage(
            banking_packet_receiver,
            fixed_banking_packet_handler,
            transaction_recorder,
        );

        // Quickly take and return the scheduler so that this test can test the behavior while
        // waiting for new session...
        let context = SchedulingContext::for_production(bank.clone());
        let scheduler = pool.take_scheduler(context.clone());
        let bank_tmp = BankWithScheduler::new(bank.clone(), Some(scheduler));
        assert_matches!(bank_tmp.wait_for_completed_scheduler(), Some((Ok(()), _)));

        // Send fake packet batch to trigger banking_packet_handler
        assert_eq!(banking_packet_sender.len(), 0);
        banking_packet_sender
            .send(BankingPacketBatch::default())
            .unwrap();

        // Confirm the banking packet channel is cleared, even before taking scheduler
        sleepless_testing::at(TestCheckPoint::AfterNewBufferedTask);
        assert_eq!(banking_packet_sender.len(), 0);

        assert_eq!(bank.transaction_count(), 0);
        let scheduler = pool.take_scheduler(context);
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        assert_matches!(bank.wait_for_completed_scheduler(), Some((Ok(()), _)));
        assert_eq!(bank.transaction_count(), 1);

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }

    #[test]
    #[should_panic(expected = "register_banking_stage() isn't called yet")]
    fn test_block_production_scheduler_take_without_registering() {
        solana_logger::setup();

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new(None, None, None, None, ignored_prioritization_fee_cache);
        let bank = Arc::new(Bank::default_for_tests());
        let context = &SchedulingContext::for_production(bank);
        let scheduler = pool.do_take_scheduler(context.clone());
        Box::new(scheduler.into_inner().1).return_to_pool();
    }

    #[test]
    #[should_panic(expected = "cannot take: Taken(0)")]
    fn test_block_production_scheduler_double_take_without_returning() {
        solana_logger::setup();

        let GenesisConfigInfo { genesis_config, .. } =
            create_genesis_config_for_block_production(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new(None, None, None, None, ignored_prioritization_fee_cache);

        let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let (exit, _poh_recorder, transaction_recorder, poh_service, _signal_receiver) =
            create_test_recorder_with_index_tracking(
                bank.clone(),
                blockstore.clone(),
                None,
                Some(leader_schedule_cache),
            );
        let (_banking_packet_sender, banking_packet_receiver) = crossbeam_channel::unbounded();
        pool.register_banking_stage(
            banking_packet_receiver,
            Box::new(|_, _| unreachable!()),
            transaction_recorder,
        );

        let context = SchedulingContext::for_production(bank.clone());
        let scheduler1 = pool.do_take_scheduler(context.clone());
        let scheduler2 = pool.do_take_scheduler(context);

        Box::new(scheduler1.into_inner().1).return_to_pool();
        Box::new(scheduler2.into_inner().1).return_to_pool();

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }

    #[test]
    fn test_block_production_scheduler_drop_overgrown() {
        solana_logger::setup();

        let GenesisConfigInfo { genesis_config, .. } =
            create_genesis_config_for_block_production(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        const REDUCED_MAX_USAGE_QUEUE_COUNT: usize = 0;
        let pool = DefaultSchedulerPool::do_new(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
            SHORTENED_POOL_CLEANER_INTERVAL,
            DEFAULT_MAX_POOLING_DURATION,
            REDUCED_MAX_USAGE_QUEUE_COUNT,
            DEFAULT_TIMEOUT_DURATION,
        );

        let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let (exit, _poh_recorder, transaction_recorder, poh_service, _signal_receiver) =
            create_test_recorder_with_index_tracking(
                bank.clone(),
                blockstore.clone(),
                None,
                Some(leader_schedule_cache),
            );

        let (_banking_packet_sender, banking_packet_receiver) = crossbeam_channel::unbounded();
        pool.register_banking_stage(
            banking_packet_receiver,
            Box::new(|_, _| unreachable!()),
            transaction_recorder,
        );

        let context = SchedulingContext::for_production(bank);
        let scheduler = pool.do_take_scheduler(context.clone());
        let trashed_old_scheduler_id = scheduler.id();

        // Make scheduler overgrown and trash it by returning
        scheduler
            .inner
            .usage_queue_loader
            .load(Pubkey::new_unique());
        Box::new(scheduler.into_inner().1).return_to_pool();

        // Re-take a brand-new one
        let scheduler = pool.do_take_scheduler(context);
        let respawned_new_scheduler_id = scheduler.id();
        Box::new(scheduler.into_inner().1).return_to_pool();

        // id should be different
        assert_ne!(trashed_old_scheduler_id, respawned_new_scheduler_id);

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }

    #[test]
    fn test_block_production_scheduler_return_block_verification_scheduler_while_pooled() {
        solana_logger::setup();

        let GenesisConfigInfo { genesis_config, .. } =
            create_genesis_config_for_block_production(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new(None, None, None, None, ignored_prioritization_fee_cache);

        let (_banking_packet_sender, banking_packet_receiver) = crossbeam_channel::unbounded();
        let (ledger_path, _blockhash) = create_new_tmp_ledger_auto_delete!(&genesis_config);
        let blockstore = Arc::new(Blockstore::open(ledger_path.path()).unwrap());
        let leader_schedule_cache = Arc::new(LeaderScheduleCache::new_from_bank(&bank));
        let (exit, _poh_recorder, transaction_recorder, poh_service, _signal_receiver) =
            create_test_recorder_with_index_tracking(
                bank.clone(),
                blockstore.clone(),
                None,
                Some(leader_schedule_cache),
            );
        pool.register_banking_stage(
            banking_packet_receiver,
            Box::new(|_, _| unreachable!()),
            transaction_recorder,
        );

        // Make sure the assertion in BlockProductionSchedulerInner::can_put() doesn't cause false
        // positives...
        let context = SchedulingContext::for_verification(bank.clone());
        let scheduler = pool.take_scheduler(context);
        let bank_tmp = BankWithScheduler::new(bank, Some(scheduler));
        assert_matches!(bank_tmp.wait_for_completed_scheduler(), Some((Ok(()), _)));

        exit.store(true, Ordering::Relaxed);
        poh_service.join().unwrap();
    }
}
