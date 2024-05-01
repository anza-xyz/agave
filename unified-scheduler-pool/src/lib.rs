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
    cpu_time::ThreadTime,
    crossbeam_channel::{
        self, disconnected, never, select_biased, Receiver, RecvError, RecvTimeoutError, SendError,
        Sender, TryRecvError,
    },
    dashmap::DashMap,
    derivative::Derivative,
    log::*,
    solana_ledger::blockstore_processor::{
        execute_batch, TransactionBatchWithIndexes, TransactionStatusSender,
    },
    solana_measure::measure::Measure,
    solana_metrics::datapoint_info_at,
    solana_program_runtime::timings::ExecuteTimings,
    solana_runtime::{
        bank::Bank,
        compute_budget_details::GetComputeBudgetDetails,
        installed_scheduler_pool::{
            DefaultScheduleExecutionArg, InstalledScheduler, InstalledSchedulerPool,
            InstalledSchedulerPoolArc, ResultWithTimings, ScheduleExecutionArg, SchedulerId,
            SchedulingContext, UninstalledScheduler, UninstalledSchedulerBox,
            WithTransactionAndIndex,
        },
        prioritization_fee_cache::PrioritizationFeeCache,
    },
    solana_sdk::{
        clock::Slot,
        pubkey::Pubkey,
        transaction::{Result, SanitizedTransaction, TransactionError},
    },
    solana_unified_scheduler_logic::{SchedulingStateMachine, Task, UsageQueue},
    solana_vote::vote_sender_types::ReplayVoteSender,
    std::{
        env,
        fmt::Debug,
        sync::{
            atomic::{AtomicU64, Ordering::Relaxed},
            Arc, Mutex, OnceLock, RwLock, RwLockReadGuard, Weak,
        },
        thread::{self, JoinHandle},
        time::{Duration, Instant, SystemTime},
    },
};

type AtomicSchedulerId = AtomicU64;

// SchedulerPool must be accessed as a dyn trait from solana-runtime, because SchedulerPool
// contains some internal fields, whose types aren't available in solana-runtime (currently
// TransactionStatusSender; also, PohRecorder in the future)...
#[derive(Debug)]
pub struct SchedulerPool<S, TH, SEA>
where
    S: SpawnableScheduler<TH, SEA>,
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    scheduler_inners: Mutex<Vec<S::Inner>>,
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
    // prune schedulers, stop idling scheduler's threads, sanity check on the
    // usage queue loader after scheduler is returned.
    cleaner_sender: Sender<Weak<RwLock<ThreadManager<S, TH, SEA>>>>,
    cleaner_exit_signal_sender: Sender<()>,
    cleaner_thread: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug)]
pub struct HandlerContext {
    log_messages_bytes_limit: Option<usize>,
    transaction_status_sender: Option<TransactionStatusSender>,
    replay_vote_sender: Option<ReplayVoteSender>,
    prioritization_fee_cache: Arc<PrioritizationFeeCache>,
}

pub type DefaultSchedulerPool = SchedulerPool<
    PooledScheduler<DefaultTaskHandler, DefaultScheduleExecutionArg>,
    DefaultTaskHandler,
    DefaultScheduleExecutionArg,
>;

struct WatchedThreadManager<S, TH, SEA>
where
    S: SpawnableScheduler<TH, SEA>,
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    thread_manager: Weak<RwLock<ThreadManager<S, TH, SEA>>>,
    #[cfg(target_os = "linux")]
    tick: u64,
    #[cfg(target_os = "linux")]
    updated_at: Instant,
}

impl<S, TH, SEA> WatchedThreadManager<S, TH, SEA>
where
    S: SpawnableScheduler<TH, SEA>,
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    fn new(thread_manager: Weak<RwLock<ThreadManager<S, TH, SEA>>>) -> Self {
        Self {
            thread_manager,
            #[cfg(target_os = "linux")]
            tick: 0,
            #[cfg(target_os = "linux")]
            updated_at: Instant::now(),
        }
    }

    fn retire_if_stale(&mut self) -> bool {
        #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
        let Some(thread_manager) = self.thread_manager.upgrade() else {
            return false;
        };

        // The following linux-only code implements an eager native thread reclaiming, which is
        // only useful if the solana-validator sees many unrooted forks. Such hostile situations
        // should NEVER happen on remotely-uncontrollable ledgers created by solana-test-validator.
        // And it's generally not expected mainnet-beta validators (or any live clusters for that
        // matter) to be run on non-linux OSes at all.
        //
        // Thus, this OS-specific implementation can be justified because this enables the hot-path
        // (the scheduler main thread) to omit VDSO calls and timed-out futex syscalls by relying on
        // this out-of-bound cleaner for a defensive thread reclaiming.
        #[cfg(target_os = "linux")]
        {
            let Some(tid) = thread_manager.read().unwrap().active_tid_if_not_primary() else {
                self.tick = 0;
                self.updated_at = Instant::now();
                return true;
            };

            let pid = std::process::id();
            let task = procfs::process::Process::new(pid.try_into().unwrap())
                .unwrap()
                .task_from_tid(tid)
                .unwrap();
            let stat = task.stat().unwrap();
            let current_tick = stat.utime.checked_add(stat.stime).unwrap();
            if current_tick > self.tick {
                self.tick = current_tick;
                self.updated_at = Instant::now();
            } else {
                // 5x of 400ms block time
                const IDLE_DURATION_FOR_EAGER_THREAD_RECLAIM: Duration = Duration::from_secs(2);

                let elapsed = self.updated_at.elapsed();
                if elapsed > IDLE_DURATION_FOR_EAGER_THREAD_RECLAIM {
                    const BITS_PER_HEX_DIGIT: usize = 4;
                    let thread_manager = &mut thread_manager.write().unwrap();
                    info!(
                        "[sch_{:0width$x}]: cleaner: retire_if_stale(): stopping thread manager ({tid}/{} <= {}/{:?})...",
                        thread_manager.scheduler_id,
                        current_tick,
                        self.tick,
                        elapsed,
                        width = SchedulerId::BITS as usize / BITS_PER_HEX_DIGIT,
                    );
                    thread_manager.suspend();
                    self.tick = 0;
                    self.updated_at = Instant::now();
                }
            }
        }

        true
    }
}

impl<S, TH, SEA> Drop for SchedulerPool<S, TH, SEA>
where
    S: SpawnableScheduler<TH, SEA>,
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    fn drop(&mut self) {
        info!("SchedulerPool::drop() is successfully called");
    }
}

impl<S, TH, SEA> SchedulerPool<S, TH, SEA>
where
    S: SpawnableScheduler<TH, SEA>,
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
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
        let handler_count = handler_count.unwrap_or(Self::default_handler_count());
        assert!(handler_count >= 1);

        let (scheduler_pool_sender, scheduler_pool_receiver) = crossbeam_channel::bounded(1);
        let (cleaner_sender, cleaner_receiver) = crossbeam_channel::unbounded();
        let (cleaner_exit_signal_sender, cleaner_exit_signal_receiver) =
            crossbeam_channel::unbounded();

        let cleaner_main_loop = || {
            move || {
                let scheduler_pool: Arc<Self> = scheduler_pool_receiver.recv().unwrap();
                drop(scheduler_pool_receiver);

                let mut thread_managers: Vec<WatchedThreadManager<S, TH, SEA>> = vec![];

                'outer: loop {
                    let mut schedulers = scheduler_pool.scheduler_inners.lock().unwrap();
                    let schedulers_len_pre_retain = schedulers.len();
                    schedulers.retain_mut(|scheduler| scheduler.retire_if_stale());
                    let schedulers_len_post_retain = schedulers.len();
                    drop(schedulers);

                    let thread_manager_len_pre_retain = thread_managers.len();
                    thread_managers.retain_mut(|thread_manager| thread_manager.retire_if_stale());

                    let thread_manager_len_pre_push = thread_managers.len();
                    'inner: loop {
                        match cleaner_receiver.try_recv() {
                            Ok(thread_manager) => {
                                thread_managers.push(WatchedThreadManager::new(thread_manager))
                            }
                            Err(TryRecvError::Disconnected) => break 'outer,
                            Err(TryRecvError::Empty) => break 'inner,
                        }
                    }

                    info!(
                        "cleaner: unused schedulers in the pool: {} => {}, all thread managers: {} => {} => {}",
                        schedulers_len_pre_retain,
                        schedulers_len_post_retain,
                        thread_manager_len_pre_retain,
                        thread_manager_len_pre_push,
                        thread_managers.len(),
                    );
                    // wait for signal with timeout here instead of recv_timeout() to write all the
                    // preceeding logs at once.
                    match cleaner_exit_signal_receiver.recv_timeout(Duration::from_secs(1)) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break 'outer,
                        Err(RecvTimeoutError::Timeout) => continue,
                    }
                }
                info!("cleaner thread terminating!");
            }
        };

        let cleaner_thread = thread::Builder::new()
            .name("solScCleaner".to_owned())
            .spawn(cleaner_main_loop())
            .unwrap();

        let scheduler_pool = Arc::new_cyclic(|weak_self| Self {
            scheduler_inners: Mutex::default(),
            handler_count,
            handler_context: HandlerContext {
                log_messages_bytes_limit,
                transaction_status_sender,
                replay_vote_sender,
                prioritization_fee_cache,
            },
            weak_self: weak_self.clone(),
            next_scheduler_id: AtomicSchedulerId::new(PRIMARY_SCHEDULER_ID),
            cleaner_thread: Mutex::new(Some(cleaner_thread)),
            cleaner_sender,
            cleaner_exit_signal_sender,
        });
        scheduler_pool_sender.send(scheduler_pool.clone()).unwrap();
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
    ) -> InstalledSchedulerPoolArc<SEA> {
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

    fn return_scheduler(&self, scheduler: S::Inner) {
        self.scheduler_inners
            .lock()
            .expect("not poisoned")
            .push(scheduler);
    }

    #[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
    fn do_take_scheduler(&self, context: SchedulingContext) -> S {
        // pop is intentional for filo, expecting relatively warmed-up scheduler due to having been
        // returned recently
        if let Some(pooled_inner) = self.scheduler_inners.lock().expect("not poisoned").pop() {
            S::from_inner(pooled_inner, context)
        } else {
            S::spawn(self.self_arc(), context, TH::create(self))
        }
    }

    fn register_to_cleaner(&self, thread_manager: Weak<RwLock<ThreadManager<S, TH, SEA>>>) {
        self.cleaner_sender.send(thread_manager).unwrap();
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

impl<S, TH, SEA> InstalledSchedulerPool<SEA> for SchedulerPool<S, TH, SEA>
where
    S: SpawnableScheduler<TH, SEA>,
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    fn take_scheduler(&self, context: SchedulingContext) -> Box<dyn InstalledScheduler<SEA>> {
        Box::new(self.do_take_scheduler(context))
    }

    fn uninstalled_from_bank_forks(self: Arc<Self>) {
        self.scheduler_inners.lock().unwrap().clear();
        self.cleaner_exit_signal_sender.send(()).unwrap();
        let () = self
            .cleaner_thread
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .join()
            .unwrap();
        info!(
            "SchedulerPool::uninstalled_from_bank_forks(): joined cleaner thread at {:?}...",
            thread::current()
        );
    }
}

pub trait TaskHandler<SEA: ScheduleExecutionArg>:
    Send + Sync + Debug + Sized + Clone + 'static
{
    fn create<T: SpawnableScheduler<Self, SEA>>(pool: &SchedulerPool<T, Self, SEA>) -> Self;

    fn handle(
        &self,
        result: &mut Result<()>,
        timings: &mut ExecuteTimings,
        bank: &Arc<Bank>,
        transaction: &SanitizedTransaction,
        index: usize,
        handler_context: &HandlerContext,
    );
}

#[derive(Clone, Debug)]
pub struct DefaultTaskHandler;

impl<SEA: ScheduleExecutionArg> TaskHandler<SEA> for DefaultTaskHandler {
    fn create<T: SpawnableScheduler<Self, SEA>>(_pool: &SchedulerPool<T, Self, SEA>) -> Self {
        Self
    }

    fn handle(
        &self,
        result: &mut Result<()>,
        timings: &mut ExecuteTimings,
        bank: &Arc<Bank>,
        transaction: &SanitizedTransaction,
        index: usize,
        handler_context: &HandlerContext,
    ) {
        // scheduler must properly prevent conflicting tx executions. thus, task handler isn't
        // responsible for locking.
        let batch = bank.prepare_unlocked_batch_from_single_tx(transaction);
        let batch_with_indexes = TransactionBatchWithIndexes {
            batch,
            transaction_indexes: vec![index],
        };

        *result = execute_batch(
            &batch_with_indexes,
            bank,
            handler_context.transaction_status_sender.as_ref(),
            handler_context.replay_vote_sender.as_ref(),
            timings,
            handler_context.log_messages_bytes_limit,
            &handler_context.prioritization_fee_cache,
        );
    }
}

struct ExecutedTask {
    task: Task,
    result_with_timings: ResultWithTimings,
    slot: Slot,
    thx: usize,
    handler_timings: Option<HandlerTimings>,
}

pub struct HandlerTimings {
    finish_time: SystemTime,
    execution_us: u64,
    execution_cpu_us: u128,
}

impl ExecutedTask {
    fn new_boxed(task: Task, thx: usize, slot: Slot) -> Box<Self> {
        Box::new(Self {
            task,
            result_with_timings: initialized_result_with_timings(),
            slot,
            thx,
            handler_timings: None,
        })
    }

    fn is_err(&self) -> bool {
        self.result_with_timings.0.is_err()
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

type NewTaskPayload = SubchanneledPayload<Task, SchedulingContext>;
type RetiredTaskPayload = SubchanneledPayload<Box<ExecutedTask>, ()>;

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
            context: C,
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
/// Currently, the simplest implementation. This grows memory usage in unbounded way. Cleaning will
/// be added later. This struct is here to be put outside `solana-unified-scheduler-logic` for the
/// crate's original intent (separation of logics from this crate). Some practical and mundane
/// pruning will be implemented in this type.
#[derive(Default, Debug)]
pub struct UsageQueueLoader {
    usage_queues: DashMap<Pubkey, UsageQueue>,
}

impl UsageQueueLoader {
    pub fn load(&self, address: Pubkey) -> UsageQueue {
        self.usage_queues.entry(address).or_default().clone()
    }

    pub fn usage_queue_count(&self) -> usize {
        self.usage_queues.len()
    }

    pub fn clear(&self) {
        self.usage_queues.clear();
    }
}

fn initialized_result_with_timings() -> ResultWithTimings {
    (Ok(()), ExecuteTimings::default())
}

#[derive(Debug)]
pub struct PooledScheduler<TH, SEA>
where
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    inner: PooledSchedulerInner<Self, TH, SEA>,
    context: SchedulingContext,
}

#[derive(Debug)]
pub struct PooledSchedulerInner<S, TH, SEA>
where
    S: SpawnableScheduler<TH, SEA>,
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    thread_manager: Arc<RwLock<ThreadManager<S, TH, SEA>>>,
    usage_queue_loader: UsageQueueLoader,
    pooled_at: Instant,
}

impl<S, TH, SEA> PooledSchedulerInner<S, TH, SEA>
where
    S: SpawnableScheduler<TH, SEA>,
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    fn pooled_since(&self) -> Duration {
        self.pooled_at.elapsed()
    }

    fn suspend_thread_manager(&mut self) {
        debug!("suspend_thread_manager()");
        self.thread_manager.write().unwrap().suspend();
    }

    fn id(&self) -> SchedulerId {
        self.thread_manager.read().unwrap().scheduler_id
    }
}

type Tid = i32;
// The linux's tid (essentially is in the pid name space) is guaranteed to be non-zero; so
// using 0 for special purpose at user-land is totally safe.
#[cfg_attr(target_os = "linux", allow(dead_code))]
const DUMMY_TID: Tid = 0;

#[derive(Default)]
struct LogInterval(usize);

impl LogInterval {
    fn increment(&mut self) -> bool {
        let should_log = self.0 % 1000 == 0;
        self.0 = self.0.checked_add(1).unwrap();
        should_log
    }
}

const PRIMARY_SCHEDULER_ID: SchedulerId = 0;

// This type manages the OS threads for scheduling and executing transactions. The term
// `session` is consistently used to mean a group of Tasks scoped under a single SchedulingContext.
// This is equivalent to a particular bank for block verification. However, new terms is introduced
// here to mean some continuous time over multiple continuous banks/slots for the block production,
// which is planned to be implemented in the future.
#[derive(Debug)]
struct ThreadManager<S, TH, SEA>
where
    S: SpawnableScheduler<TH, SEA>,
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    scheduler_id: SchedulerId,
    pool: Arc<SchedulerPool<S, TH, SEA>>,
    handler: TH,
    new_task_sender: Sender<NewTaskPayload>,
    new_task_receiver: Option<Receiver<NewTaskPayload>>,
    session_result_sender: Sender<Option<ResultWithTimings>>,
    session_result_receiver: Receiver<Option<ResultWithTimings>>,
    session_result_with_timings: Option<ResultWithTimings>,
    scheduler_thread_and_tid: Option<(JoinHandle<Option<ResultWithTimings>>, Tid)>,
    handler_threads: Vec<JoinHandle<()>>,
    accumulator_thread: Option<JoinHandle<()>>,
}

impl<TH, SEA> PooledScheduler<TH, SEA>
where
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    fn do_spawn(
        pool: Arc<SchedulerPool<Self, TH, SEA>>,
        initial_context: SchedulingContext,
        handler: TH,
    ) -> Self {
        Self::from_inner(
            PooledSchedulerInner {
                thread_manager: Arc::new(RwLock::new(ThreadManager::new(pool.clone(), handler))),
                usage_queue_loader: UsageQueueLoader::default(),
                pooled_at: Instant::now(),
            },
            initial_context,
        )
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn clear_session_result_with_timings(&mut self) {
        assert_matches!(
            self.inner
                .thread_manager
                .write()
                .unwrap()
                .take_session_result_with_timings(),
            (Ok(_), _)
        );
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn restart_session(&mut self) {
        self.inner
            .thread_manager
            .write()
            .unwrap()
            .start_session(&self.context);
    }

    #[cfg(feature = "dev-context-only-utils")]
    pub fn schedule_task(&self, task: Task) {
        self.inner.thread_manager.read().unwrap().send_task(task);
    }

    fn ensure_thread_manager_resumed(
        &self,
        context: &SchedulingContext,
    ) -> std::result::Result<RwLockReadGuard<'_, ThreadManager<Self, TH, SEA>>, TransactionError>
    {
        let mut was_already_active = false;
        loop {
            let read = self.inner.thread_manager.read().unwrap();
            if !read.is_suspended() {
                debug!(
                    "{}",
                    if was_already_active {
                        "ensure_thread_manager_resumed(): was already active."
                    } else {
                        "ensure_thread_manager_resumed(): wasn't already active..."
                    }
                );
                return Ok(read);
            } else {
                debug!("ensure_thread_manager_resumed(): will start threads...");
                drop(read);
                let mut write = self.inner.thread_manager.write().unwrap();
                write.start_or_try_resume_threads(context)?;
                drop(write);
                was_already_active = false;
            }
        }
    }
}

impl<S, TH, SEA> ThreadManager<S, TH, SEA>
where
    S: SpawnableScheduler<TH, SEA>,
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    fn new(pool: Arc<SchedulerPool<S, TH, SEA>>, handler: TH) -> Self {
        let (new_task_sender, new_task_receiver) = crossbeam_channel::unbounded();
        let (session_result_sender, session_result_receiver) = crossbeam_channel::unbounded();
        let handler_count = pool.handler_count;

        Self {
            scheduler_id: pool.new_scheduler_id(),
            pool,
            handler,
            new_task_sender,
            new_task_receiver: Some(new_task_receiver),
            session_result_sender,
            session_result_receiver,
            session_result_with_timings: None,
            scheduler_thread_and_tid: None,
            handler_threads: Vec::with_capacity(handler_count),
            accumulator_thread: None,
        }
    }

    fn is_suspended(&self) -> bool {
        self.scheduler_thread_and_tid.is_none()
    }

    pub fn take_scheduler_thread(&mut self) -> Option<JoinHandle<Option<ResultWithTimings>>> {
        self.scheduler_thread_and_tid
            .take()
            .map(|(thread, _tid)| thread)
    }

    fn execute_task_with_handler(
        handler: &TH,
        bank: &Arc<Bank>,
        executed_task: &mut Box<ExecutedTask>,
        handler_context: &HandlerContext,
        send_metrics: bool,
    ) {
        let handler_timings =
            send_metrics.then_some((Measure::start("process_message_time"), ThreadTime::now()));
        debug!("handling task at {:?}", thread::current());
        TH::handle(
            handler,
            &mut executed_task.result_with_timings.0,
            &mut executed_task.result_with_timings.1,
            bank,
            executed_task.task.transaction(),
            executed_task.task.task_index(),
            handler_context,
        );
        if let Some((mut wall_time, cpu_time)) = handler_timings {
            executed_task.handler_timings = Some(HandlerTimings {
                finish_time: SystemTime::now(),
                execution_cpu_us: cpu_time.elapsed().as_micros(),
                execution_us: {
                    // make wall time is longer than cpu time, always
                    wall_time.stop();
                    wall_time.as_us()
                },
            });
        }
    }

    fn accumulate_result_with_timings(
        (_result, timings): &mut ResultWithTimings,
        executed_task: Box<ExecutedTask>,
    ) {
        assert_matches!(executed_task.result_with_timings.0, Ok(()));

        if let Some(handler_timings) = &executed_task.handler_timings {
            let thread = format!("solScExLane{:02}", executed_task.thx);
            let signature = executed_task.task.transaction().signature().to_string();
            let account_locks_in_json = serde_json::to_string(
                &executed_task
                    .task
                    .transaction()
                    .get_account_locks_unchecked(),
            )
            .unwrap();
            let status = format!("{:?}", executed_task.result_with_timings.0);
            let compute_unit_price = executed_task
                .task
                .transaction()
                .get_compute_budget_details(false)
                .map(|d| d.compute_unit_price)
                .unwrap_or_default();

            datapoint_info_at!(
                handler_timings.finish_time,
                "transaction_timings",
                ("slot", executed_task.slot, i64),
                ("index", executed_task.task.task_index(), i64),
                ("thread", thread, String),
                ("signature", signature, String),
                ("account_locks_in_json", account_locks_in_json, String),
                ("status", status, String),
                ("duration", handler_timings.execution_us, i64),
                ("cpu_duration", handler_timings.execution_cpu_us, i64),
                ("compute_units", 0 /*task.cu*/, i64),
                ("priority", compute_unit_price, i64), // old name is kept for compat...
            );
        }
        timings.accumulate(&executed_task.result_with_timings.1);
        drop(executed_task);
    }

    fn take_session_result_with_timings(&mut self) -> ResultWithTimings {
        self.session_result_with_timings.take().unwrap()
    }

    fn reset_session_on_error(&mut self) -> Result<()> {
        let err = self
            .session_result_with_timings
            .replace(initialized_result_with_timings())
            .unwrap()
            .0;
        assert_matches!(err, Err(_));
        err
    }

    fn put_session_result_with_timings(&mut self, result_with_timings: ResultWithTimings) {
        assert_matches!(
            self.session_result_with_timings
                .replace(result_with_timings),
            None
        );
    }

    fn start_or_try_resume_threads(&mut self, context: &SchedulingContext) -> Result<()> {
        if !self.is_suspended() {
            // this can't be promoted to panic! as read => write upgrade isn't completely
            // race-free in ensure_thread_manager_resumed()...
            warn!("try_resume(): already resumed");
            return Ok(());
        } else if self
            .session_result_with_timings
            .as_ref()
            .map(|(result, _)| result.is_err())
            .unwrap_or(false)
        {
            warn!("try_resume(): skipping resuming due to err, while resetting session result");
            return self.reset_session_on_error();
        }
        debug!("try_resume(): doing now");

        let send_metrics = env::var("SOLANA_TRANSACTION_TIMINGS").is_ok();
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
        // Finally, note that this optimization should be combined with biased select (i.e.
        // `select_biased!`), which isn't for now... However, consistent performance improvement is
        // observed just with this priority queuing alone.
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
            chained_channel::unbounded::<Task, SchedulingContext>(context.clone());
        // Create two handler-to-scheduler channels to prioritize the finishing of blocked tasks,
        // because it is more likely that a blocked task will have more blocked tasks behind it,
        // which should be scheduled while minimizing the delay to clear buffered linearized runs
        // as fast as possible.
        let (finished_blocked_task_sender, finished_blocked_task_receiver) =
            crossbeam_channel::unbounded::<Box<ExecutedTask>>();
        let (finished_idle_task_sender, finished_idle_task_receiver) =
            crossbeam_channel::unbounded::<Box<ExecutedTask>>();

        let (retired_task_sender, retired_task_receiver) =
            crossbeam_channel::unbounded::<RetiredTaskPayload>();
        let (accumulated_result_sender, accumulated_result_receiver) =
            crossbeam_channel::unbounded::<Option<ResultWithTimings>>();

        let scheduler_id = self.scheduler_id;
        let mut slot = context.bank().slot();
        let (tid_sender, tid_receiver) = crossbeam_channel::bounded(1);

        // High-level flow of new tasks:
        // 1. the replay stage thread send a new task.
        // 2. the scheduler thread accepts the task.
        // 3. the scheduler thread dispatches the task after proper locking.
        // 4. the handler thread processes the dispatched task.
        // 5. the handler thread reply back to the scheduler thread as an executed task.
        // 6. the scheduler thread post-processes the executed task.
        // 7. the scheduler thread send the executed task to the accumulator thread.
        // 8. the accumulator thread examines the executed task's result and accumulate its timing,
        //    finally dropping the transaction inside the executed task.
        let scheduler_main_loop = || {
            let handler_count = self.pool.handler_count;
            let session_result_sender = self.session_result_sender.clone();
            let mut new_task_receiver = self.new_task_receiver.take().unwrap();

            let mut session_ending = false;
            let mut thread_suspending = false;

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

                const BITS_PER_HEX_DIGIT: usize = 4;
                let mut state_machine = unsafe {
                    SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling()
                };
                let mut log_interval = LogInterval::default();
                // hint compiler about inline[never] and unlikely?
                macro_rules! log_scheduler {
                    ($prefix:tt) => {
                        info!(
                            "[sch_{:0width$x}]: slot: {}[{:12}]({}{}): state_machine(({}(+{})=>{})/{}|{}) channels(<{} >{}+{} <{}+{})",
                            scheduler_id, slot,
                            (if ($prefix) == "step" { "interval" } else { $prefix }),
                            (if session_ending {"S"} else {"-"}), (if thread_suspending {"T"} else {"-"}),
                            state_machine.active_task_count(), state_machine.unblocked_task_queue_count(), state_machine.handled_task_count(),
                            state_machine.total_task_count(),
                            state_machine.unblocked_task_count(),
                            new_task_receiver.len(),
                            runnable_task_sender.len(), runnable_task_sender.aux_len(),
                            finished_blocked_task_receiver.len(), finished_idle_task_receiver.len(),
                            width = SchedulerId::BITS as usize / BITS_PER_HEX_DIGIT,
                        );
                    };
                }

                trace!("solScheduler thread is running at: {:?}", thread::current());
                tid_sender
                    .send({
                        #[cfg(not(target_os = "linux"))]
                        let tid = DUMMY_TID;
                        #[cfg(target_os = "linux")]
                        let tid = rustix::thread::gettid().as_raw_nonzero().get();
                        tid
                    })
                    .unwrap();
                log_scheduler!("T:started");

                while !thread_suspending {
                    let mut is_finished = false;
                    while !is_finished {
                        // ALL recv selectors are eager-evaluated ALWAYS by current crossbeam impl,
                        // which isn't great and is inconsistent with `if`s in the Rust's match
                        // arm. So, eagerly binding the result to a variable unconditionally here
                        // makes no perf. difference...
                        let dummy_unblocked_task_receiver =
                            dummy_receiver(state_machine.has_unblocked_task());

                        // There's something special called dummy_unblocked_task_receiver here.
                        // This odd pattern was needed to react to newly unblocked tasks from
                        // _not-crossbeam-channel_ event sources, precisely at the specified
                        // precedence among other selectors, while delegating the conrol flow to
                        // select_biased!.
                        //
                        // In this way, hot looping is avoided and overall control flow is much
                        // consistent. Note that unified scheduler will go
                        // into busy looping to seek lowest latency eventually. However, not now,
                        // to measure _actual_ cpu usage easily with the select approach.
                        let state_change = select_biased! {
                            recv(finished_blocked_task_receiver) -> executed_task => {
                                let executed_task = executed_task.unwrap();

                                if executed_task.is_err() {
                                    log_scheduler!("S+T:aborted");
                                    // MUST: clear the usage queue loader before reusing this scheduler
                                    // ...
                                    session_result_sender.send(None).unwrap();
                                    // be explicit about specifically dropping this receiver
                                    drop(new_task_receiver);
                                    // this timings aren't for the accumulated one. but
                                    // caller doesn't care.
                                    return Some(executed_task.result_with_timings);
                                } else {
                                    state_machine.deschedule_task(&executed_task.task);
                                    retired_task_sender.send_buffered(RetiredTaskPayload::Payload(executed_task)).unwrap();
                                }
                                "step"
                            },
                            recv(dummy_unblocked_task_receiver) -> dummy => {
                                assert_matches!(dummy, Err(RecvError));

                                let task = state_machine
                                    .schedule_next_unblocked_task()
                                    .expect("unblocked task");
                                runnable_task_sender.send_payload(task).unwrap();
                                "step"
                            },
                            recv(new_task_receiver) -> message => {
                                assert!(message.is_err() || (!session_ending && !thread_suspending));

                                match message {
                                    Ok(NewTaskPayload::Payload(task)) => {
                                        if let Some(task) = state_machine.schedule_task(task) {
                                            runnable_task_sender.send_aux_payload(task).unwrap();
                                        }
                                        "step"
                                    }
                                    Ok(NewTaskPayload::CloseSubchannel) => {
                                        session_ending = true;
                                        "S:ending"
                                    }
                                    Ok(NewTaskPayload::OpenSubchannel(_context)) => {
                                        unreachable!();
                                    }
                                    Err(_) => {
                                        assert!(!thread_suspending);
                                        thread_suspending = true;

                                        // Err(_) on new_task_receiver guarantees
                                        // that there's no live sender and no messages to be
                                        // received anymore; so dropping by overriding it with
                                        // never() should pose no possibility of missed messages.
                                        new_task_receiver = never();

                                        "T:suspending"
                                    }
                                }
                            },
                            recv(finished_idle_task_receiver) -> executed_task => {
                                let executed_task = executed_task.unwrap();

                                if executed_task.is_err() {
                                    log_scheduler!("S+T:aborted");
                                    session_result_sender.send(None).unwrap();
                                    // be explicit about specifically dropping this receiver
                                    drop(new_task_receiver);
                                    // this timings aren't for the accumulated one. but
                                    // caller doesn't care.
                                    return Some(executed_task.result_with_timings);
                                } else {
                                    state_machine.deschedule_task(&executed_task.task);
                                    retired_task_sender.send_buffered(RetiredTaskPayload::Payload(executed_task)).unwrap();
                                }
                                "step"
                            },
                        };
                        if state_change != "step" || log_interval.increment() {
                            log_scheduler!(state_change);
                        }

                        is_finished = (session_ending || thread_suspending)
                            && state_machine.has_no_active_task();
                    }

                    if session_ending {
                        log_scheduler!("S:ended");
                        state_machine.reinitialize();
                        log_interval = LogInterval::default();
                        retired_task_sender
                            .send(RetiredTaskPayload::CloseSubchannel)
                            .unwrap();
                        session_result_sender
                            .send(Some(
                                accumulated_result_receiver
                                    .recv()
                                    .unwrap()
                                    .unwrap_or_else(initialized_result_with_timings),
                            ))
                            .unwrap();
                        if !thread_suspending {
                            session_ending = false;
                        }
                    }

                    if !thread_suspending {
                        match new_task_receiver.recv() {
                            Ok(NewTaskPayload::OpenSubchannel(context)) => {
                                slot = context.bank().slot();
                                // signal about new SchedulingContext to handler threads
                                runnable_task_sender
                                    .send_chained_channel(context, handler_count)
                                    .unwrap();
                                retired_task_sender
                                    .send(RetiredTaskPayload::OpenSubchannel(()))
                                    .unwrap();
                                log_scheduler!("S:started");
                            }
                            Err(_) => {
                                assert!(!thread_suspending);
                                thread_suspending = true;
                                log_scheduler!("T:suspending");
                                continue;
                            }
                            Ok(_) => {
                                unreachable!();
                            }
                        }
                    }
                }

                log_scheduler!("T:suspended");
                let scheduler_result_with_timings = if session_ending {
                    None
                } else {
                    retired_task_sender
                        .send(RetiredTaskPayload::CloseSubchannel)
                        .unwrap();
                    accumulated_result_receiver.recv().unwrap()
                };
                trace!(
                    "solScheduler thread is terminating at: {:?}",
                    thread::current()
                );
                scheduler_result_with_timings
            }
        };

        let handler_main_loop = |thx| {
            let pool = self.pool.clone();
            let handler = self.handler.clone();
            let mut runnable_task_receiver = runnable_task_receiver.clone();
            let finished_blocked_task_sender = finished_blocked_task_sender.clone();
            let finished_idle_task_sender = finished_idle_task_sender.clone();

            move || {
                trace!(
                    "solScHandler{:02} thread is running at: {:?}",
                    thx,
                    thread::current()
                );
                loop {
                    let (task, sender) = select_biased! {
                        recv(runnable_task_receiver.for_select()) -> message => {
                            match message {
                                Ok(message) => {
                                    if let Some(task) = runnable_task_receiver.after_select(message) {
                                        (task, &finished_blocked_task_sender)
                                    } else {
                                        continue;
                                    }
                                },
                                Err(_) => break,
                            }
                        },
                        recv(runnable_task_receiver.aux_for_select()) -> task => {
                            if let Ok(task) = task {
                                (task, &finished_idle_task_sender)
                            } else {
                                continue;
                            }
                        },
                    };
                    let bank = runnable_task_receiver.context().bank();
                    let mut task = ExecutedTask::new_boxed(task, thx, bank.slot());
                    Self::execute_task_with_handler(
                        &handler,
                        bank,
                        &mut task,
                        &pool.handler_context,
                        send_metrics,
                    );
                    if sender.send(task).is_err() {
                        break;
                    }
                }
                trace!(
                    "solScHandler{:02} thread is terminating at: {:?}",
                    thx,
                    thread::current()
                );
            }
        };

        let mut accumulator_result_with_timings = self.session_result_with_timings.take();

        let accumulator_main_loop = || {
            move || 'outer: loop {
                match retired_task_receiver.recv_timeout(Duration::from_millis(40)) {
                    Ok(RetiredTaskPayload::Payload(executed_task)) => {
                        Self::accumulate_result_with_timings(
                            accumulator_result_with_timings.as_mut().unwrap(),
                            executed_task,
                        );
                    }
                    Ok(RetiredTaskPayload::OpenSubchannel(())) => {
                        assert_matches!(
                            accumulator_result_with_timings
                                .replace(initialized_result_with_timings()),
                            None
                        );
                    }
                    Ok(RetiredTaskPayload::CloseSubchannel) => {
                        if accumulated_result_sender
                            .send(accumulator_result_with_timings.take())
                            .is_err()
                        {
                            break 'outer;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => break 'outer,
                    Err(RecvTimeoutError::Timeout) => continue,
                }
            }
        };

        self.scheduler_thread_and_tid = Some((
            thread::Builder::new()
                .name("solScheduler".to_owned())
                .spawn(scheduler_main_loop())
                .unwrap(),
            tid_receiver.recv().unwrap(),
        ));

        self.accumulator_thread = Some(
            thread::Builder::new()
                .name("solScAccmltr".to_owned())
                .spawn(accumulator_main_loop())
                .unwrap(),
        );

        self.handler_threads = (0..self.pool.handler_count)
            .map({
                |thx| {
                    thread::Builder::new()
                        .name(format!("solScHandler{:02}", thx))
                        .spawn(handler_main_loop(thx))
                        .unwrap()
                }
            })
            .collect();
        Ok(())
    }

    fn send_task(&self, task: Task) -> bool {
        debug!("send_task()");
        self.new_task_sender
            .send(NewTaskPayload::Payload(task))
            .is_err()
    }

    fn end_session(&mut self) {
        debug!("end_session(): will end session...");
        if self.is_suspended() {
            debug!("end_session(): no threads..");
            assert_matches!(self.session_result_with_timings, Some(_));
            return;
        } else if self.session_result_with_timings.is_some() {
            debug!("end_session(): already result resides within thread manager..");
            return;
        }

        let mut abort_detected = self
            .new_task_sender
            .send(NewTaskPayload::CloseSubchannel)
            .is_err();

        if let Some(result_with_timings) = self.session_result_receiver.recv().unwrap() {
            assert!(!abort_detected);
            self.put_session_result_with_timings(result_with_timings);
        } else {
            abort_detected = true;
        }

        if abort_detected {
            self.suspend();
        }
    }

    fn start_session(&mut self, context: &SchedulingContext) {
        if !self.is_suspended() {
            assert_matches!(self.session_result_with_timings, None);
            self.new_task_sender
                .send(NewTaskPayload::OpenSubchannel(context.clone()))
                .unwrap();
        } else {
            self.put_session_result_with_timings(initialized_result_with_timings());
            assert_matches!(self.start_or_try_resume_threads(context), Ok(()));
        }
    }

    fn suspend(&mut self) {
        let Some(scheduler_thread) = self.take_scheduler_thread() else {
            warn!("suspend(): already suspended...");
            return;
        };
        debug!("suspend(): terminating threads by {:?}", thread::current());

        let (s, r) = crossbeam_channel::unbounded();
        (self.new_task_sender, self.new_task_receiver) = (s, Some(r));

        let () = self.accumulator_thread.take().unwrap().join().unwrap();
        for thread in self.handler_threads.drain(..) {
            debug!("joining...: {:?}", thread);
            () = thread.join().unwrap();
        }
        if let Some(result_with_timings) = scheduler_thread.join().unwrap() {
            self.put_session_result_with_timings(result_with_timings);
        }

        debug!(
            "suspend(): successfully suspended threads by {:?}",
            thread::current()
        );
    }

    fn is_primary(&self) -> bool {
        self.scheduler_id == PRIMARY_SCHEDULER_ID
    }

    #[cfg(target_os = "linux")]
    fn active_tid_if_not_primary(&self) -> Option<Tid> {
        if self.is_primary() {
            // always exempt from cleaner...
            None
        } else {
            self.scheduler_thread_and_tid.as_ref().map(|&(_, tid)| tid)
        }
    }
}

pub trait SpawnableScheduler<TH, SEA>: InstalledScheduler<SEA>
where
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    type Inner: Debug + Send + Sync + RetirableSchedulerInner;

    fn into_inner(self) -> (ResultWithTimings, Self::Inner);

    fn from_inner(inner: Self::Inner, context: SchedulingContext) -> Self;

    fn spawn(
        pool: Arc<SchedulerPool<Self, TH, SEA>>,
        initial_context: SchedulingContext,
        handler: TH,
    ) -> Self
    where
        Self: Sized;
}

pub trait RetirableSchedulerInner {
    fn retire_if_stale(&mut self) -> bool;
}

impl<TH, SEA> SpawnableScheduler<TH, SEA> for PooledScheduler<TH, SEA>
where
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    type Inner = PooledSchedulerInner<Self, TH, SEA>;

    fn into_inner(self) -> (ResultWithTimings, Self::Inner) {
        let result_with_timings = {
            let manager = &mut self.inner.thread_manager.write().unwrap();
            manager.end_session();
            manager.take_session_result_with_timings()
        };
        (result_with_timings, self.inner)
    }

    fn from_inner(inner: Self::Inner, context: SchedulingContext) -> Self {
        inner
            .thread_manager
            .write()
            .unwrap()
            .start_session(&context);
        Self { inner, context }
    }

    fn spawn(
        pool: Arc<SchedulerPool<Self, TH, SEA>>,
        initial_context: SchedulingContext,
        handler: TH,
    ) -> Self {
        let scheduler = Self::do_spawn(pool.clone(), initial_context, handler);
        pool.register_to_cleaner(Arc::downgrade(&scheduler.inner.thread_manager));
        scheduler
    }
}

impl<TH, SEA> InstalledScheduler<SEA> for PooledScheduler<TH, SEA>
where
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    fn id(&self) -> SchedulerId {
        self.inner.id()
    }

    fn context(&self) -> &SchedulingContext {
        &self.context
    }

    fn schedule_execution(
        &self,
        transaction_with_index: SEA::TransactionWithIndex<'_>,
    ) -> Result<()> {
        transaction_with_index.with_transaction_and_index(|transaction, index| {
            let task =
                SchedulingStateMachine::create_task(transaction.clone(), index, &mut |pubkey| {
                    self.inner.usage_queue_loader.load(pubkey)
                });
            let abort_detected = self
                .ensure_thread_manager_resumed(&self.context)?
                .send_task(task);
            if abort_detected {
                let thread_manager = &mut self.inner.thread_manager.write().unwrap();
                thread_manager.suspend();
                thread_manager.reset_session_on_error()
            } else {
                Ok(())
            }
        })
    }

    fn wait_for_termination(
        self: Box<Self>,
        _is_dropped: bool,
    ) -> (ResultWithTimings, UninstalledSchedulerBox) {
        let (result_with_timings, uninstalled_scheduler) = self.into_inner();
        (result_with_timings, Box::new(uninstalled_scheduler))
    }

    fn pause_for_recent_blockhash(&mut self) {
        self.inner.thread_manager.write().unwrap().end_session();
    }
}

impl<S, TH, SEA> UninstalledScheduler for PooledSchedulerInner<S, TH, SEA>
where
    S: SpawnableScheduler<TH, SEA, Inner = PooledSchedulerInner<S, TH, SEA>>,
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    fn return_to_pool(mut self: Box<Self>) {
        let pool = self.thread_manager.write().unwrap().pool.clone();
        self.pooled_at = Instant::now();
        pool.return_scheduler(*self)
    }
}

impl<S, TH, SEA> RetirableSchedulerInner for PooledSchedulerInner<S, TH, SEA>
where
    S: SpawnableScheduler<TH, SEA, Inner = PooledSchedulerInner<S, TH, SEA>>,
    TH: TaskHandler<SEA>,
    SEA: ScheduleExecutionArg,
{
    fn retire_if_stale(&mut self) -> bool {
        // reap threads after 10mins of inactivity for any pooled (idle) schedulers. The primary
        // scheduler is special-cased to empty its usage queue loader book instead, for easier
        // monitoring to accumulate os-level thread metrics. The duration is chosen based on the
        // rough estimation from the frequency of short-lived forks on the mainnet-beta, with
        // consideration of some increased forking at epoch boundaries.
        const IDLE_DURATION_FOR_LAZY_THREAD_RECLAIM: Duration = Duration::from_secs(600);

        const BITS_PER_HEX_DIGIT: usize = 4;
        let usage_queue_count = self.usage_queue_loader.usage_queue_count();
        if usage_queue_count < 200_000 {
            info!(
                "[sch_{:0width$x}]: cleaner: usage queue loader book size: {usage_queue_count}...",
                self.id(),
                width = SchedulerId::BITS as usize / BITS_PER_HEX_DIGIT,
            );
        } else if self.thread_manager.read().unwrap().is_primary() {
            info!(
                "[sch_{:0width$x}]: cleaner: too big usage queue loader book size: {usage_queue_count}...; emptying the primary scheduler",
                self.id(),
                width = SchedulerId::BITS as usize / BITS_PER_HEX_DIGIT,
            );
            self.usage_queue_loader.clear();
            return true;
        } else {
            info!(
                "[sch_{:0width$x}]: cleaner: too big usage queue loader book size: {usage_queue_count}...; retiring scheduler",
                self.id(),
                width = SchedulerId::BITS as usize / BITS_PER_HEX_DIGIT,
            );
            self.suspend_thread_manager();
            return false;
        }

        let pooled_duration = self.pooled_since();
        if pooled_duration <= IDLE_DURATION_FOR_LAZY_THREAD_RECLAIM {
            true
        } else if !self.thread_manager.read().unwrap().is_primary() {
            info!(
                "[sch_{:0width$x}]: cleaner: retiring unused scheduler after {:?}...",
                self.id(),
                pooled_duration,
                width = SchedulerId::BITS as usize / BITS_PER_HEX_DIGIT,
            );
            self.suspend_thread_manager();
            false
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
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
            scheduling::SchedulingMode,
            signer::keypair::Keypair,
            system_transaction,
            transaction::{SanitizedTransaction, TransactionError},
        },
        std::{
            mem,
            sync::Arc,
            thread::{self, sleep, JoinHandle},
            time::Duration,
        },
    };

    #[test]
    fn test_scheduler_pool_new() {
        solana_logger::setup();

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new_dyn(None, None, None, None, ignored_prioritization_fee_cache);

        // this indirectly proves that there should be circular link because there's only one Arc
        // at this moment now
        assert_eq!(
            (Arc::strong_count(&pool), Arc::weak_count(&pool)),
            (1 + 1 /* todo */, 1)
        );
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
        let context = SchedulingContext::new(SchedulingMode::BlockVerification, bank);
        let scheduler = pool.take_scheduler(context);

        let debug = format!("{scheduler:#?}");
        assert!(!debug.is_empty());
    }

    #[test]
    fn test_scheduler_pool_filo() {
        solana_logger::setup();

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new(None, None, None, None, ignored_prioritization_fee_cache);
        let bank = Arc::new(Bank::default_for_tests());
        let context = &SchedulingContext::new(SchedulingMode::BlockVerification, bank);

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
        let context = &SchedulingContext::new(SchedulingMode::BlockVerification, bank);
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

        let old_context =
            &SchedulingContext::new(SchedulingMode::BlockVerification, old_bank.clone());
        let new_context =
            &SchedulingContext::new(SchedulingMode::BlockVerification, new_bank.clone());

        let scheduler = pool.do_take_scheduler(old_context.clone());
        let scheduler_id = scheduler.id();
        pool.return_scheduler(scheduler.into_inner().1);

        let scheduler = pool.take_scheduler(new_context.clone());
        assert_eq!(scheduler_id, scheduler.id());
        assert!(Arc::ptr_eq(scheduler.context().bank(), new_bank));
    }

    #[test]
    fn test_scheduler_pool_install_into_bank_forks() {
        solana_logger::setup();

        let bank = Bank::default_for_tests();
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut bank_forks_write = bank_forks.write().unwrap();
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new_dyn(None, None, None, None, ignored_prioritization_fee_cache);
        bank_forks_write.install_scheduler_pool(pool);
        bank_forks_write.prepare_to_drop();
        drop(bank_forks_write);
        drop::<BankForks>(Arc::into_inner(bank_forks).unwrap().into_inner().unwrap());
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

    fn setup_dummy_fork_graph(bank: Bank) -> Arc<Bank> {
        let slot = bank.slot();
        let bank_fork = BankForks::new_rw_arc(bank);
        let bank = bank_fork.read().unwrap().get(slot).unwrap();
        bank.set_fork_graph_in_program_cache(bank_fork);
        bank
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
        let bank = setup_dummy_fork_graph(bank);
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new_dyn(None, None, None, None, ignored_prioritization_fee_cache);
        let context = SchedulingContext::new(SchedulingMode::BlockVerification, bank.clone());

        assert_eq!(bank.transaction_count(), 0);
        let scheduler = pool.take_scheduler(context);
        scheduler.schedule_execution(&(tx0, 0)).unwrap();
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        assert_matches!(bank.wait_for_completed_scheduler(), Some((Ok(()), _)));
        assert_eq!(bank.transaction_count(), 1);
    }

    #[test]
    fn test_scheduler_schedule_execution_failure() {
        solana_logger::setup();

        let GenesisConfigInfo {
            genesis_config,
            mint_keypair,
            ..
        } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank = setup_dummy_fork_graph(bank);

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool =
            DefaultSchedulerPool::new_dyn(None, None, None, None, ignored_prioritization_fee_cache);
        let context = SchedulingContext::new(SchedulingMode::BlockVerification, bank.clone());
        let mut scheduler = pool.take_scheduler(context);

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
        // simulate the task-sending thread is stalled for some reason.
        sleep(Duration::from_secs(1));
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
        sleep(Duration::from_secs(3));
        scheduler
            .schedule_execution(&(good_tx_after_bad_tx, 1))
            .unwrap_err();
        error!("last pause!");
        scheduler.pause_for_recent_blockhash();
        // transaction_count should remain same as scheduler should be bailing out.
        // That's because we're testing the serialized failing execution case in this test.
        // Also note that bank.transaction_count() is generally racy by nature, because
        // blockstore_processor and unified_scheduler both tend to process non-conflicting batches
        // in parallel as part of the normal operation.
        assert_eq!(bank.transaction_count(), 0);

        let bank = BankWithScheduler::new(bank, Some(scheduler));
        assert_matches!(
            bank.wait_for_completed_scheduler(),
            Some((Ok(()), _timings))
        );
        pool.uninstalled_from_bank_forks();
    }

    #[test]
    fn test_scheduler_schedule_execution_blocked() {
        solana_logger::setup();

        const STALLED_TRANSACTION_INDEX: usize = 0;
        const BLOCKED_TRANSACTION_INDEX: usize = 1;
        static LOCK_TO_STALL: Mutex<()> = Mutex::new(());

        #[derive(Debug, Clone)]
        struct StallingHandler;
        impl TaskHandler<DefaultScheduleExecutionArg> for StallingHandler {
            fn create<T: SpawnableScheduler<Self, DefaultScheduleExecutionArg>>(
                _pool: &SchedulerPool<T, Self, DefaultScheduleExecutionArg>,
            ) -> Self {
                Self
            }

            fn handle(
                &self,
                result: &mut Result<()>,
                timings: &mut ExecuteTimings,
                bank: &Arc<Bank>,
                transaction: &SanitizedTransaction,
                index: usize,
                handler_context: &HandlerContext,
            ) {
                match index {
                    STALLED_TRANSACTION_INDEX => *LOCK_TO_STALL.lock().unwrap(),
                    BLOCKED_TRANSACTION_INDEX => {}
                    _ => unreachable!(),
                };
                <DefaultTaskHandler as TaskHandler<DefaultScheduleExecutionArg>>::handle(
                    &DefaultTaskHandler,
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
        let bank = setup_dummy_fork_graph(bank);
        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool = SchedulerPool::<
            PooledScheduler<StallingHandler, DefaultScheduleExecutionArg>,
            _,
            _,
        >::new_dyn(None, None, None, None, ignored_prioritization_fee_cache);
        let context = SchedulingContext::new(SchedulingMode::BlockVerification, bank.clone());

        assert_eq!(bank.transaction_count(), 0);
        let scheduler = pool.take_scheduler(context);

        // Stall handling tx0 and tx1
        let lock_to_stall = LOCK_TO_STALL.lock().unwrap();
        scheduler
            .schedule_execution(&(tx0, STALLED_TRANSACTION_INDEX))
            .unwrap();
        scheduler
            .schedule_execution(&(tx1, BLOCKED_TRANSACTION_INDEX))
            .unwrap();

        // Wait a bit for the scheduler thread to decide to block tx1
        sleep(Duration::from_secs(1));

        // Resume handling by unlocking LOCK_TO_STALL
        drop(lock_to_stall);
        let bank = BankWithScheduler::new(bank, Some(scheduler));
        assert_matches!(bank.wait_for_completed_scheduler(), Some((Ok(()), _)));
        assert_eq!(bank.transaction_count(), 2);
    }

    #[test]
    fn test_scheduler_mismatched_scheduling_context_race() {
        solana_logger::setup();

        #[derive(Debug, Clone)]
        struct TaskAndContextChecker;
        impl TaskHandler<DefaultScheduleExecutionArg> for TaskAndContextChecker {
            fn create<T: SpawnableScheduler<Self, DefaultScheduleExecutionArg>>(
                _pool: &SchedulerPool<T, Self, DefaultScheduleExecutionArg>,
            ) -> Self {
                Self
            }

            fn handle(
                &self,
                _result: &mut Result<()>,
                _timings: &mut ExecuteTimings,
                bank: &Arc<Bank>,
                _transaction: &SanitizedTransaction,
                index: usize,
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
        let bank0 = setup_dummy_fork_graph(bank0);
        let bank1 = Arc::new(Bank::new_from_parent(
            bank0.clone(),
            &Pubkey::default(),
            bank0.slot().checked_add(1).unwrap(),
        ));

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool = SchedulerPool::<
            PooledScheduler<TaskAndContextChecker, DefaultScheduleExecutionArg>,
            _,
            _,
        >::new(
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
        let context0 = &SchedulingContext::new(SchedulingMode::BlockVerification, bank0.clone());
        let context1 = &SchedulingContext::new(SchedulingMode::BlockVerification, bank1.clone());

        // Exercise the scheduler by busy-looping to expose the race condition
        for (context, index) in [(context0, 0), (context1, 1)]
            .into_iter()
            .cycle()
            .take(10000)
        {
            let scheduler = pool.take_scheduler(context.clone());
            scheduler.schedule_execution(&(dummy_tx, index)).unwrap();
            scheduler.wait_for_termination(false).1.return_to_pool();
        }
    }

    #[derive(Debug)]
    struct AsyncScheduler<const TRIGGER_RACE_CONDITION: bool>(
        Mutex<ResultWithTimings>,
        Mutex<Vec<JoinHandle<ResultWithTimings>>>,
        SchedulingContext,
        Arc<SchedulerPool<Self, DefaultTaskHandler, DefaultScheduleExecutionArg>>,
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

    impl<const TRIGGER_RACE_CONDITION: bool> InstalledScheduler<DefaultScheduleExecutionArg>
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
            &(transaction, index): &(&SanitizedTransaction, usize),
        ) -> Result<()> {
            let transaction_and_index = (transaction.clone(), index);
            let context = self.context().clone();
            let pool = self.3.clone();

            self.1.lock().unwrap().push(thread::spawn(move || {
                // intentionally sleep to simulate race condition where register_recent_blockhash
                // is handle before finishing executing scheduled transactions
                sleep(Duration::from_secs(1));

                let mut result = Ok(());
                let mut timings = ExecuteTimings::default();

                <DefaultTaskHandler as TaskHandler<DefaultScheduleExecutionArg>>::handle(
                    &DefaultTaskHandler,
                    &mut result,
                    &mut timings,
                    context.bank(),
                    &transaction_and_index.0,
                    transaction_and_index.1,
                    &pool.handler_context,
                );
                (result, timings)
            }));

            Ok(())
        }

        fn wait_for_termination(
            self: Box<Self>,
            _is_dropped: bool,
        ) -> (ResultWithTimings, UninstalledSchedulerBox) {
            self.do_wait();
            let result_with_timings = mem::replace(
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
            self.3.clone().return_scheduler(*self)
        }
    }

    impl<const TRIGGER_RACE_CONDITION: bool>
        SpawnableScheduler<DefaultTaskHandler, DefaultScheduleExecutionArg>
        for AsyncScheduler<TRIGGER_RACE_CONDITION>
    {
        // well, i wish i can use ! (never type).....
        type Inner = Self;

        fn into_inner(self) -> (ResultWithTimings, Self::Inner) {
            unimplemented!();
        }

        fn from_inner(_inner: Self::Inner, _context: SchedulingContext) -> Self {
            unimplemented!();
        }

        fn spawn(
            pool: Arc<SchedulerPool<Self, DefaultTaskHandler, DefaultScheduleExecutionArg>>,
            initial_context: SchedulingContext,
            _handler: DefaultTaskHandler,
        ) -> Self {
            AsyncScheduler::<TRIGGER_RACE_CONDITION>(
                Mutex::new(initialized_result_with_timings()),
                Mutex::new(vec![]),
                initial_context,
                pool,
            )
        }
    }

    impl<const TRIGGER_RACE_CONDITION: bool> RetirableSchedulerInner
        for AsyncScheduler<TRIGGER_RACE_CONDITION>
    {
        fn retire_if_stale(&mut self) -> bool {
            unimplemented!();
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
        let bank = setup_dummy_fork_graph(bank);
        let context = SchedulingContext::new(SchedulingMode::BlockVerification, bank.clone());

        let ignored_prioritization_fee_cache = Arc::new(PrioritizationFeeCache::new(0u64));
        let pool = SchedulerPool::<AsyncScheduler<TRIGGER_RACE_CONDITION>, _, _>::new_dyn(
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
        let bank = &setup_dummy_fork_graph(bank);

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
        let handler_context = &HandlerContext {
            log_messages_bytes_limit: None,
            transaction_status_sender: None,
            replay_vote_sender: None,
            prioritization_fee_cache,
        };

        <DefaultTaskHandler as TaskHandler<DefaultScheduleExecutionArg>>::handle(
            &DefaultTaskHandler,
            result,
            timings,
            bank,
            tx,
            0,
            handler_context,
        );
        assert_matches!(result, Err(TransactionError::AccountLoadedTwice));
    }
}
