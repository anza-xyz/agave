#![allow(clippy::arithmetic_side_effects)]
//! Transaction scheduling code.
//!
//! This crate implements 3 solana-runtime traits (`InstalledScheduler`, `UninstalledScheduler` and
//! `InstalledSchedulerPool`) to provide a concrete transaction scheduling implementation
//! (including executing txes and committing tx results).
//!
//! At the highest level, this crate takes `SanitizedTransaction`s via its `schedule_execution()`
//! and commits any side-effects (i.e. on-chain state changes) into the associated `Bank` via
//! `solana-ledger`'s helper function called `execute_batch()`.

#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    assert_matches::assert_matches,
    crossbeam_channel::{self, never, select, select_biased, Receiver, RecvError, SendError, Sender},
    dashmap::{DashMap, DashSet},
    derivative::Derivative,
    log::*,
    scopeguard::defer,
    solana_ledger::blockstore_processor::{
        execute_batch, TransactionBatchWithIndexes, TransactionStatusSender,
    },
    solana_poh::poh_recorder::TransactionRecorder,
    solana_runtime::{
        installed_scheduler_pool::{
            initialized_result_with_timings, InstalledScheduler, InstalledSchedulerBox,
            InstalledSchedulerPool, InstalledSchedulerPoolArc, ResultWithTimings, ScheduleResult,
            SchedulerAborted, SchedulerId, SchedulingContext, TimeoutListener, UninstalledScheduler,
            UninstalledSchedulerBox,
        },
        prioritization_fee_cache::PrioritizationFeeCache,
        vote_sender_types::ReplayVoteSender,
    },
    solana_sdk::{
        hash::Hash,
        pubkey::Pubkey,
        scheduling::SchedulingMode,
        transaction::{Result, SanitizedTransaction, TransactionError},
    },
    solana_timings::ExecuteTimings,
    solana_unified_scheduler_logic::{SchedulingStateMachine, Task, UsageQueue},
    static_assertions::const_assert_eq,
    std::{
        fmt::Debug,
        marker::PhantomData,
        mem,
        sync::{
            atomic::{AtomicU64, Ordering::Relaxed},
            Arc, Mutex, OnceLock, Weak,
        },
        thread::{self, sleep, JoinHandle},
        time::{Duration, Instant},
    },
    vec_extract_if_polyfill::MakeExtractIf,
};
use solana_sdk::scheduling::TaskKey;
use solana_perf::packet::BankingPacketBatch;
use solana_perf::packet::BankingPacketReceiver;
use std::sync::Condvar;
use std::sync::RwLock;
use dyn_clone::DynClone;
use dyn_clone::clone_trait_object;

mod sleepless_testing;
use crate::sleepless_testing::BuilderTracked;

// dead_code is false positive; these tuple fields are used via Debug.
#[allow(dead_code)]
#[derive(Debug)]
enum CheckPoint {
    NewTask(TaskKey),
    TaskHandled(TaskKey),
    SchedulerThreadAborted,
    IdleSchedulerCleaned(usize),
    TrashedSchedulerCleaned(usize),
    TimeoutListenerTriggered(usize),
}

type AtomicSchedulerId = AtomicU64;

#[derive(Debug)]
pub enum SupportedSchedulingMode {
    Either(SchedulingMode),
    Both,
}

impl SupportedSchedulingMode {
    fn is_supported(&self, requested_mode: SchedulingMode) -> bool {
        match (self, requested_mode) {
            (Self::Both, _) => true,
            (Self::Either(ref supported), ref requested) if supported == requested => true,
            _ => false,
        }
    }

    fn block_verification_only() -> Self {
        Self::Either(SchedulingMode::BlockVerification)
    }
}

// SchedulerPool must be accessed as a dyn trait from solana-runtime, because SchedulerPool
// contains some internal fields, whose types aren't available in solana-runtime (currently
// TransactionStatusSender; also, PohRecorder in the future)...
#[derive(Debug)]
pub struct SchedulerPool<S: SpawnableScheduler<TH>, TH: TaskHandler> {
    supported_scheduling_mode: SupportedSchedulingMode,
    scheduler_inners: Mutex<Vec<(S::Inner, Instant)>>,
    block_production_scheduler_inner: Mutex<(Option<SchedulerId>, Option<S::Inner>, Option<SchedulingContext>)>,
    block_production_scheduler_condvar: Condvar,
    block_production_scheduler_respawner: Mutex<Option<BlockProductionSchedulerRespawner>>,
    trashed_scheduler_inners: Mutex<Vec<S::Inner>>,
    timeout_listeners: Mutex<Vec<(TimeoutListener, Instant)>>,
    handler_count: usize,
    handler_context: HandlerContext,
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

#[derive(Debug)]
pub struct HandlerContext {
    log_messages_bytes_limit: Option<usize>,
    transaction_status_sender: Option<TransactionStatusSender>,
    replay_vote_sender: Option<ReplayVoteSender>,
    prioritization_fee_cache: Arc<PrioritizationFeeCache>,
    transaction_recorder: TransactionRecorder,
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

pub trait AAA: DynClone + (FnMut(BankingPacketBatch) -> Vec<Task>) + Send {
}

impl<T> AAA for T
where
    T: DynClone + (FnMut(BankingPacketBatch) -> Vec<Task>) + Send {
}

clone_trait_object!(AAA);

type BBB = Box<dyn (FnMut(Arc<BankingStageAdapter>) -> Box<dyn AAA>) + Send>;

struct BlockProductionSchedulerRespawner {
    on_spawn_block_production_scheduler: BBB,
    bank_forks: Arc<RwLock<BankForks>>,
    banking_packet_receiver: BankingPacketReceiver,
}

impl std::fmt::Debug for BlockProductionSchedulerRespawner {
    fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        todo!();
    }
}


impl<S, TH> SchedulerPool<S, TH>
where
    S: SpawnableScheduler<TH>,
    TH: TaskHandler,
{
    // Some internal impl and test code want an actual concrete type, NOT the
    // `dyn InstalledSchedulerPool`. So don't merge this into `Self::new_dyn()`.
    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    pub fn new(
        supported_scheduling_mode: SupportedSchedulingMode,
        handler_count: Option<usize>,
        log_messages_bytes_limit: Option<usize>,
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: Option<ReplayVoteSender>,
        prioritization_fee_cache: Arc<PrioritizationFeeCache>,
        transaction_recorder: TransactionRecorder,
    ) -> Arc<Self> {
        Self::do_new(
            supported_scheduling_mode,
            handler_count,
            log_messages_bytes_limit,
            transaction_status_sender,
            replay_vote_sender,
            prioritization_fee_cache,
            transaction_recorder,
            DEFAULT_POOL_CLEANER_INTERVAL,
            DEFAULT_MAX_POOLING_DURATION,
            DEFAULT_MAX_USAGE_QUEUE_COUNT,
            DEFAULT_TIMEOUT_DURATION,
        )
    }

    pub fn new_for_verification(
        handler_count: Option<usize>,
        log_messages_bytes_limit: Option<usize>,
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: Option<ReplayVoteSender>,
        prioritization_fee_cache: Arc<PrioritizationFeeCache>,
    ) -> Arc<Self> {
        Self::new(
            SupportedSchedulingMode::block_verification_only(),
            handler_count,
            log_messages_bytes_limit,
            transaction_status_sender,
            replay_vote_sender,
            prioritization_fee_cache,
            TransactionRecorder::new_dummy(),
        )
    }

    fn do_new_for_verification(
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
        Self::do_new(
            SupportedSchedulingMode::block_verification_only(),
            handler_count,
            log_messages_bytes_limit,
            transaction_status_sender,
            replay_vote_sender,
            prioritization_fee_cache,
            pool_cleaner_interval,
            max_pooling_duration,
            max_usage_queue_count,
            timeout_duration,
            TransactionRecorder::new_dummy(),
        )
    }

    fn do_new(
        supported_scheduling_mode: SupportedSchedulingMode,
        handler_count: Option<usize>,
        log_messages_bytes_limit: Option<usize>,
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: Option<ReplayVoteSender>,
        prioritization_fee_cache: Arc<PrioritizationFeeCache>,
        transaction_recorder: TransactionRecorder,
        pool_cleaner_interval: Duration,
        max_pooling_duration: Duration,
        max_usage_queue_count: usize,
        timeout_duration: Duration,
    ) -> Arc<Self> {
        let handler_count = handler_count.unwrap_or(Self::default_handler_count());
        assert!(handler_count >= 1);

        let scheduler_pool = Arc::new_cyclic(|weak_self| Self {
            supported_scheduling_mode,
            scheduler_inners: Mutex::default(),
            block_production_scheduler_inner: Mutex::default(),
            block_production_scheduler_condvar: Condvar::default(),
            block_production_scheduler_respawner: Mutex::default(),
            trashed_scheduler_inners: Mutex::default(),
            timeout_listeners: Mutex::default(),
            handler_count,
            handler_context: HandlerContext {
                log_messages_bytes_limit,
                transaction_status_sender,
                replay_vote_sender,
                prioritization_fee_cache,
                transaction_recorder,
            },
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
                    #[allow(unstable_name_collisions)]
                    idle_inners.extend(scheduler_inners.extract_if(|(_inner, pooled_at)| {
                        now.duration_since(*pooled_at) > max_pooling_duration
                    }));
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
                    #[allow(unstable_name_collisions)]
                    expired_listeners.extend(timeout_listeners.extract_if(
                        |(_callback, registered_at)| {
                            now.duration_since(*registered_at) > timeout_duration
                        },
                    ));
                    drop(timeout_listeners);

                    let count = expired_listeners.len();
                    for (timeout_listener, _registered_at) in expired_listeners {
                        timeout_listener.trigger(scheduler_pool.clone());
                    }
                    count
                };

                let mut g = scheduler_pool.block_production_scheduler_inner.lock().unwrap();
                if let Some(pooled) = &g.1 {
                    match pooled.banking_stage_status() {
                        BankingStageStatus::Active => (),
                        BankingStageStatus::Inactive => {
                            info!("sch {} IS idle", pooled.id());
                            if pooled.is_overgrown(false) {
                                info!("sch {} is overgrown!", pooled.id());
                                let pooled = g.1.take().unwrap();
                                assert_eq!(Some(pooled.id()), g.0.take());
                                drop(g);
                                let id = pooled.id();
                                info!("dropping sch {id}");
                                drop(pooled);
                                info!("dropped sch {id}");
                                scheduler_pool.spawn_block_production_scheduler();
                            } else {
                                info!("sch {} isn't overgrown", pooled.id());
                                pooled.reset();
                            }
                        }
                        BankingStageStatus::Exited => {
                            let pooled = g.1.take().unwrap();
                            assert_eq!(Some(pooled.id()), g.0.take());
                            drop(g);
                            let id = pooled.id();
                            info!("dropping sch {id}");
                            drop(pooled);
                            info!("dropped sch {id}");
                            scheduler_pool.reset_respawner();
                        }
                    }
                }

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
        supported_scheduling_mode: SupportedSchedulingMode,
        handler_count: Option<usize>,
        log_messages_bytes_limit: Option<usize>,
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: Option<ReplayVoteSender>,
        prioritization_fee_cache: Arc<PrioritizationFeeCache>,
        transaction_recorder: TransactionRecorder,
    ) -> InstalledSchedulerPoolArc {
        Self::new(
            supported_scheduling_mode,
            handler_count,
            log_messages_bytes_limit,
            transaction_status_sender,
            replay_vote_sender,
            prioritization_fee_cache,
            transaction_recorder,
        )
    }

    pub fn new_dyn_for_verification(
        handler_count: Option<usize>,
        log_messages_bytes_limit: Option<usize>,
        transaction_status_sender: Option<TransactionStatusSender>,
        replay_vote_sender: Option<ReplayVoteSender>,
        prioritization_fee_cache: Arc<PrioritizationFeeCache>,
    ) -> InstalledSchedulerPoolArc {
        Self::new(
            SupportedSchedulingMode::block_verification_only(),
            handler_count,
            log_messages_bytes_limit,
            transaction_status_sender,
            replay_vote_sender,
            prioritization_fee_cache,
            TransactionRecorder::new_dummy(),
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
    fn return_scheduler(&self, scheduler: S::Inner, should_trash: bool) {
        let id = scheduler.id();
        debug!("return_scheduler(): id: {id} should_trash: {should_trash}");
        let mut g = self.block_production_scheduler_inner.lock().unwrap();
        let bp_id: Option<SchedulerId> = g.0.as_ref().copied();
        let is_block_production_scheduler_returned = Some(id) == bp_id;

        if should_trash {
            // Delay drop()-ing this trashed returned scheduler inner by stashing it in
            // self.trashed_scheduler_inners, which is periodically drained by the `solScCleaner`
            // thread. Dropping it could take long time (in fact,
            // PooledSchedulerInner::block_verification_usage_queue_loader can contain many entries to drop).
            self.trashed_scheduler_inners
                .lock()
                .expect("not poisoned")
                .push(scheduler);

            if is_block_production_scheduler_returned {
                g.0.take();
            }
            if is_block_production_scheduler_returned {
                drop(g);
                self.spawn_block_production_scheduler();
            }
        } else {
            drop(g);
            if !is_block_production_scheduler_returned {
                self.scheduler_inners
                    .lock()
                    .expect("not poisoned")
                    .push((scheduler, Instant::now()));
            } else {
                assert!(self.block_production_scheduler_inner.lock().unwrap().1.replace(scheduler).is_none());
            }
        }
    }

    #[cfg(test)]
    fn do_take_scheduler(&self, context: SchedulingContext) -> S {
        self.do_take_resumed_scheduler(context, initialized_result_with_timings(), None)
    }

    fn do_take_resumed_scheduler(
        &self,
        context: SchedulingContext,
        result_with_timings: ResultWithTimings,
    ) -> S {
        assert_matches!(result_with_timings, (Ok(_), _));

        if matches!(context.mode(), SchedulingMode::BlockVerification) {
            // pop is intentional for filo, expecting relatively warmed-up scheduler due to having been
            // returned recently
            if let Some((inner, _pooled_at)) = self.scheduler_inners.lock().expect("not poisoned").pop()
            {
                S::from_inner(inner, context, result_with_timings)
            } else {
                S::spawn(self.self_arc(), context, result_with_timings, None::<(_, fn(BankingPacketBatch) -> Vec<Task>)>, None)
            }
        } else {
            let mut g = self.block_production_scheduler_inner.lock().expect("not poisoned");
            g = self.block_production_scheduler_condvar.wait_while(g, |g| {
                let not_yet = g.0.is_none();
                if not_yet {
                    info!("will wait for bps...");
                    g.2 = Some(context.clone());
                }
                not_yet
            }).unwrap();
            if let Some(inner) = g.1.take()
            {
                S::from_inner(inner, context, result_with_timings)
            } else {
                panic!();
            }
        }
    }

    pub fn prepare_to_spawn_block_production_scheduler(&self, bank_forks: Arc<RwLock<BankForks>>, banking_packet_receiver: BankingPacketReceiver, on_spawn_block_production_scheduler: BBB) {
        *self.block_production_scheduler_respawner.lock().unwrap() = Some(BlockProductionSchedulerRespawner {
            bank_forks,
            banking_packet_receiver,
            on_spawn_block_production_scheduler,
        });
    }

    pub fn reset_respawner(&self) {
        *self.block_production_scheduler_respawner.lock().unwrap() = None;
    }

    pub fn spawn_block_production_scheduler(&self) {
        info!("flash session: start!");
        let mut respawner_write = self.block_production_scheduler_respawner.lock().unwrap();
        let BlockProductionSchedulerRespawner {
            bank_forks,
            banking_packet_receiver,
            on_spawn_block_production_scheduler,
        } = &mut *respawner_write.as_mut().unwrap();

        let adapter = Arc::new(BankingStageAdapter {
            usage_queue_loader: UsageQueueLoader::default(),
            transaction_deduper: DashSet::with_capacity(1_000_000),
            idling_detector: Mutex::default(),
        });

        let on_banking_packet_receive = on_spawn_block_production_scheduler(adapter.clone());
        let banking_stage_context = (banking_packet_receiver.clone(), on_banking_packet_receive);
        let scheduler = {
            let mut g = self.block_production_scheduler_inner.lock().expect("not poisoned");
            let context = g.2.take().inspect(|context| {
                assert_matches!(context.mode(), SchedulingMode::BlockProduction);
            }).unwrap_or_else(|| {
                SchedulingContext::new(SchedulingMode::BlockProduction, bank_forks.read().unwrap().root_bank())
            });
            let s = S::spawn(self.self_arc(), context, initialized_result_with_timings(), Some(banking_stage_context), Some(adapter));
            assert!(g.0.replace(s.id()).is_none());
            s
        };
        self.return_scheduler(scheduler.into_inner().1, false);
        self.block_production_scheduler_condvar.notify_all();
        info!("flash session: end!");
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn pooled_scheduler_count(&self) -> usize {
        self.scheduler_inners.lock().expect("not poisoned").len()
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

use solana_runtime::bank_forks::BankForks;

impl<S, TH> InstalledSchedulerPool for SchedulerPool<S, TH>
where
    S: SpawnableScheduler<TH>,
    TH: TaskHandler,
{
    fn take_resumed_scheduler(
        &self,
        context: SchedulingContext,
        result_with_timings: ResultWithTimings,
    ) -> Option<InstalledSchedulerBox> {
        if !self.supported_scheduling_mode.is_supported(context.mode()) {
            return None;
        }

        Some(Box::new(self.do_take_resumed_scheduler(context, result_with_timings)))
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
        transaction: &SanitizedTransaction,
        index: TaskKey,
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
        transaction: &SanitizedTransaction,
        index: TaskKey,
        handler_context: &HandlerContext,
    ) {
        let (cost, added_cost) = if matches!(scheduling_context.mode(), SchedulingMode::BlockProduction) {
            use solana_cost_model::cost_model::CostModel;
            let c = CostModel::calculate_cost(transaction, &scheduling_context.bank().feature_set);
            loop {
                let r = scheduling_context.bank().write_cost_tracker().unwrap().try_add(&c);
                if let Err(e) = r {
                    use solana_cost_model::cost_tracker::CostTrackerError;
                    if matches!(e, CostTrackerError::WouldExceedAccountDataBlockLimit) {
                        sleep(Duration::from_millis(10));
                        continue;
                    } else {
                        *result = Err(e.into());
                        break (Some(c), false)
                    }
                } else {
                    break (Some(c), true)
                }
            }
        } else {
            (None, false)
        };

        if result.is_ok() {
            // scheduler must properly prevent conflicting tx executions. thus, task handler isn't
            // responsible for locking.
            let batch = scheduling_context
                .bank()
                .prepare_unlocked_batch_from_single_tx(transaction);
            let batch_with_indexes = TransactionBatchWithIndexes {
                batch,
                transaction_indexes: vec![(index as usize)],
            };

            let pre_commit_callback = match scheduling_context.mode() {
                SchedulingMode::BlockVerification => None,
                SchedulingMode::BlockProduction => Some(|| {
                    let summary = handler_context.transaction_recorder
                        .record_transactions(
                            scheduling_context.bank().slot(),
                            vec![transaction.to_versioned_transaction()],
                        );
                    summary.result.is_ok()
                }),
            };

            *result = execute_batch(
                &batch_with_indexes,
                scheduling_context.bank(),
                handler_context.transaction_status_sender.as_ref(),
                handler_context.replay_vote_sender.as_ref(),
                timings,
                handler_context.log_messages_bytes_limit,
                &handler_context.prioritization_fee_cache,
                pre_commit_callback,
            );
        }

        if result.is_err() {
            if let Some(cost2) = cost {
                if added_cost {
                    scheduling_context.bank().write_cost_tracker().unwrap().remove(&cost2);
                }
            }
        }

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
}

// A very tiny generic message type to signal about opening and closing of subchannels, which are
// logically segmented series of Payloads (P1) over a single continuous time-span, potentially
// carrying some subchannel metadata (P2) upon opening a new subchannel.
// Note that the above properties can be upheld only when this is used inside MPSC or SPSC channels
// (i.e. the consumer side needs to be single threaded). For the multiple consumer cases,
// ChainedChannel can be used instead.
use enum_ptr::{Aligned, Compact, EnumPtr, Unit};

#[repr(C, usize)]
#[derive(Debug, EnumPtr)]
pub enum SubchanneledPayload<P1: Aligned, P2: Aligned> {
    Payload(P1),
    OpenSubchannel(P2),
    CloseSubchannel(Unit),
    Disconnect(Unit),
    Reset(Unit),
}

type NewTaskPayload = SubchanneledPayload<Task, Box<(SchedulingContext, ResultWithTimings)>>;
type CompactNewTaskPayload = Compact<NewTaskPayload>;
const_assert_eq!(mem::size_of::<NewTaskPayload>(), 16);
const_assert_eq!(mem::size_of::<CompactNewTaskPayload>(), 8);


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

    #[derive(EnumPtr)]
    #[repr(C, usize)]
    pub(super) enum ChainedChannel<P: Aligned, C> {
        Payload(P),
        ContextAndChannels(Box<(C, Receiver<Compact<ChainedChannel<P, C>>>, Receiver<P>)>),
    }

    impl<P: Aligned, C> ChainedChannel<P, C> {
        fn chain_to_new_channel(
            context: C,
            receiver: Receiver<Compact<Self>>,
            aux_receiver: Receiver<P>,
        ) -> Self {
            ChainedChannel::ContextAndChannels(Box::new((context, receiver, aux_receiver)))
        }
    }

    pub(super) struct ChainedChannelSender<P: Aligned, C> {
        sender: Sender<Compact<ChainedChannel<P, C>>>,
        aux_sender: Sender<P>,
    }

    #[allow(dead_code)]
    pub(super) trait WithMessageType {
        type ChannelMessage;
    }

    impl<P: Aligned, C: Clone> WithMessageType for ChainedChannelSender<P, C> {
        type ChannelMessage = ChainedChannel<P, C>;
    }

    impl<P: Aligned, C: Clone> ChainedChannelSender<P, C> {
        fn new(sender: Sender<Compact<ChainedChannel<P, C>>>, aux_sender: Sender<P>) -> Self {
            Self { sender, aux_sender }
        }

        pub(super) fn send_payload(
            &self,
            payload: P,
        ) -> std::result::Result<(), SendError<Compact<ChainedChannel<P, C>>>> {
            self.sender.send(ChainedChannel::Payload(payload).into())
        }

        /*
        pub(super) fn send_aux_payload(&self, payload: P) -> std::result::Result<(), SendError<P>> {
            self.aux_sender.send(payload)
        }
        */

        pub(super) fn send_chained_channel(
            &mut self,
            context: C,
            count: usize,
        ) -> std::result::Result<(), SendError<Compact<ChainedChannel<P, C>>>> {
            let (chained_sender, chained_receiver) = crossbeam_channel::unbounded();
            let (chained_aux_sender, chained_aux_receiver) = crossbeam_channel::unbounded();
            for _ in 0..count {
                self.sender.send(
                    ChainedChannel::chain_to_new_channel(
                        context.clone(),
                        chained_receiver.clone(),
                        chained_aux_receiver.clone(),
                    )
                    .into(),
                )?
            }
            self.sender = chained_sender;
            self.aux_sender = chained_aux_sender;
            Ok(())
        }

        pub(super) fn len(&self) -> usize {
            self.sender.len()
        }

        pub(super) fn aux_len(&self) -> usize {
            self.aux_sender.len()
        }
    }

    // P doesn't need to be `: Clone`, yet rustc derive can't handle it.
    // see https://github.com/rust-lang/rust/issues/26925
    #[derive(Derivative)]
    #[derivative(Clone(bound = "C: Clone"))]
    pub(super) struct ChainedChannelReceiver<P: Aligned, C: Clone> {
        receiver: Receiver<Compact<ChainedChannel<P, C>>>,
        aux_receiver: Receiver<P>,
        context: C,
    }

    impl<P: Aligned, C: Clone> ChainedChannelReceiver<P, C> {
        fn new(
            receiver: Receiver<Compact<ChainedChannel<P, C>>>,
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

        pub(super) fn for_select(&self) -> &Receiver<Compact<ChainedChannel<P, C>>> {
            &self.receiver
        }

        /*
        pub(super) fn aux_for_select(&self) -> &Receiver<P> {
            &self.aux_receiver
        }

        pub(super) fn never_receive_from_aux(&mut self) {
            self.aux_receiver = never();
        }
        */

        pub(super) fn after_select(&mut self, message: ChainedChannel<P, C>) -> Option<P> {
            match message {
                ChainedChannel::Payload(payload) => Some(payload),
                ChainedChannel::ContextAndChannels(b) => {
                    let (context, channel, idle_channel) = *b;

                    self.context = context;
                    self.receiver = channel;
                    self.aux_receiver = idle_channel;
                    None
                }
            }
        }
    }

    pub(super) fn unbounded<P: Aligned, C: Clone>(
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
/// Currently, the simplest implementation. This grows memory usage in unbounded way. Cleaning will
/// be added later. This struct is here to be put outside `solana-unified-scheduler-logic` for the
/// crate's original intent (separation of logics from this crate). Some practical and mundane
/// pruning will be implemented in this type.
#[derive(Default, Debug)]
pub struct UsageQueueLoader {
    usage_queues: DashMap<Pubkey, UsageQueue, ahash::RandomState>,
}

impl UsageQueueLoader {
    pub fn load(&self, address: Pubkey) -> UsageQueue {
        // taken from https://github.com/xacrimon/dashmap/issues/292#issuecomment-1916621009
        match self.usage_queues.get(&address) {
            Some(bar_read_guard) => bar_read_guard.value().clone(),
            None => self.usage_queues.entry(address).or_default().clone(),
        }
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

#[derive(Debug)]
pub struct PooledScheduler<TH: TaskHandler> {
    inner: PooledSchedulerInner<Self, TH>,
    context: SchedulingContext,
}

#[derive(Debug)]
enum TaskCreator {
    BlockVerification {
        usage_queue_loader: UsageQueueLoader,
    },
    BlockProduction {
        banking_stage_adapter: Arc<BankingStageAdapter>,
    },
}

impl TaskCreator {
    fn usage_queue_loader(&self) -> &UsageQueueLoader {
        use TaskCreator::*;

        match self {
            BlockVerification { usage_queue_loader } => usage_queue_loader,
            BlockProduction { banking_stage_adapter } => &banking_stage_adapter.usage_queue_loader,
        }
    }

    fn banking_stage_status(&self) -> BankingStageStatus {
        use TaskCreator::*;

        match self {
            BlockVerification { usage_queue_loader: _ } => todo!(),
            BlockProduction { banking_stage_adapter } => banking_stage_adapter.banking_stage_status(),
        }
    }

    fn reset(&self) {
        use TaskCreator::*;

        match self {
            BlockVerification { usage_queue_loader: _ } => todo!(),
            BlockProduction { banking_stage_adapter } => banking_stage_adapter.reset(),
        }
    }

    fn is_overgrown(&self, max_usage_queue_count: usize, on_hot_path: bool) -> bool {
        use TaskCreator::*;

        match self {
            BlockVerification { usage_queue_loader } => {
                assert!(on_hot_path);
                // This check must be done on hot path everytime scheduler are returned to reliably
                // detect too large loaders...
                usage_queue_loader.count() > max_usage_queue_count
            }
            BlockProduction { banking_stage_adapter } => {
                if on_hot_path {
                    // the slow path can be ensured to be called periodically.
                    false
                } else {
                    let current_usage_queue_count = banking_stage_adapter.usage_queue_loader.count();
                    let current_transaction_count = banking_stage_adapter.transaction_deduper.len();
                    info!("bsa: {current_usage_queue_count} {current_transaction_count}");

                    current_usage_queue_count > max_usage_queue_count || current_transaction_count > 1_000_000
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct PooledSchedulerInner<S: SpawnableScheduler<TH>, TH: TaskHandler> {
    thread_manager: ThreadManager<S, TH>,
    task_creator: TaskCreator,
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
        self.new_task_sender = Arc::new(crossbeam_channel::unbounded().0);

        self.ensure_join_threads(true);
        assert_matches!(self.session_result_with_timings, Some((Ok(_), _)));
    }
}

impl<S, TH> PooledSchedulerInner<S, TH>
where
    S: SpawnableScheduler<TH, Inner = Self>,
    TH: TaskHandler,
{
    fn is_trashed(&self, on_hot_path: bool) -> bool {
        self.is_aborted() || self.is_overgrown(on_hot_path)
    }

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
        // for any scheudler which is taken out of the pool.
        //
        // Thus, any transaction errors are always handled without loss of information and
        // the aborted scheduler itself will always be handled as _trashed_ before returning the
        // scheduler to the pool, considering is_trashed() is checked immediately before that.
        self.thread_manager.are_threads_joined()
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
    new_task_sender: Arc<Sender<CompactNewTaskPayload>>,
    new_task_receiver: Option<Receiver<CompactNewTaskPayload>>,
    session_result_sender: Sender<ResultWithTimings>,
    session_result_receiver: Receiver<ResultWithTimings>,
    session_result_with_timings: Option<ResultWithTimings>,
    scheduler_thread: Option<JoinHandle<()>>,
    handler_threads: Vec<JoinHandle<()>>,
}

#[derive(Default)]
struct LogInterval(usize);

impl LogInterval {
    fn increment(&mut self) -> bool {
        self.0 = self.0.checked_add(1).unwrap();
        self.0 % 2000 == 0
    }
}

struct HandlerPanicked;
type HandlerResult = std::result::Result<Box<ExecutedTask>, HandlerPanicked>;
const_assert_eq!(mem::size_of::<HandlerResult>(), 8);

impl<S: SpawnableScheduler<TH>, TH: TaskHandler> ThreadManager<S, TH> {
    fn new(pool: Arc<SchedulerPool<S, TH>>) -> Self {
        let (new_task_sender, new_task_receiver) = crossbeam_channel::unbounded();
        let (session_result_sender, session_result_receiver) = crossbeam_channel::unbounded();
        let handler_count = pool.handler_count;

        Self {
            scheduler_id: pool.new_scheduler_id(),
            pool,
            new_task_sender: Arc::new(new_task_sender),
            new_task_receiver: Some(new_task_receiver),
            session_result_sender,
            session_result_receiver,
            session_result_with_timings: None,
            scheduler_thread: None,
            handler_threads: Vec::with_capacity(handler_count),
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
            executed_task.task.transaction(),
            executed_task.task.task_index(),
            handler_context,
        );
    }

    #[must_use]
    fn accumulate_result_with_timings(
        context: &SchedulingContext,
        (result, timings): &mut ResultWithTimings,
        executed_task: HandlerResult,
        error_count: &mut u32,
        already_finishing: bool,
    ) -> Option<(Box<ExecutedTask>, bool)> {
        let Ok(executed_task) = executed_task else {
            return None;
        };
        timings.accumulate(&executed_task.result_with_timings.1);
        match context.mode() {
            SchedulingMode::BlockVerification => match executed_task.result_with_timings.0 {
                Ok(()) => Some((executed_task, false)),
                Err(error) => {
                    error!("error is detected while accumulating....: {error:?}");
                    *result = Err(error);
                    None
                }
            },
            SchedulingMode::BlockProduction => {
                match executed_task.result_with_timings.0 {
                Ok(()) => Some((executed_task, false)),
                Err(TransactionError::CommitFailed) => {
                    if !already_finishing {
                        info!("maybe reached max tick height...");
                    }
                    *error_count += 1;
                    Some((executed_task, true))
                }
                Err(ref e @ TransactionError::WouldExceedMaxBlockCostLimit) |
                Err(ref e @ TransactionError::WouldExceedMaxVoteCostLimit) |
                Err(ref e @ TransactionError::WouldExceedMaxAccountCostLimit) |
                Err(ref e @ TransactionError::WouldExceedAccountDataBlockLimit) => {
                    if !already_finishing {
                        info!("hit block cost: {e:?}");
                    }
                    *error_count += 1;
                    Some((executed_task, true))
                }
                Err(ref error) => {
                    debug!("error is detected while accumulating....: {error:?}");
                    *error_count += 1;
                    Some((executed_task, false))
                }
            }},
        }
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
        mut context: SchedulingContext,
        mut result_with_timings: ResultWithTimings,
        banking_stage_context: Option<(BankingPacketReceiver, impl FnMut(BankingPacketBatch) -> Vec<Task> + Clone + Send + 'static)>,
    ) {
        let scheduler_id = self.scheduler_id;
        let mut slot = context.bank().slot();

        let postfix = match context.mode() {
            SchedulingMode::BlockVerification => "V",
            SchedulingMode::BlockProduction => "P",
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
        let mode = context.mode();
        use crate::chained_channel::{ChainedChannelSender, WithMessageType};
        type RunnableTaskSender = ChainedChannelSender<Task, SchedulingContext>;
        let (mut runnable_task_sender, runnable_task_receiver): (RunnableTaskSender, _) =
            chained_channel::unbounded(context.clone());
        const_assert_eq!(
            mem::size_of::<<RunnableTaskSender as WithMessageType>::ChannelMessage>(),
            16
        );
        const_assert_eq!(
            mem::size_of::<Compact<<RunnableTaskSender as WithMessageType>::ChannelMessage>>(),
            8
        );
        // Create two handler-to-scheduler channels to prioritize the finishing of blocked tasks,
        // because it is more likely that a blocked task will have more blocked tasks behind it,
        // which should be scheduled while minimizing the delay to clear buffered linearized runs
        // as fast as possible.
        let (finished_blocked_task_sender, finished_blocked_task_receiver) =
            crossbeam_channel::unbounded::<HandlerResult>();
        //let (finished_idle_task_sender, finished_idle_task_receiver) =
        //    crossbeam_channel::unbounded::<HandlerResult>();

        assert_matches!(self.session_result_with_timings, None);

        // High-level flow of new tasks:
        // 1. the replay stage thread send a new task.
        // 2. the scheduler thread accepts the task.
        // 3. the scheduler thread dispatches the task after proper locking.
        // 4. the handler thread processes the dispatched task.
        // 5. the handler thread reply back to the scheduler thread as an executed task.
        // 6. the scheduler thread post-processes the executed task.
        let scheduler_main_loop = {
            let banking_stage_context = banking_stage_context.clone();
            let handler_count = self.pool.handler_count;
            let session_result_sender = self.session_result_sender.clone();
            // Taking new_task_receiver here is important to ensure there's a single receiver. In
            // this way, the replay stage will get .send() failures reliably, after this scheduler
            // thread died along with the single receiver.
            let new_task_receiver = self
                .new_task_receiver
                .take()
                .expect("no 2nd start_threads()");

            let mut session_ending = false;
            let (mut session_pausing, mut is_finished) = if context.mode() == SchedulingMode::BlockProduction {
                (true, true)
            } else {
                (false, false)
            };
            let mut session_resetting = false;

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
                        mode,
                    )
                };
                let mut log_interval = LogInterval::default();
                let mut session_started_at = Instant::now();
                let mut cpu_session_started_at = cpu_time::ThreadTime::now();
                let (mut log_reported_at, mut reported_task_total, mut reported_executed_task_total) = (session_started_at, 0, 0);
                let mut cpu_log_reported_at = cpu_session_started_at;
                let mut error_count: u32 = 0;

                let (banking_packet_receiver, _on_recv) = banking_stage_context.unzip();
                let banking_packet_receiver = banking_packet_receiver.unwrap_or_else(|| never());

                macro_rules! log_scheduler {
                    ($level:ident, $prefix:tt) => {
                        $level! {
                            "sch: {}: slot: {}({})[{:12}]({}{}): state_machine(({}({}b{}B{}F)=>{}({}+{}))/{}|{}TB|{}Lr) channels(<{} >{}+{} <{}+{} <B{}) {}",
                            scheduler_id, slot,
                            match state_machine.mode() {
                                SchedulingMode::BlockVerification => "v",
                                SchedulingMode::BlockProduction => "p",
                            },
                            $prefix,
                            (if session_ending {"S"} else {"-"}),
                            (if session_pausing {"P"} else {"-"}),
                            state_machine.alive_task_count(),
                            state_machine.blocked_task_count(), state_machine.buffered_task_queue_count(), state_machine.eager_lock_total(),
                            state_machine.executed_task_total(), state_machine.executed_task_total() - error_count, error_count,
                            state_machine.task_total(),
                            state_machine.buffered_task_total(),
                            state_machine.reblocked_lock_total(),
                            new_task_receiver.len(),
                            runnable_task_sender.len(), runnable_task_sender.aux_len(),
                            finished_blocked_task_receiver.len(), 0 /*finished_idle_task_receiver.len()*/,
                            banking_packet_receiver.len(),
                            {
                                let now = Instant::now();
                                let cpu_now = cpu_time::ThreadTime::now();
                                let session_elapsed_us = now.duration_since(session_started_at).as_micros();
                                let cpu_session_elapsed_us = cpu_now.duration_since(cpu_session_started_at).as_micros();
                                let log_elapsed_us = now.duration_since(log_reported_at).as_micros();
                                let cpu_log_elapsed_us = cpu_now.duration_since(cpu_log_reported_at).as_micros();

                                let l = format!(
                                    "tps({}us|{}us): ({}|{}) ({}us|{}us): ({}|{})",
                                    log_elapsed_us,
                                    session_elapsed_us,
                                    if log_elapsed_us > 0 {
                                        format!(
                                            "<{}>{}",
                                            1_000_000_u128 * ((state_machine.task_total() - reported_task_total) as u128) / log_elapsed_us,
                                            1_000_000_u128 * ((state_machine.executed_task_total() - reported_executed_task_total) as u128) / log_elapsed_us,
                                        )
                                    } else { "-".to_string() },
                                    if session_elapsed_us > 0 {
                                        format!(
                                            "<{}>{}",
                                            1_000_000_u128 * (state_machine.task_total() as u128) / session_elapsed_us,
                                            1_000_000_u128 * (state_machine.executed_task_total() as u128) / session_elapsed_us,
                                        )
                                    } else { "-".to_string() },
                                    cpu_log_elapsed_us,
                                    cpu_session_elapsed_us,
                                    if cpu_log_elapsed_us > 0 {
                                        format!(
                                            "<{}>{}",
                                            1_000_000_u128 * ((state_machine.task_total() - reported_task_total) as u128) / cpu_log_elapsed_us,
                                            1_000_000_u128 * ((state_machine.executed_task_total() - reported_executed_task_total) as u128) / cpu_log_elapsed_us,
                                        )
                                    } else { "-".to_string() },
                                    if cpu_session_elapsed_us > 0 {
                                        format!(
                                            "<{}>{}",
                                            1_000_000_u128 * (state_machine.task_total() as u128) / cpu_session_elapsed_us,
                                            1_000_000_u128 * (state_machine.executed_task_total() as u128) / cpu_session_elapsed_us,
                                        )
                                    } else { "-".to_string() },
                                );
                                #[allow(unused_assignments)]
                                {
                                    (log_reported_at, reported_task_total, reported_executed_task_total) = (now, state_machine.task_total(), state_machine.executed_task_total());
                                }
                                cpu_log_reported_at = cpu_now;
                                l
                            },
                        }
                    }
                }

                if !is_finished {
                    log_scheduler!(info, "started");
                }

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
                        let dummy_buffered_task_receiver =
                            dummy_receiver(state_machine.has_runnable_task() && !session_pausing);

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
                        let step_type = select! {
                            recv(finished_blocked_task_receiver) -> executed_task => {
                                let Some((executed_task, should_pause)) = Self::accumulate_result_with_timings(
                                    &context,
                                    &mut result_with_timings,
                                    executed_task.expect("alive handler"),
                                    &mut error_count,
                                    session_ending || session_pausing,
                                ) else {
                                    break 'nonaborted_main_loop;
                                };
                                state_machine.deschedule_task(&executed_task.task);
                                if should_pause && !session_ending {
                                    /*
                                    state_machine.reset_task(&executed_task.task);
                                    let ExecutedTask {
                                        task,
                                        result_with_timings,
                                    } = *executed_task;
                                    state_machine.do_schedule_task(task, true);
                                    std::mem::forget(result_with_timings);
                                    */
                                    std::mem::forget(executed_task);
                                } else {
                                    std::mem::forget(executed_task);
                                }
                                if should_pause && !session_pausing && slot != 282254387 {
                                    session_pausing = true;
                                    "pausing"
                                } else {
                                    "desc_b_task"
                                }
                            },
                            recv(dummy_buffered_task_receiver) -> dummy => {
                                assert_matches!(dummy, Err(RecvError));

                                let task = state_machine
                                    .schedule_next_buffered_task()
                                    .expect("unblocked task");
                                runnable_task_sender.send_payload(task).unwrap();
                                "sc_b_task"
                            },
                            recv(new_task_receiver) -> message => {
                                assert!(state_machine.mode() == SchedulingMode::BlockProduction || !session_ending);

                                match message.map(|a| a.into()) {
                                    Ok(NewTaskPayload::Payload(task)) => {
                                        if session_ending {
                                            continue;
                                        }
                                        sleepless_testing::at(CheckPoint::NewTask(task.task_index()));
                                        if let Some(task) = state_machine.do_schedule_task(task, session_pausing) {
                                            //runnable_task_sender.send_aux_payload(task).unwrap();
                                            runnable_task_sender.send_payload(task).unwrap();
                                            "sc_i_task"
                                        } else {
                                            "new_b_task"
                                        }
                                    }
                                    Ok(NewTaskPayload::CloseSubchannel(_)) => {
                                        match state_machine.mode() {
                                            SchedulingMode::BlockVerification => {
                                                session_ending = true;
                                                "ending"
                                            },
                                            SchedulingMode::BlockProduction => {
                                                if slot == 282254387 {
                                                    // can't assert pause signal may have been emitted..
                                                    session_ending = true;
                                                    "ending"
                                                } else if !session_pausing {
                                                    session_pausing = true;
                                                    "pausing"
                                                } else {
                                                    info!("ignoring duplicate close subch");
                                                    continue;
                                                }
                                            },
                                        }
                                    }
                                    Ok(NewTaskPayload::Reset(_)) => {
                                        session_pausing = true;
                                        session_resetting = true;
                                        "draining"
                                    }
                                    Ok(NewTaskPayload::OpenSubchannel(_context_and_result_with_timings)) =>
                                        unreachable!(),
                                    Ok(NewTaskPayload::Disconnect(_)) | Err(RecvError) => {
                                        // Mostly likely is that this scheduler is dropped for pruned blocks of
                                        // abandoned forks...
                                        // This short-circuiting is tested with test_scheduler_drop_short_circuiting.
                                        break 'nonaborted_main_loop;
                                    }
                                }
                            },
                            /*
                            recv(finished_idle_task_receiver) -> executed_task => {
                                let Some((executed_task, should_pause)) = Self::accumulate_result_with_timings(
                                    &context,
                                    &mut result_with_timings,
                                    executed_task.expect("alive handler"),
                                    &mut error_count,
                                    session_ending || session_pausing,
                                ) else {
                                    break 'nonaborted_main_loop;
                                };
                                state_machine.deschedule_task(&executed_task.task);
                                std::mem::forget(executed_task);
                                if should_pause && !session_pausing {
                                    session_pausing = true;
                                    "pausing"
                                } else {
                                    "desc_i_task"
                                }
                            },
                            */
                            default => {
                                if let Some(task) = (!session_pausing).then(|| state_machine.scan_and_schedule_next_task()).flatten() {
                                    runnable_task_sender.send_payload(task).unwrap();
                                    "scan"
                                } else {
                                    continue;
                                }
                            }
                        };
                        let force_log = if step_type == "ending" || step_type == "pausing" || step_type == "draining" {
                            true
                        } else {
                            false
                        };
                        if log_interval.increment() || force_log {
                            log_scheduler!(info, step_type);
                        } else {
                            log_scheduler!(trace, step_type);
                        }

                        is_finished = session_ending && state_machine.has_no_alive_task() || session_pausing && state_machine.has_no_executing_task();
                    }
                    assert!(mem::replace(&mut is_finished, false));

                    // Finalize the current session after asserting it's explicitly requested so.
                    assert!(session_ending || session_pausing);
                    // Send result first because this is blocking the replay code-path.
                    session_result_sender
                        .send(result_with_timings)
                        .expect("always outlived receiver");
                    if session_ending {
                        log_scheduler!(info, "ended");
                    } else {
                        log_scheduler!(info, "paused");
                    }
                    match state_machine.mode() {
                        SchedulingMode::BlockVerification => {
                            reported_task_total = 0;
                            reported_executed_task_total = 0;
                            assert_eq!(error_count, 0);
                        },
                        SchedulingMode::BlockProduction => {
                            session_started_at = Instant::now();
                            cpu_session_started_at = cpu_time::ThreadTime::now();
                            state_machine.reset_task_total();
                            state_machine.reset_executed_task_total();
                            reported_task_total = 0;
                            reported_executed_task_total = 0;
                            error_count = 0;
                        },
                    }

                    // Prepare for the new session.
                    loop {
                        if session_resetting {
                            while let Some(task) = state_machine.schedule_next_buffered_task() {
                                state_machine.deschedule_task(&task);
                                if log_interval.increment() {
                                    log_scheduler!(info, "drained_desc");
                                } else {
                                    log_scheduler!(trace, "drained_desc");
                                }
                                std::mem::forget(task);
                            }
                            log_scheduler!(info, "drained");
                            session_started_at = Instant::now();
                            cpu_session_started_at = cpu_time::ThreadTime::now();
                            reported_task_total = 0;
                            reported_executed_task_total = 0;
                            error_count = 0;
                            session_resetting = false;
                        }
                        match new_task_receiver.recv().map(|a| a.into()) {
                            Ok(NewTaskPayload::OpenSubchannel(context_and_result_with_timings)) => {
                                let (new_context, new_result_with_timings) =
                                    *context_and_result_with_timings;
                                // We just received subsequent (= not initial) session and about to
                                // enter into the preceding `while(!is_finished) {...}` loop again.
                                // Before that, propagate new SchedulingContext to handler threads
                                assert_eq!(state_machine.mode(), new_context.mode());
                                slot = new_context.bank().slot();
                                session_started_at = Instant::now();
                                cpu_session_started_at = cpu_time::ThreadTime::now();

                                if session_ending {
                                    log_interval = LogInterval::default();
                                    state_machine.reinitialize(new_context.mode());
                                    session_ending = false;
                                    log_scheduler!(info, "started");
                                } else {
                                    state_machine.reset_task_total();
                                    state_machine.reset_executed_task_total();
                                    reported_task_total = 0;
                                    reported_executed_task_total = 0;
                                    error_count = 0;
                                    session_pausing = false;
                                    log_scheduler!(info, "unpaused");
                                }

                                runnable_task_sender
                                    .send_chained_channel(new_context.clone(), handler_count)
                                    .unwrap();
                                context = new_context;
                                result_with_timings = new_result_with_timings;
                                break;
                            }
                            Ok(NewTaskPayload::CloseSubchannel(_)) if matches!(state_machine.mode(), SchedulingMode::BlockProduction) => {
                                info!("ignoring duplicate CloseSubchannel...");
                            }
                            Ok(NewTaskPayload::Reset(_)) if matches!(state_machine.mode(), SchedulingMode::BlockProduction) => {
                                session_resetting = true;
                                log_scheduler!(info, "draining");
                            }
                            Ok(NewTaskPayload::Payload(task)) if matches!(state_machine.mode(), SchedulingMode::BlockProduction) => {
                                assert!(state_machine.do_schedule_task(task, true).is_none());
                                if log_interval.increment() {
                                    log_scheduler!(info, "rebuffer");
                                } else {
                                    log_scheduler!(trace, "rebuffer");
                                }
                            }
                            Ok(NewTaskPayload::Disconnect(_)) | Err(_) => {
                                // This unusual condition must be triggered by ThreadManager::drop().
                                // Initialize result_with_timings with a harmless value...
                                result_with_timings = initialized_result_with_timings();
                                session_ending = false;
                                session_pausing = false;
                                break 'nonaborted_main_loop;
                            }
                            Ok(_) => unreachable!(),
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
                log_scheduler!(info, "aborted");
                let _ = cpu_log_reported_at;

                // Next, drop `new_task_receiver`. After that, the paired singleton
                // `new_task_sender` will start to error when called by external threads, resulting
                // in propagation of thread abortion to the external threads.
                drop(new_task_receiver);

                // We will now exit this thread finally... Good bye.
                sleepless_testing::at(CheckPoint::SchedulerThreadAborted);
            }
        };

        let handler_main_loop = || {
            let (banking_packet_receiver, mut on_recv) = banking_stage_context.clone().unzip();
            let banking_packet_receiver = banking_packet_receiver.unwrap_or_else(|| never());
            let new_task_sender = Arc::downgrade(&self.new_task_sender);

            let pool = self.pool.clone();
            let mut runnable_task_receiver = runnable_task_receiver.clone();
            let finished_blocked_task_sender = finished_blocked_task_sender.clone();
            //let finished_idle_task_sender = finished_idle_task_sender.clone();

            // The following loop maintains and updates SchedulingContext as its
            // externally-provided state for each session in this way:
            //
            // 1. Initial context is propagated implicitly by the moved runnable_task_receiver,
            //    which is clone()-d just above for this particular thread.
            // 2. Subsequent contexts are propagated explicitly inside `.after_select()` as part of
            //    `select_biased!`, which are sent from `.send_chained_channel()` in the scheduler
            //    thread for all-but-initial sessions.
            move || loop {
                let (task, sender) = select_biased! {
                    recv(runnable_task_receiver.for_select()) -> message => {
                        let Ok(message) = message else {
                            break;
                        };
                        if let Some(task) = runnable_task_receiver.after_select(message.into()) {
                            (task, &finished_blocked_task_sender)
                        } else {
                            continue;
                        }
                    },
                    recv(banking_packet_receiver) -> banking_packet => {
                        let Some(new_task_sender) = new_task_sender.upgrade() else {
                            info!("dead new_task_sender");
                            break;
                        };

                        let Ok(banking_packet) = banking_packet else {
                            info!("disconnected banking_packet_receiver");
                            let current_thread = thread::current();
                            if new_task_sender.send(NewTaskPayload::Disconnect(Unit::new()).into()).is_ok() {
                                info!("notified a disconnect from {:?}", current_thread);
                            } else {
                                // It seems that the scheduler thread has been aborted already...
                                warn!("failed to notify a disconnect from {:?}", current_thread);
                            }
                            break;
                        };
                        let tasks = on_recv.as_mut().unwrap()(banking_packet);
                        for task in tasks {
                            new_task_sender
                                .send(NewTaskPayload::Payload(task).into())
                                .unwrap();
                        }
                        continue;
                    },
                    /*
                    recv(runnable_task_receiver.aux_for_select()) -> task => {
                        if let Ok(task) = task {
                            (task, &finished_idle_task_sender)
                        } else {
                            runnable_task_receiver.never_receive_from_aux();
                            continue;
                        }
                    },
                    */
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
                    &pool.handler_context,
                );
                if sender.send(Ok(task)).is_err() {
                    warn!("handler_thread: scheduler thread aborted...");
                    break;
                }
            }
        };

        self.scheduler_thread = Some(
            thread::Builder::new()
                .name(format!("solSchedule{postfix}"))
                .spawn_tracked(scheduler_main_loop)
                .unwrap(),
        );

        self.handler_threads = (0..self.pool.handler_count)
            .map({
                |thx| {
                    thread::Builder::new()
                        .name(format!("solScHandle{postfix}{:02}", thx))
                        .spawn_tracked(handler_main_loop())
                        .unwrap()
                }
            })
            .collect();
    }

    fn send_task(&self, task: Task) -> ScheduleResult {
        debug!("send_task()");
        self.new_task_sender
            .send(NewTaskPayload::Payload(task).into())
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
    ) {
        self.ensure_join_threads(should_receive_aborted_session_result);
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

    fn do_end_session(&mut self, nonblocking: bool) {
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
            .send(NewTaskPayload::CloseSubchannel(Unit::new()).into())
            .is_err();

        if abort_detected {
            self.ensure_join_threads_after_abort(true);
            return;
        }

        if nonblocking {
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

    fn end_session(&mut self) {
        self.do_end_session(false)
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
            ))).into())
            .expect("no new session after aborted");
    }
}

pub trait SchedulerInner {
    fn id(&self) -> SchedulerId;
    fn banking_stage_status(&self) -> BankingStageStatus;
    fn is_overgrown(&self, on_hot_path: bool) -> bool;
    fn reset(&self);
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
        banking_stage_context: Option<(BankingPacketReceiver, impl FnMut(BankingPacketBatch) -> Vec<Task> + Clone + Send + 'static)>,
        banking_stage_adapter: Option<Arc<BankingStageAdapter>>,
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
        banking_stage_context: Option<(BankingPacketReceiver, impl FnMut(BankingPacketBatch) -> Vec<Task> + Clone + Send + 'static)>,
        banking_stage_adapter: Option<Arc<BankingStageAdapter>>,
    ) -> Self {
        info!(
            "spawning new scheduler for slot: {}",
            context.bank().slot()
        );
        let task_creator = match context.mode() {
            SchedulingMode::BlockVerification => {
                TaskCreator::BlockVerification { usage_queue_loader: UsageQueueLoader::default() }
            },
            SchedulingMode::BlockProduction => {
                TaskCreator::BlockProduction { banking_stage_adapter: banking_stage_adapter.unwrap() }
            },
        };
        let mut inner = Self::Inner {
            thread_manager: ThreadManager::new(pool),
            task_creator,
        };
        inner
            .thread_manager
            .start_threads(context.clone(), result_with_timings, banking_stage_context);
        Self { inner, context }
    }
}

#[derive(Debug)]
pub enum BankingStageStatus {
    Active,
    Inactive,
    Exited,
}

pub trait BankingStageMonitor: Send + Debug {
    fn banking_stage_status(&self) -> BankingStageStatus;
}

#[derive(Debug)]
pub struct BankingStageAdapter {
    usage_queue_loader: UsageQueueLoader,
    transaction_deduper: DashSet<Hash>,
    pub idling_detector: Mutex<Option<Box<dyn BankingStageMonitor>>>,
}

/*
impl BankingStageAdapter {
    fn clean() {
    }
}
*/

impl BankingStageAdapter {
    pub fn create_task(
        &self,
        &(transaction, index): &(&SanitizedTransaction, TaskKey),
    ) -> Option<Task> {
        if self.transaction_deduper.contains(transaction.message_hash()) {
            return None;
        } else {
            self.transaction_deduper.insert(*transaction.message_hash());
        }

        Some(SchedulingStateMachine::create_task(transaction.clone(), index, &mut |pubkey| {
            self.usage_queue_loader.load(pubkey)
        }))
    }

    fn banking_stage_status(&self) -> BankingStageStatus {
        self.idling_detector.lock().unwrap().as_ref().unwrap().banking_stage_status()
    }

    fn reset(&self)  {
        info!("resetting transaction_deduper... {}", self.transaction_deduper.len());
        self.transaction_deduper.clear();
        info!("resetting transaction_deduper... done: {}", self.transaction_deduper.len());
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
        &(transaction, index): &(&SanitizedTransaction, TaskKey),
    ) -> ScheduleResult {
        assert_matches!(self.context().mode(), SchedulingMode::BlockVerification);
        let task = SchedulingStateMachine::create_task(transaction.clone(), index, &mut |pubkey| {
            self.inner.task_creator.usage_queue_loader().load(pubkey)
        });
        self.inner.thread_manager.send_task(task)
    }

    fn recover_error_after_abort(&mut self) -> TransactionError {
        self.inner
            .thread_manager
            .ensure_join_threads_after_abort(true);
        self.inner
            .thread_manager
            .session_result_with_timings
            .as_mut()
            .unwrap()
            .0
            .clone()
            .unwrap_err()
    }

    fn wait_for_termination(
        self: Box<Self>,
        _is_dropped: bool,
    ) -> (ResultWithTimings, UninstalledSchedulerBox) {
        let (result_with_timings, uninstalled_scheduler) = self.into_inner();
        (result_with_timings, Box::new(uninstalled_scheduler))
    }

    fn pause_for_recent_blockhash(&mut self) {
        // this fn is called from poh thread, while it's being locked. so, we can't wait scheduler
        // termination here to avoid deadlock. just async signaling is enough
        let nonblocking = matches!(self.context().mode(), SchedulingMode::BlockProduction);
        self.inner.thread_manager.do_end_session(nonblocking);
    }
}

impl<S, TH> UninstalledScheduler for PooledSchedulerInner<S, TH>
where
    S: SpawnableScheduler<TH, Inner = Self>,
    TH: TaskHandler,
{
    fn return_to_pool(self: Box<Self>) {
        // Refer to the comment in is_trashed() as to the exact definition of the concept of
        // _trashed_ and the interaction among different parts of unified scheduler.
        let should_trash = self.is_trashed(true);
        if should_trash {
            info!("trashing scheduler (id: {})...", self.id());
        }
        self.thread_manager
            .pool
            .clone()
            .return_scheduler(*self, should_trash);
    }
}

impl<S, TH> SchedulerInner for PooledSchedulerInner<S, TH>
where
    S: SpawnableScheduler<TH, Inner = Self>,
    TH: TaskHandler,
{
    fn id(&self) -> SchedulerId {
        self.thread_manager.scheduler_id
    }

    fn is_overgrown(&self, on_hot_path: bool) -> bool {
        self.task_creator.is_overgrown(self.thread_manager.pool.max_usage_queue_count, on_hot_path)
    }

    fn banking_stage_status(&self) -> BankingStageStatus {
        self.task_creator.banking_stage_status()
    }

    fn reset(&self) {
        if let Err(a) = self.thread_manager.new_task_sender.send(NewTaskPayload::Reset(Unit::new()).into()) {
            warn!("failed to send a reset due to error: {a:?}");
        }
        self.task_creator.reset()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::sleepless_testing,
        assert_matches::assert_matches,
        solana_runtime::{
            bank::Bank,
            bank_forks::BankForks,
            genesis_utils::{create_genesis_config, GenesisConfigInfo},
            installed_scheduler_pool::{BankWithScheduler, SchedulingContext},
            prioritization_fee_cache::PrioritizationFeeCache,
        },
        solana_sdk::{
            clock::{Slot, MAX_PROCESSING_AGE},
            pubkey::Pubkey,
            signer::keypair::Keypair,
            system_transaction,
            transaction::{SanitizedTransaction, TransactionError},
        },
        solana_timings::ExecuteTimingType,
        std::{
            sync::{Arc, RwLock},
            thread::JoinHandle,
        },
    };

    #[derive(Debug)]
    enum TestCheckPoint {
        BeforeNewTask,
        AfterTaskHandled,
        AfterSchedulerThreadAborted,
        BeforeIdleSchedulerCleaned,
        AfterIdleSchedulerCleaned,
        BeforeTrashedSchedulerCleaned,
        AfterTrashedSchedulerCleaned,
        BeforeTimeoutListenerTriggered,
        AfterTimeoutListenerTriggered,
        BeforeThreadManagerDrop,
        BeforeEndSession,
    }

    #[test]
    fn test_scheduler_pool_new() {
        solana_logger::setup();

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new_dyn_for_verification(None, None, None, None, ignored_prioritization_fee_cache);

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
            DefaultSchedulerPool::new_dyn_for_verification(None, None, None, None, ignored_prioritization_fee_cache);
        let bank = Arc::new(Bank::default_for_tests());
        let context = SchedulingContext::for_verification(bank);
        let scheduler = pool.take_scheduler(context).unwrap();

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
        let pool_raw = DefaultSchedulerPool::do_new_for_verification(
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
        let pool_raw = DefaultSchedulerPool::do_new_for_verification(
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
                .task_creator()
                .usage_queue_loader()
                .load(Pubkey::new_unique());
        }
        let big_scheduler = pool.do_take_scheduler(context2);
        for _ in 0..REDUCED_MAX_USAGE_QUEUE_COUNT + 1 {
            big_scheduler
                .inner
                .task_creator()
                .usage_queue_loader()
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
        let pool_raw = DefaultSchedulerPool::do_new_for_verification(
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
        let scheduler = pool.take_scheduler(context).unwrap();
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
        let pool_raw = SchedulerPool::<PooledScheduler<ExecuteTimingCounter>, _>::do_new_for_verification(
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
                _transaction: &SanitizedTransaction,
                _index: TaskKey,
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

        let scheduler = pool.take_scheduler(context).unwrap();
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        pool.register_timeout_listener(bank.create_timeout_listener());

        let tx_before_stale =
            &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_sdk::pubkey::new_rand(),
                2,
                genesis_config.hash(),
            ));
        bank.schedule_transaction_executions([(tx_before_stale, &0)].into_iter())
            .unwrap();
        sleepless_testing::at(TestCheckPoint::BeforeTimeoutListenerTriggered);

        sleepless_testing::at(TestCheckPoint::AfterTimeoutListenerTriggered);
        let tx_after_stale =
            &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_sdk::pubkey::new_rand(),
                2,
                genesis_config.hash(),
            ));
        bank.schedule_transaction_executions([(tx_after_stale, &1)].into_iter())
            .unwrap();

        // Observe second occurrence of TimeoutListenerTriggered(1), which indicates a new timeout
        // lister is registered correctly again for reactivated scheduler.
        sleepless_testing::at(TestCheckPoint::BeforeTimeoutListenerTriggered);
        sleepless_testing::at(TestCheckPoint::AfterTimeoutListenerTriggered);

        let (result, timings) = bank.wait_for_completed_scheduler().unwrap();
        assert_matches!(result, Ok(()));
        // ResultWithTimings should be carried over across active=>stale=>active transitions.
        assert_eq!(timings.metrics[ExecuteTimingType::CheckUs], 246);
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
        let pool_raw = DefaultSchedulerPool::do_new_for_verification(
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

        let scheduler = pool.take_scheduler(context).unwrap();
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
        let pool_raw = SchedulerPool::<PooledScheduler<FaultyHandler>, _>::do_new_for_verification(
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

        let scheduler = pool.take_scheduler(context).unwrap();
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        pool.register_timeout_listener(bank.create_timeout_listener());

        let tx_before_stale =
            &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_sdk::pubkey::new_rand(),
                2,
                genesis_config.hash(),
            ));
        bank.schedule_transaction_executions([(tx_before_stale, &0)].into_iter())
            .unwrap();
        sleepless_testing::at(TestCheckPoint::BeforeTimeoutListenerTriggered);
        sleepless_testing::at(TestCheckPoint::AfterSchedulerThreadAborted);

        sleepless_testing::at(TestCheckPoint::AfterTimeoutListenerTriggered);
        let tx_after_stale =
            &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_sdk::pubkey::new_rand(),
                2,
                genesis_config.hash(),
            ));
        let result = bank.schedule_transaction_executions([(tx_after_stale, &1)].into_iter());
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
            _transaction: &SanitizedTransaction,
            _index: TaskKey,
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

        let tx = &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &solana_sdk::pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));

        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool = SchedulerPool::<PooledScheduler<FaultyHandler>, _>::new_for_verification(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
        );
        let context = SchedulingContext::for_verification(bank.clone());
        let scheduler = pool.do_take_scheduler(context);
        scheduler.schedule_execution(&(tx, 0)).unwrap();

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

        static TASK_COUNT: Mutex<TaskKey> = Mutex::new(0);

        #[derive(Debug)]
        struct CountingHandler;
        impl TaskHandler for CountingHandler {
            fn handle(
                _result: &mut Result<()>,
                _timings: &mut ExecuteTimings,
                _bank: &SchedulingContext,
                _transaction: &SanitizedTransaction,
                _index: TaskKey,
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
        let pool = SchedulerPool::<PooledScheduler<CountingHandler>, _>::new_for_verification(
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
        const MAX_TASK_COUNT: TaskKey = 100;

        for i in 0..MAX_TASK_COUNT {
            let tx =
                &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
                    &mint_keypair,
                    &solana_sdk::pubkey::new_rand(),
                    2,
                    genesis_config.hash(),
                ));
            scheduler.schedule_execution(&(tx, i)).unwrap();
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
            DefaultSchedulerPool::new_for_verification(None, None, None, None, ignored_prioritization_fee_cache);
        let bank = Arc::new(Bank::default_for_tests());
        let context = &SchedulingContext::for_verification(bank);

        let scheduler1 = pool.do_take_scheduler(context.clone());
        let scheduler_id1 = scheduler1.id();
        let scheduler2 = pool.do_take_scheduler(context.clone());
        let scheduler_id2 = scheduler2.id();
        assert_ne!(scheduler_id1, scheduler_id2);

        let (result_with_timings, scheduler1) = scheduler1.into_inner();
        assert_matches!(result_with_timings, (Ok(()), _));
        pool.return_scheduler(scheduler1, false);
        let (result_with_timings, scheduler2) = scheduler2.into_inner();
        assert_matches!(result_with_timings, (Ok(()), _));
        pool.return_scheduler(scheduler2, false);

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
            DefaultSchedulerPool::new_for_verification(None, None, None, None, ignored_prioritization_fee_cache);
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
            DefaultSchedulerPool::new_for_verification(None, None, None, None, ignored_prioritization_fee_cache);
        let old_bank = &Arc::new(Bank::default_for_tests());
        let new_bank = &Arc::new(Bank::default_for_tests());
        assert!(!Arc::ptr_eq(old_bank, new_bank));

        let old_context = &SchedulingContext::for_verification(old_bank.clone());
        let new_context = &SchedulingContext::for_verification(new_bank.clone());

        let scheduler = pool.do_take_scheduler(old_context.clone());
        let scheduler_id = scheduler.id();
        pool.return_scheduler(scheduler.into_inner().1, false);

        let scheduler = pool.take_scheduler(new_context.clone()).unwrap();
        assert_eq!(scheduler_id, scheduler.id());
        assert!(Arc::ptr_eq(scheduler.context().bank(), new_bank));
    }

    #[test]
    fn test_scheduler_pool_install_into_bank_forks() {
        solana_logger::setup();

        let bank = Bank::default_for_tests();
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut bank_forks = bank_forks.write().unwrap();
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new_dyn_for_verification(None, None, None, None, ignored_prioritization_fee_cache);
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
            DefaultSchedulerPool::new_dyn_for_verification(None, None, None, None, ignored_prioritization_fee_cache);

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
        let tx0 = &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &solana_sdk::pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));
        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new_dyn_for_verification(None, None, None, None, ignored_prioritization_fee_cache);
        let context = SchedulingContext::for_verification(bank.clone());

        assert_eq!(bank.transaction_count(), 0);
        let scheduler = pool.take_scheduler(context).unwrap();
        scheduler.schedule_execution(&(tx0, 0)).unwrap();
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
        let pool_raw = DefaultSchedulerPool::do_new_for_verification(
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
        let scheduler = pool.take_scheduler(context).unwrap();

        let unfunded_keypair = Keypair::new();
        let bad_tx =
            &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
                &unfunded_keypair,
                &solana_sdk::pubkey::new_rand(),
                2,
                genesis_config.hash(),
            ));
        assert_eq!(bank.transaction_count(), 0);
        scheduler.schedule_execution(&(bad_tx, 0)).unwrap();
        sleepless_testing::at(TestCheckPoint::AfterTaskHandled);
        assert_eq!(bank.transaction_count(), 0);

        let good_tx_after_bad_tx =
            &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_sdk::pubkey::new_rand(),
                3,
                genesis_config.hash(),
            ));
        // make sure this tx is really a good one to execute.
        assert_matches!(
            bank.simulate_transaction_unchecked(good_tx_after_bad_tx, false)
                .result,
            Ok(_)
        );
        sleepless_testing::at(TestCheckPoint::AfterSchedulerThreadAborted);
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        if extra_tx_after_failure {
            assert_matches!(
                bank.schedule_transaction_executions([(good_tx_after_bad_tx, &1)].into_iter()),
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
                _transaction: &SanitizedTransaction,
                index: TaskKey,
                _handler_context: &HandlerContext,
            ) {
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
        const TX_COUNT: TaskKey = 2;

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool = SchedulerPool::<PooledScheduler<PanickingHandler>, _>::new_dyn_for_verification(
            Some(TX_COUNT as usize), // fix to use exactly 2 handlers
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
        );
        let context = SchedulingContext::for_verification(bank.clone());

        let scheduler = pool.take_scheduler(context).unwrap();

        for index in 0..TX_COUNT {
            // Use 2 non-conflicting txes to exercise the channel disconnected case as well.
            let tx =
                &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
                    &Keypair::new(),
                    &solana_sdk::pubkey::new_rand(),
                    1,
                    genesis_config.hash(),
                ));
            scheduler.schedule_execution(&(tx, index)).unwrap();
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
                _transaction: &SanitizedTransaction,
                index: TaskKey,
                _handler_context: &HandlerContext,
            ) {
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
        let pool = SchedulerPool::<PooledScheduler<CountingFaultyHandler>, _>::new_for_verification(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
        );
        let context = SchedulingContext::for_verification(bank.clone());
        let scheduler = pool.do_take_scheduler(context);

        for i in 0..10 {
            let tx =
                &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
                    &mint_keypair,
                    &solana_sdk::pubkey::new_rand(),
                    2,
                    genesis_config.hash(),
                ));
            scheduler.schedule_execution(&(tx, i)).unwrap();
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

    #[test]
    fn test_scheduler_schedule_execution_blocked() {
        solana_logger::setup();

        const STALLED_TRANSACTION_INDEX: TaskKey = 0;
        const BLOCKED_TRANSACTION_INDEX: TaskKey = 1;
        static LOCK_TO_STALL: Mutex<()> = Mutex::new(());

        #[derive(Debug)]
        struct StallingHandler;
        impl TaskHandler for StallingHandler {
            fn handle(
                result: &mut Result<()>,
                timings: &mut ExecuteTimings,
                bank: &SchedulingContext,
                transaction: &SanitizedTransaction,
                index: TaskKey,
                handler_context: &HandlerContext,
            ) {
                match index {
                    STALLED_TRANSACTION_INDEX => *LOCK_TO_STALL.lock().unwrap(),
                    BLOCKED_TRANSACTION_INDEX => {}
                    _ => unreachable!(),
                };
                DefaultTaskHandler::handle(
                    result,
                    timings,
                    bank,
                    transaction,
                    index,
                    handler_context,
                );
            }
        }

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);

        // tx0 and tx1 is definitely conflicting to write-lock the mint address
        let tx0 = &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &solana_sdk::pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));
        let tx1 = &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
            &mint_keypair,
            &solana_sdk::pubkey::new_rand(),
            2,
            genesis_config.hash(),
        ));

        let bank = Bank::new_for_tests(&genesis_config);
        let (bank, _bank_forks) = setup_dummy_fork_graph(bank);
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool = SchedulerPool::<PooledScheduler<StallingHandler>, _>::new_dyn_for_verification(
            None,
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
        );
        let context = SchedulingContext::for_verification(bank.clone());

        assert_eq!(bank.transaction_count(), 0);
        let scheduler = pool.take_scheduler(context).unwrap();

        // Stall handling tx0 and tx1
        let lock_to_stall = LOCK_TO_STALL.lock().unwrap();
        scheduler
            .schedule_execution(&(tx0, STALLED_TRANSACTION_INDEX))
            .unwrap();
        scheduler
            .schedule_execution(&(tx1, BLOCKED_TRANSACTION_INDEX))
            .unwrap();

        // Wait a bit for the scheduler thread to decide to block tx1
        std::thread::sleep(std::time::Duration::from_secs(1));

        // Resume handling by unlocking LOCK_TO_STALL
        drop(lock_to_stall);
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        assert_matches!(bank.wait_for_completed_scheduler(), Some((Ok(()), _)));
        assert_eq!(bank.transaction_count(), 2);
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
                bank: &SchedulingContext,
                _transaction: &SanitizedTransaction,
                index: TaskKey,
                _handler_context: &HandlerContext,
            ) {
                // The task index must always be matched to the slot.
                assert_eq!(index as Slot, bank.slot());
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
        let pool = SchedulerPool::<PooledScheduler<TaskAndContextChecker>, _>::new_for_verification(
            Some(4), // spawn 4 threads
            None,
            None,
            None,
            ignored_prioritization_fee_cache,
        );

        // Create a dummy tx and two contexts
        let dummy_tx =
            &SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_sdk::pubkey::new_rand(),
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
            let scheduler = pool.take_scheduler(context.clone()).unwrap();
            scheduler.schedule_execution(&(dummy_tx, index)).unwrap();
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
            &(transaction, index): &(&SanitizedTransaction, TaskKey),
        ) -> ScheduleResult {
            let transaction_and_index = (transaction.clone(), index);
            let context = self.context().clone();
            let pool = self.3.clone();

            self.1.lock().unwrap().push(std::thread::spawn(move || {
                // intentionally sleep to simulate race condition where register_recent_blockhash
                // is handle before finishing executing scheduled transactions
                std::thread::sleep(std::time::Duration::from_secs(1));

                let mut result = Ok(());
                let mut timings = ExecuteTimings::default();

                <DefaultTaskHandler as TaskHandler>::handle(
                    &mut result,
                    &mut timings,
                    &context,
                    &transaction_and_index.0,
                    transaction_and_index.1,
                    &pool.handler_context,
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

    impl<const TRIGGER_RACE_CONDITION: bool> UninstalledScheduler
        for AsyncScheduler<TRIGGER_RACE_CONDITION>
    {
        fn return_to_pool(self: Box<Self>) {
            self.3.clone().return_scheduler(*self, false)
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
            SanitizedTransaction::from_transaction_for_tests(system_transaction::transfer(
                &mint_keypair,
                &solana_sdk::pubkey::new_rand(),
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
            SchedulerPool::<AsyncScheduler<TRIGGER_RACE_CONDITION>, DefaultTaskHandler>::new_dyn_for_verification(
                None,
                None,
                None,
                None,
                ignored_prioritization_fee_cache,
            );
        let scheduler = pool.take_scheduler(context).unwrap();

        let bank = BankWithScheduler::new(bank, Some(scheduler));
        assert_eq!(bank.transaction_count(), 0);

        // schedule but not immediately execute transaction
        bank.schedule_transaction_executions([(&very_old_valid_tx, &0)].into_iter())
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
            &solana_sdk::pubkey::new_rand(),
            2,
            genesis_config.hash(),
        );
        // mangle the transfer tx to try to lock fee_payer (= mint_keypair) address twice!
        tx.message.account_keys.push(tx.message.account_keys[0]);
        let tx = &SanitizedTransaction::from_transaction_for_tests(tx);

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
            transaction_recorder: TransactionRecorder::new_dummy(), 
        };

        DefaultTaskHandler::handle(result, timings, scheduling_context, tx, 0, handler_context);
        assert_matches!(result, Err(TransactionError::AccountLoadedTwice));
    }
}
