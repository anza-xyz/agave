#![cfg_attr(
    not(feature = "agave-unstable-api"),
    deprecated(
        since = "3.1.0",
        note = "This crate has been marked for formal inclusion in the Agave Unstable API. From \
                v4.0.0 onward, the `agave-unstable-api` crate feature must be specified to \
                acknowledge use of an interface that may break without warning."
    )
)]
#![allow(rustdoc::private_intra_doc_links)]
//! The task (transaction) scheduling code for the unified scheduler
//!
//! ### High-level API and design
//!
//! The most important type is [`SchedulingStateMachine`]. It takes new tasks (= transactions) and
//! may return back them if runnable via
//! [`::schedule_task()`](SchedulingStateMachine::schedule_task) while maintaining the account
//! readonly/writable lock rules. Those returned runnable tasks are guaranteed to be safe to
//! execute in parallel. Lastly, `SchedulingStateMachine` should be notified about the completion
//! of the execution via [`::deschedule_task()`](SchedulingStateMachine::deschedule_task), so that
//! conflicting tasks can be returned from
//! [`::schedule_next_unblocked_task()`](SchedulingStateMachine::schedule_next_unblocked_task) as
//! newly-unblocked runnable ones.
//!
//! The design principle of this crate (`solana-unified-scheduler-logic`) is simplicity for the
//! separation of concern. It is interacted only with a few of its public API by
//! `solana-unified-scheduler-pool`. This crate doesn't know about banks, slots, solana-runtime,
//! threads, crossbeam-channel at all. Because of this, it's deterministic, easy-to-unit-test, and
//! its perf footprint is well understood. It really focuses on its single job: sorting
//! transactions in executable order.
//!
//! ### Algorithm
//!
//! The algorithm can be said it's based on per-address FIFO queues, which are updated every time
//! both new task is coming (= called _scheduling_) and runnable (= _post-scheduling_) task is
//! finished (= called _descheduling_).
//!
//! For the _non-conflicting scheduling_ case, the story is very simple; it just remembers that all
//! of accessed addresses are write-locked or read-locked with the number of active (=
//! _currently-scheduled-and-not-descheduled-yet_) tasks. Correspondingly, descheduling does the
//! opposite book-keeping process, regardless whether a finished task has been conflicted or not.
//!
//! For the _conflicting scheduling_ case, it remembers that each of **non-conflicting addresses**
//! like the non-conflicting case above. As for **conflicting addresses**, each task is recorded to
//! respective FIFO queues attached to the (conflicting) addresses. Importantly, the number of
//! conflicting addresses of the conflicting task is also remembered.
//!
//! The last missing piece is that the scheduler actually tries to reschedule previously blocked
//! tasks while deschduling, in addition to the above-mentioned book-keeping processing. Namely,
//! when given address is ready for new fresh locking resulted from descheduling a task (i.e. write
//! lock is released or read lock count has reached zero), it pops out the first element of the
//! FIFO blocked-task queue of the address. Then, it immediately marks the address as relocked. It
//! also decrements the number of conflicting addresses of the popped-out task. As the final step,
//! if the number reaches to the zero, it means the task has fully finished locking all of its
//! addresses and is directly routed to be runnable. Lastly, if the next first element of the
//! blocked-task queue is trying to read-lock the address like the popped-out one, this
//! rescheduling is repeated as an optimization to increase parallelism of task execution.
//!
//! Put differently, this algorithm tries to gradually lock all of addresses of tasks at different
//! timings while not deviating the execution order from the original task ingestion order. This
//! implies there's no locking retries in general, which is the primary source of non-linear perf.
//! degradation.
//!
//! As a ballpark number from a synthesized micro benchmark on usual CPU for `mainnet-beta`
//! validators, it takes roughly 100ns to schedule and deschedule a transaction with 10 accounts.
//! And 1us for a transaction with 100 accounts. Note that this excludes crossbeam communication
//! overhead at all. That's said, it's not unrealistic to say the whole unified scheduler can
//! attain 100k-1m tps overall, assuming those transaction executions aren't bottlenecked.
//!
//! ### Runtime performance characteristics and data structure arrangement
//!
//! Its algorithm is very fast for high throughput, real-time for low latency. The whole
//! unified-scheduler architecture is designed from grounds up to support the fastest execution of
//! this scheduling code. For that end, unified scheduler pre-loads address-specific locking state
//! data structures (called [`UsageQueue`]) for all of transaction's accounts, in order to offload
//! the job to other threads from the scheduler thread. This preloading is done inside
//! [`create_task()`](SchedulingStateMachine::create_task). In this way, task scheduling
//! computational complexity is basically reduced to several word-sized loads and stores in the
//! scheduler thread (i.e.  constant; no allocations nor syscalls), while being proportional to the
//! number of addresses in a given transaction. Note that this statement is held true, regardless
//! of conflicts. This is because the preloading also pre-allocates some scratch-pad area
//! ([`blocked_usages_from_tasks`](UsageQueueInner::blocked_usages_from_tasks)) to stash blocked
//! ones. So, a conflict only incurs some additional fixed number of mem stores, within error
//! margin of the constant complexity. And additional memory allocation for the scratchpad could
//! said to be amortized, if such an unusual event should occur.
//!
//! [`Arc`] is used to implement this preloading mechanism, because `UsageQueue`s are shared across
//! tasks accessing the same account, and among threads due to the preloading. Also, interior
//! mutability is needed. However, `SchedulingStateMachine` doesn't use conventional locks like
//! RwLock.  Leveraging the fact it's the only state-mutating exclusive thread, it instead uses
//! `UnsafeCell`, which is sugar-coated by a tailored wrapper called [`TokenCell`]. `TokenCell`
//! imposes an overly restrictive aliasing rule via rust type system to maintain the memory safety.
//! By localizing any synchronization to the message passing, the scheduling code itself attains
//! maximally possible single-threaed execution without stalling cpu pipelines at all, only
//! constrained to mem access latency, while efficiently utilizing L1-L3 cpu cache with full of
//! `UsageQueue`s.
//!
//! ### Buffer bloat insignificance
//!
//! The scheduler code itself doesn't care about the buffer bloat problem, which can occur in
//! unified scheduler, where a run of heavily linearized and blocked tasks could be severely
//! hampered by very large number of interleaved runnable tasks along side.  The reason is again
//! for separation of concerns. This is acceptable because the scheduling code itself isn't
//! susceptible to the buffer bloat problem by itself as explained by the description and validated
//! by the mentioned benchmark above. Thus, this should be solved elsewhere, specifically at the
//! scheduler pool.
use {
    crate::utils::{ShortCounter, Token, TokenCell},
    assert_matches::assert_matches,
    solana_clock::{Epoch, Slot},
    solana_pubkey::Pubkey,
    solana_runtime_transaction::runtime_transaction::RuntimeTransaction,
    solana_transaction::sanitized::SanitizedTransaction,
    static_assertions::const_assert_eq,
    std::{
        cmp::Ordering,
        collections::{BTreeMap, VecDeque},
        mem,
        sync::Arc,
    },
    unwrap_none::UnwrapNone,
};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SchedulingMode {
    BlockVerification,
    BlockProduction,
}

#[derive(Debug)]
pub enum Capability {
    /// Basic capability of simple fifo queueing. This is intended for block verification.
    FifoQueueing,
    /// Strictly superset capability of priority queueing with reordering of tasks by task_id.
    /// This is intended for block production
    /// In other words, any use of FifoQueueing can safely replaced with PriorityQueueing, just
    /// being slower due to use of more expensive collections.
    PriorityQueueing,
}

/// This type alias is intentionally not exposed to public API with `pub`. The choice of explicit
/// `u32`, rather than more neutral `usize`, is an implementation detail to squeeze out CPU-cache
/// footprint as much as possible.
/// Note that usage of `u32` is safe because it's expected `SchedulingStateMachine` to be
/// `reinitialize()`-d rather quickly after short period: 1 slot for block verification, 4 (or up to
/// 8) consecutive slots for block production.
type CounterInner = u32;

pub type OrderedTaskId = u128;

/// Internal utilities. Namely this contains [`ShortCounter`] and [`TokenCell`].
mod utils {
    use {
        crate::CounterInner,
        std::{
            any::{self, TypeId},
            cell::{RefCell, UnsafeCell},
            collections::BTreeSet,
            marker::PhantomData,
            thread,
        },
    };

    /// A really tiny counter to hide `.checked_{add,sub}` all over the place.
    ///
    /// It's caller's responsibility to ensure this (backed by [`CounterInner`]) never overflow.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct ShortCounter(CounterInner);

    impl ShortCounter {
        pub(super) fn zero() -> Self {
            Self(0)
        }

        pub(super) fn one() -> Self {
            Self(1)
        }

        pub(super) fn is_one(&self) -> bool {
            self.0 == 1
        }

        pub(super) fn is_zero(&self) -> bool {
            self.0 == 0
        }

        pub(super) fn current(&self) -> CounterInner {
            self.0
        }

        #[must_use]
        pub(super) fn increment(self) -> Self {
            Self(self.0.checked_add(1).unwrap())
        }

        #[must_use]
        pub(super) fn decrement(self) -> Self {
            Self(self.0.checked_sub(1).unwrap())
        }

        pub(super) fn increment_self(&mut self) -> &mut Self {
            *self = self.increment();
            self
        }

        pub(super) fn decrement_self(&mut self) -> &mut Self {
            *self = self.decrement();
            self
        }

        pub(super) fn reset_to_zero(&mut self) -> &mut Self {
            self.0 = 0;
            self
        }
    }

    /// A conditionally [`Send`]-able and [`Sync`]-able cell leveraging scheduler's one-by-one data
    /// access pattern with zero runtime synchronization cost.
    ///
    /// To comply with Rust's aliasing rules, these cells require a carefully-created [`Token`] to
    /// be passed around to access the inner values. The token is a special-purpose phantom object
    /// to get rid of its inherent `unsafe`-ness in [`UnsafeCell`], which is internally used for
    /// the interior mutability.
    ///
    /// The final objective of [`Token`] is to ensure there's only one mutable reference to the
    /// [`TokenCell`] at most _at any given moment_. To that end, it's `unsafe` to create it,
    /// shifting the responsibility of binding the only singleton instance to a particular thread
    /// and not creating more than one, onto the API consumers. And its constructor is non-`const`,
    /// and the type is `!Clone` (and `!Copy` as well), `!Default`, `!Send` and `!Sync` to make it
    /// relatively hard to cross thread boundaries accidentally.
    ///
    /// In other words, the token semantically _owns_ all of its associated instances of
    /// [`TokenCell`]s. And `&mut Token` is needed to access one of them as if the one is of
    /// [`Token`]'s `*_mut()` getters. Thus, the Rust aliasing rule for `UnsafeCell` can
    /// transitively be proven to be satisfied simply based on the usual borrow checking of the
    /// `&mut` reference of [`Token`] itself via
    /// [`::with_borrow_mut()`](TokenCell::with_borrow_mut).
    ///
    /// By extension, it's allowed to create _multiple_ tokens in a _single_ process as long as no
    /// instance of [`TokenCell`] is shared by multiple instances of [`Token`].
    ///
    /// Note that this is overly restrictive in that it's forbidden, yet, technically possible
    /// to _have multiple mutable references to the inner values at the same time, if and only
    /// if the respective cells aren't aliased to each other (i.e. different instances)_. This
    /// artificial restriction is acceptable for its intended use by the unified scheduler's code
    /// because its algorithm only needs to access each instance of [`TokenCell`]-ed data once at a
    /// time. Finally, this restriction is traded off for restoration of Rust aliasing rule at zero
    /// runtime cost.  Without this token mechanism, there's no way to realize this.
    #[derive(Debug, Default)]
    pub(super) struct TokenCell<V>(UnsafeCell<V>);

    impl<V> TokenCell<V> {
        /// Creates a new `TokenCell` with the `value` typed as `V`.
        ///
        /// Note that this isn't parametric over the its accompanied `Token`'s lifetime to avoid
        /// complex handling of non-`'static` heaped data in general. Instead, it's manually
        /// required to ensure this instance is accessed only via its associated Token for the
        /// entire lifetime.
        ///
        /// This is intentionally left to be non-`const` to forbid unprotected sharing via static
        /// variables among threads.
        pub(super) fn new(value: V) -> Self {
            Self(UnsafeCell::new(value))
        }

        /// Acquires a mutable reference inside a given closure, while borrowing the mutable
        /// reference of the given token.
        ///
        /// In this way, any additional reborrow can never happen at the same time across all
        /// instances of [`TokenCell<V>`] conceptually owned by the instance of [`Token<V>`] (a
        /// particular thread), unless previous borrow is released. After the release, the used
        /// singleton token should be free to be reused for reborrows.
        ///
        /// Note that lifetime of the acquired reference is still restricted to 'self, not
        /// 'token, in order to avoid use-after-free undefined behaviors.
        pub(super) fn with_borrow_mut<R>(
            &self,
            _token: &mut Token<V>,
            f: impl FnOnce(&mut V) -> R,
        ) -> R {
            f(unsafe { &mut *self.0.get() })
        }
    }

    // Safety: Once after a (`Send`-able) `TokenCell` is transferred to a thread from other
    // threads, access to `TokenCell` is assumed to be only from the single thread by proper use of
    // Token. Thereby, implementing `Sync` can be thought as safe and doing so is needed for the
    // particular implementation pattern in the unified scheduler (multi-threaded off-loading).
    //
    // In other words, TokenCell is technically still `!Sync`. But there should be no
    // legalized usage which depends on real `Sync` to avoid undefined behaviors.
    unsafe impl<V> Sync for TokenCell<V> {}

    /// A auxiliary zero-sized type to enforce aliasing rule to [`TokenCell`] via rust type system
    ///
    /// Token semantically owns a collection of `TokenCell` objects and governs the _unique_
    /// existence of mutable access over them by requiring the token itself to be mutably borrowed
    /// to get a mutable reference to the internal value of `TokenCell`.
    // *mut is used to make this type !Send and !Sync
    pub(super) struct Token<V: 'static>(PhantomData<*mut V>);

    impl<V> Token<V> {
        /// Returns the token to acquire a mutable reference to the inner value of [TokenCell].
        ///
        /// This is intentionally left to be non-`const` to forbid unprotected sharing via static
        /// variables among threads.
        ///
        /// # Panics
        ///
        /// This function will `panic!()` if called multiple times with same type `V` from the same
        /// thread to detect potential misuses.
        ///
        /// # Safety
        ///
        /// This method should be called exactly once for each thread at most to avoid undefined
        /// behavior when used with [`Token`].
        #[must_use]
        pub(super) unsafe fn assume_exclusive_mutating_thread() -> Self {
            thread_local! {
                static TOKENS: RefCell<BTreeSet<TypeId>> = const { RefCell::new(BTreeSet::new()) };
            }
            // TOKEN.with_borrow_mut can't panic because it's the only non-overlapping
            // bound-to-local-variable borrow of the _thread local_ variable.
            assert!(
                TOKENS.with_borrow_mut(|tokens| tokens.insert(TypeId::of::<Self>())),
                "{:?} is wrongly initialized twice on {:?}",
                any::type_name::<Self>(),
                thread::current()
            );

            Self(PhantomData)
        }
    }

    #[cfg(test)]
    mod tests {
        use {
            super::{Token, TokenCell},
            std::{mem, sync::Arc, thread},
        };

        #[test]
        #[should_panic(
            expected = "\"solana_unified_scheduler_logic::utils::Token<usize>\" is wrongly \
                        initialized twice on Thread"
        )]
        fn test_second_creation_of_tokens_in_a_thread() {
            unsafe {
                let _ = Token::<usize>::assume_exclusive_mutating_thread();
                let _ = Token::<usize>::assume_exclusive_mutating_thread();
            }
        }

        #[derive(Debug)]
        struct FakeQueue {
            v: Vec<u8>,
        }

        // As documented above, it's illegal to create multiple tokens inside a single thread to
        // acquire multiple mutable references to the same TokenCell at the same time.
        #[test]
        // Trigger (harmless) UB unless running under miri by conditionally #[ignore]-ing,
        // confirming false-positive result to conversely show the merit of miri!
        #[cfg_attr(miri, ignore)]
        fn test_ub_illegally_created_multiple_tokens() {
            // Unauthorized token minting!
            let mut token1 = unsafe { mem::transmute::<(), Token<FakeQueue>>(()) };
            let mut token2 = unsafe { mem::transmute::<(), Token<FakeQueue>>(()) };

            let queue = TokenCell::new(FakeQueue {
                v: Vec::with_capacity(20),
            });
            queue.with_borrow_mut(&mut token1, |queue_mut1| {
                queue_mut1.v.push(1);
                queue.with_borrow_mut(&mut token2, |queue_mut2| {
                    queue_mut2.v.push(2);
                    queue_mut1.v.push(3);
                });
                queue_mut1.v.push(4);
            });

            // It's in ub already, so we can't assert reliably, so dbg!(...) just for fun
            #[cfg(not(miri))]
            dbg!(queue.0.into_inner());

            // Return successfully to indicate an unexpected outcome, because this test should
            // have aborted by now.
        }

        // As documented above, it's illegal to share (= co-own) the same instance of TokenCell
        // across threads. Unfortunately, we can't prevent this from happening with some
        // type-safety magic to cause compile errors... So sanity-check here test fails due to a
        // runtime error of the known UB, when run under miri.
        #[test]
        // Trigger (harmless) UB unless running under miri by conditionally #[ignore]-ing,
        // confirming false-positive result to conversely show the merit of miri!
        #[cfg_attr(miri, ignore)]
        fn test_ub_illegally_shared_token_cell() {
            let queue1 = Arc::new(TokenCell::new(FakeQueue {
                v: Vec::with_capacity(20),
            }));
            let queue2 = queue1.clone();
            #[cfg(not(miri))]
            let queue3 = queue1.clone();

            // Usually miri immediately detects the data race; but just repeat enough time to avoid
            // being flaky
            for _ in 0..10 {
                let (queue1, queue2) = (queue1.clone(), queue2.clone());
                let thread1 = thread::spawn(move || {
                    let mut token = unsafe { Token::assume_exclusive_mutating_thread() };
                    queue1.with_borrow_mut(&mut token, |queue| {
                        // this is UB
                        queue.v.push(3);
                    });
                });
                // Immediately spawn next thread without joining thread1 to ensure there's a data race
                // definitely. Otherwise, joining here wouldn't cause UB.
                let thread2 = thread::spawn(move || {
                    let mut token = unsafe { Token::assume_exclusive_mutating_thread() };
                    queue2.with_borrow_mut(&mut token, |queue| {
                        // this is UB
                        queue.v.push(4);
                    });
                });

                thread1.join().unwrap();
                thread2.join().unwrap();
            }

            // It's in ub already, so we can't assert reliably, so dbg!(...) just for fun
            #[cfg(not(miri))]
            {
                drop((queue1, queue2));
                dbg!(Arc::into_inner(queue3).unwrap().0.into_inner());
            }

            // Return successfully to indicate an unexpected outcome, because this test should
            // have aborted by now
        }
    }
}

/// [`Result`] for locking a [usage_queue](UsageQueue) with particular
/// [current_usage](RequestedUsage).
type LockResult = Result<(), ()>;
const_assert_eq!(mem::size_of::<LockResult>(), 1);

/// Something to be scheduled; usually a wrapper of [`SanitizedTransaction`].
pub type Task = Arc<TaskInner>;
const_assert_eq!(mem::size_of::<Task>(), 8);

pub type BlockSize = usize;
pub const NO_CONSUMED_BLOCK_SIZE: BlockSize = 0;
pub const MAX_SANITIZED_EPOCH: Epoch = Epoch::MAX;
pub const MAX_ALT_INVALIDATION_SLOT: Slot = Slot::MAX;

/// [`Token`] for [`UsageQueue`].
type UsageQueueToken = Token<UsageQueueInner>;
const_assert_eq!(mem::size_of::<UsageQueueToken>(), 0);

/// [`Token`] for [task](Task)'s [internal mutable data](`TaskInner::blocked_usage_count`).
type BlockedUsageCountToken = Token<ShortCounter>;
const_assert_eq!(mem::size_of::<BlockedUsageCountToken>(), 0);

/// Internal scheduling data about a particular task.
#[derive(Debug)]
pub struct TaskInner {
    transaction: RuntimeTransaction<SanitizedTransaction>,
    /// For block verification, the index of a transaction in ledger entries. Carrying this along
    /// with the transaction is needed to properly record the execution result of it.
    /// For block production, the priority of a transaction for reordering with
    /// Capability::PriorityQueueing. Note that the index of a transaction in ledger entries is
    /// dynamically generated from the poh in the case of block production.
    task_id: OrderedTaskId,
    lock_contexts: Vec<LockContext>,
    /// The number of remaining usages which are currently occupied by other tasks. In other words,
    /// the task is said to be _blocked_ and needs to be _unblocked_ exactly this number of times
    /// before running.
    blocked_usage_count: TokenCell<ShortCounter>,
    consumed_block_size: BlockSize,
    sanitized_epoch: Epoch,
    alt_invalidation_slot: Slot,
}

impl TaskInner {
    pub fn task_id(&self) -> OrderedTaskId {
        self.task_id
    }

    pub fn is_higher_priority(&self, other: &Self) -> bool {
        match self.task_id().cmp(&other.task_id()) {
            Ordering::Less => true,
            Ordering::Greater => false,
            Ordering::Equal => panic!("self-compariton"),
        }
    }

    pub fn consumed_block_size(&self) -> BlockSize {
        self.consumed_block_size
    }

    pub fn sanitized_epoch(&self) -> Epoch {
        self.sanitized_epoch
    }

    pub fn alt_invalidation_slot(&self) -> Slot {
        self.alt_invalidation_slot
    }

    pub fn transaction(&self) -> &RuntimeTransaction<SanitizedTransaction> {
        &self.transaction
    }

    fn lock_contexts(&self) -> &[LockContext] {
        &self.lock_contexts
    }

    fn set_blocked_usage_count(&self, token: &mut BlockedUsageCountToken, count: ShortCounter) {
        self.blocked_usage_count
            .with_borrow_mut(token, |usage_count| {
                *usage_count = count;
            })
    }

    /// Try to change the counter's state of this task towards the runnable state (called
    /// _unblocking_), returning itself if finished completely.
    ///
    /// This should be called exactly once each time one of blocked usages of this task is newly
    /// unlocked for proper book-keeping. Eventually, when this particular unblocking is determined
    /// to be the last (i.e. [`Self::blocked_usage_count`] reaches to 0), this task should be run
    /// by consuming the returned task itself properly.
    #[must_use]
    fn try_unblock(self: Task, token: &mut BlockedUsageCountToken) -> Option<Task> {
        let did_unblock = self
            .blocked_usage_count
            .with_borrow_mut(token, |usage_count| usage_count.decrement_self().is_zero());
        did_unblock.then_some(self)
    }

    /// Try to change the counter's state of this task against the runnable state (called
    /// _reblocking_), returning `true` if succeeded.
    ///
    /// This should be called with care to be consistent with usage queue's
    /// [`blocked_usages_from_tasks`](UsageQueueInner::Priority::blocked_usages_from_tasks).
    /// Blocked usage count of tasks are usually expected only to decrement over time by
    /// [unblocking](Self::try_unblock). However, sometimes it's needed to do the opposite (
    /// [`Capability::PriorityQueueing`]). In other words, previously successfully acquired usage
    /// must be taken from a task to assign the usage to a even more higher-priority task. Note
    /// that this can't be done if usage_count has already reached to 0, meaning it's possible for
    /// it to be running already. In that case, this method returns `false` with no state change.
    /// Otherwise, returns `true` after incrementing [`Self::blocked_usage_count`].
    fn try_reblock(&self, token: &mut BlockedUsageCountToken) -> bool {
        self.blocked_usage_count
            .with_borrow_mut(token, |usage_count| {
                if usage_count.is_zero() {
                    false
                } else {
                    usage_count.increment_self();
                    true
                }
            })
    }

    pub fn into_transaction(self: Task) -> RuntimeTransaction<SanitizedTransaction> {
        Task::into_inner(self).unwrap().transaction
    }
}

/// [`Task`]'s per-address context to lock a [usage_queue](UsageQueue) with [certain kind of
/// request](RequestedUsage).
#[derive(Debug)]
struct LockContext {
    usage_queue: UsageQueue,
    requested_usage: RequestedUsage,
}
const_assert_eq!(mem::size_of::<LockContext>(), 16);

impl LockContext {
    fn new(usage_queue: UsageQueue, requested_usage: RequestedUsage) -> Self {
        Self {
            usage_queue,
            requested_usage,
        }
    }

    fn with_usage_queue_mut<R>(
        &self,
        usage_queue_token: &mut UsageQueueToken,
        f: impl FnOnce(&mut UsageQueueInner) -> R,
    ) -> R {
        self.usage_queue.0.with_borrow_mut(usage_queue_token, f)
    }
}

/// Status about how the [`UsageQueue`] is used currently.
#[derive(Copy, Clone, Debug)]
enum Usage<R, W> {
    Readonly(R),
    Writable(W),
}

impl<R, W> Usage<R, W> {
    fn requested_usage(&self) -> RequestedUsage {
        match self {
            Self::Readonly(_) => RequestedUsage::Readonly,
            Self::Writable(_) => RequestedUsage::Writable,
        }
    }
}

type FifoUsage = Usage<ShortCounter, ()>;
const_assert_eq!(mem::size_of::<FifoUsage>(), 8);

// PriorityUsage will temporarily contain current tasks, unlike its very light-weight cousin (i.e.
// FifoUsage). This arrangement is needed for reblocking (see the prepare_lock() method).
//
// Considering it may reside at usage_queue.current_usage, practically this means each addresses
// (`Pubkey`s) may instantiate its own ones independently, exactly in the same manner as
// usage_queue.blocked_usages_from_tasks.
//
// The reblocking algorithm involves the removal keyed by task_id on arbitrary-ordered unlocking
// and the ranged query to handle reblocking of current readonly usages (if any). Currently,
// BTreeMap is chosen mainly for its implementation simplicity and acceptable efficiency. This
// might be replaced with more efficient implementation in the future.
type PriorityUsage = Usage<BTreeMap<OrderedTaskId, Task>, Task>;
const_assert_eq!(mem::size_of::<PriorityUsage>(), 32);

impl From<RequestedUsage> for FifoUsage {
    fn from(requested_usage: RequestedUsage) -> Self {
        match requested_usage {
            RequestedUsage::Readonly => Self::Readonly(ShortCounter::one()),
            RequestedUsage::Writable => Self::Writable(()),
        }
    }
}

impl PriorityUsage {
    fn from(task: Task, requested_usage: RequestedUsage) -> Self {
        match requested_usage {
            RequestedUsage::Readonly => Self::Readonly(BTreeMap::from([(task.task_id(), task)])),
            RequestedUsage::Writable => Self::Writable(task),
        }
    }

    fn take_readable(maybe_usage: &mut Option<Self>) {
        let Some(Self::Readonly(tasks)) = maybe_usage.take() else {
            panic!();
        };
        assert!(tasks.is_empty());
    }

    fn take_writable(maybe_usage: &mut Option<Self>) -> Task {
        let Some(Self::Writable(task)) = maybe_usage.take() else {
            panic!();
        };
        task
    }
}

/// Status about how a task is requesting to use a particular [`UsageQueue`].
#[derive(Clone, Copy, Debug)]
enum RequestedUsage {
    Readonly,
    Writable,
}

// BTreeMap is needed for now for efficient manipulation...
type PriorityUsageQueue = BTreeMap<OrderedTaskId, UsageFromTask>;

trait PriorityUsageQueueExt: Sized {
    fn insert_usage_from_task(&mut self, usage_from_task: UsageFromTask);
    fn pop_first_usage_from_task(&mut self) -> Option<UsageFromTask>;
    fn first_usage_from_task(&self) -> Option<&UsageFromTask>;
}

impl PriorityUsageQueueExt for PriorityUsageQueue {
    fn insert_usage_from_task(&mut self, usage_from_task: UsageFromTask) {
        self.insert(usage_from_task.1.task_id(), usage_from_task)
            .unwrap_none();
    }

    fn pop_first_usage_from_task(&mut self) -> Option<UsageFromTask> {
        self.pop_first().map(|(_index, usage)| usage)
    }

    fn first_usage_from_task(&self) -> Option<&UsageFromTask> {
        self.first_key_value().map(|(_index, usage)| usage)
    }
}

/// Internal scheduling data about a particular address.
///
/// Specifically, it holds the current [`Usage`] (or no usage with [`Usage::Unused`]) and which
/// [`Task`]s are blocked to be executed after the current task is notified to be finished via
/// [`::deschedule_task`](`SchedulingStateMachine::deschedule_task`)
#[derive(Debug)]
enum UsageQueueInner {
    Fifo {
        current_usage: Option<FifoUsage>,
        blocked_usages_from_tasks: VecDeque<UsageFromTask>,
    },
    Priority {
        current_usage: Option<PriorityUsage>,
        blocked_usages_from_tasks: PriorityUsageQueue,
    },
}

type UsageFromTask = (RequestedUsage, Task);

impl UsageQueueInner {
    fn with_fifo() -> Self {
        Self::Fifo {
            current_usage: None,
            // Capacity should be configurable to create with large capacity like 1024 inside the
            // (multi-threaded) closures passed to create_task(). In this way, reallocs can be
            // avoided happening in the scheduler thread. Also, this configurability is desired for
            // unified-scheduler-logic's motto: separation of concerns (the pure logic should be
            // sufficiently distanced from any some random knob's constants needed for messy
            // reality for author's personal preference...).
            //
            // Note that large cap should be accompanied with proper scheduler cleaning after use,
            // which should be handled by higher layers (i.e. scheduler pool).
            blocked_usages_from_tasks: VecDeque::with_capacity(128),
        }
    }

    fn with_priority() -> Self {
        Self::Priority {
            current_usage: None,
            // PriorityUsageQueue (i.e. BTreeMap) doesn't support capacity provisioning unlike
            // VecDeque above. For efficient key-based lookup, BTreeMap can't usually be backed by
            // some continuous provisioning-friendly collection (i.e. Vec). And, due to the need of
            // those lookups by the current implementation, we can't use BinaryHeap and its family
            // _for now_.
            blocked_usages_from_tasks: PriorityUsageQueue::new(),
        }
    }

    fn new(capability: &Capability) -> Self {
        match capability {
            Capability::FifoQueueing => Self::with_fifo(),
            Capability::PriorityQueueing => Self::with_priority(),
        }
    }
}

impl UsageQueueInner {
    fn try_lock(&mut self, new_task: &Task, requested_usage: RequestedUsage) -> LockResult {
        match self {
            Self::Fifo { current_usage, .. } => match current_usage {
                None => Ok(FifoUsage::from(requested_usage)),
                Some(FifoUsage::Readonly(count)) => match requested_usage {
                    RequestedUsage::Readonly => Ok(FifoUsage::Readonly(count.increment())),
                    RequestedUsage::Writable => Err(()),
                },
                Some(FifoUsage::Writable(())) => Err(()),
            }
            .map(|new_usage| {
                *current_usage = Some(new_usage);
            }),
            Self::Priority { current_usage, .. } => match current_usage {
                Some(PriorityUsage::Readonly(tasks)) => match requested_usage {
                    RequestedUsage::Readonly => {
                        tasks
                            .insert(new_task.task_id(), new_task.clone())
                            .unwrap_none();
                        Ok(())
                    }
                    RequestedUsage::Writable => Err(()),
                },
                Some(PriorityUsage::Writable(_task)) => Err(()),
                None => {
                    *current_usage = Some(PriorityUsage::from(new_task.clone(), requested_usage));

                    Ok(())
                }
            },
        }
    }

    #[must_use]
    fn unlock(&mut self, task: &Task, requested_usage: RequestedUsage) -> Option<UsageFromTask> {
        let mut is_newly_lockable = false;
        match self {
            Self::Fifo { current_usage, .. } => {
                match current_usage {
                    Some(FifoUsage::Readonly(count)) => match requested_usage {
                        RequestedUsage::Readonly => {
                            if count.is_one() {
                                is_newly_lockable = true;
                            } else {
                                count.decrement_self();
                            }
                        }
                        RequestedUsage::Writable => unreachable!(),
                    },
                    Some(FifoUsage::Writable(())) => {
                        assert_matches!(requested_usage, RequestedUsage::Writable);
                        is_newly_lockable = true;
                    }
                    None => unreachable!(),
                }
                if is_newly_lockable {
                    *current_usage = None;
                }
            }
            Self::Priority { current_usage, .. } => {
                match current_usage {
                    Some(PriorityUsage::Readonly(tasks)) => match requested_usage {
                        RequestedUsage::Readonly => {
                            // Don't skip remove()-ing to assert the existence of the last task.
                            tasks.remove(&task.task_id()).unwrap();
                            if tasks.is_empty() {
                                is_newly_lockable = true;
                            }
                        }
                        RequestedUsage::Writable => unreachable!(),
                    },
                    Some(PriorityUsage::Writable(_task)) => {
                        assert_matches!(requested_usage, RequestedUsage::Writable);
                        is_newly_lockable = true;
                    }
                    None => unreachable!(),
                }
                if is_newly_lockable {
                    *current_usage = None;
                }
            }
        }

        if is_newly_lockable {
            self.pop()
        } else {
            None
        }
    }

    fn push_blocked(&mut self, usage_from_task: UsageFromTask) {
        assert_matches!(self.current_usage(), Some(_));
        self.push(usage_from_task);
    }

    #[must_use]
    fn pop_lockable_readonly(&mut self) -> Option<UsageFromTask> {
        if matches!(self.peek_blocked(), Some((RequestedUsage::Readonly, _))) {
            assert_matches!(self.current_usage(), Some(RequestedUsage::Readonly));
            self.pop()
        } else {
            None
        }
    }

    fn current_usage(&self) -> Option<RequestedUsage> {
        match self {
            Self::Fifo { current_usage, .. } => {
                current_usage.as_ref().map(|usage| usage.requested_usage())
            }
            Self::Priority { current_usage, .. } => {
                current_usage.as_ref().map(|usage| usage.requested_usage())
            }
        }
    }

    #[cfg(test)]
    fn update_current_usage(&mut self, requested_usage: RequestedUsage, task: &Task) {
        match self {
            Self::Fifo { current_usage, .. } => {
                *current_usage = Some(FifoUsage::from(requested_usage));
            }
            Self::Priority { current_usage, .. } => {
                *current_usage = Some(PriorityUsage::from(task.clone(), requested_usage));
            }
        }
    }

    fn pop(&mut self) -> Option<UsageFromTask> {
        match self {
            Self::Fifo {
                blocked_usages_from_tasks,
                ..
            } => blocked_usages_from_tasks.pop_front(),
            Self::Priority {
                blocked_usages_from_tasks,
                ..
            } => blocked_usages_from_tasks.pop_first_usage_from_task(),
        }
    }

    fn push(&mut self, usage_from_task: UsageFromTask) {
        match self {
            Self::Fifo {
                blocked_usages_from_tasks,
                ..
            } => blocked_usages_from_tasks.push_back(usage_from_task),
            Self::Priority {
                blocked_usages_from_tasks,
                ..
            } => blocked_usages_from_tasks.insert_usage_from_task(usage_from_task),
        }
    }

    fn peek_blocked(&self) -> Option<&UsageFromTask> {
        match self {
            Self::Fifo {
                blocked_usages_from_tasks,
                ..
            } => blocked_usages_from_tasks.front(),
            Self::Priority {
                blocked_usages_from_tasks,
                ..
            } => blocked_usages_from_tasks.first_usage_from_task(),
        }
    }

    fn prepare_lock(
        &mut self,
        token: &mut BlockedUsageCountToken,
        new_task: &Task,
        requested_usage: RequestedUsage,
    ) -> LockResult {
        match self {
            Self::Fifo {
                blocked_usages_from_tasks,
                ..
            } => {
                if blocked_usages_from_tasks.is_empty() {
                    Ok(())
                } else {
                    Err(())
                }
            }
            // This is the heart of our priority queue mechanism, called reblocking. Understanding
            // it needs a bit of twist of thinking.
            //
            // First, recall that the entire logic of SchedulingStateMachine is about fifo
            // queueing, whose state is ensured for completion with nice property of _bounded_
            // computation of each ticks of state transition.
            //
            // This priority reordering want to exploit it. Namely, following code mangles the
            // queue upon arrival of new higher-prioritized tasks and immediately before the actual
            // locking (thus this fn is called `prepare_lock()`), _as if those tasks should have
            // arrived earlier in the precise order of their task_ids. Then, all other code work
            // nicely with priority ordering enabled.
            //
            // Note that reblocking must be consistently applied across all usage queues.
            // otherwise, deadlock would happen.
            Self::Priority {
                current_usage,
                blocked_usages_from_tasks,
            } => {
                // This artificial var is needed to pacify rust borrow checker...
                let mut current_and_requested_usage = (current_usage, requested_usage);

                match &mut current_and_requested_usage {
                    (None, _) => {
                        assert!(blocked_usages_from_tasks.is_empty());
                        Ok(())
                    }
                    (Some(PriorityUsage::Writable(current_task)), _requested_usage) => {
                        if !new_task.is_higher_priority(current_task)
                            || !current_task.try_reblock(token)
                        {
                            return Err(());
                        }
                        let reblocked_task = Usage::take_writable(current_and_requested_usage.0);
                        blocked_usages_from_tasks
                            .insert_usage_from_task((RequestedUsage::Writable, reblocked_task));
                        Ok(())
                    }
                    (Some(PriorityUsage::Readonly(_current_tasks)), RequestedUsage::Readonly) => {
                        let Some((peeked_usage, peeked_task)) = self.peek_blocked() else {
                            return Ok(());
                        };

                        // Current usage is Readonly. This means that the highest-priority
                        // blocked task must be Writable. So, we assert this here as a
                        // precaution. Note that peeked_usage must be Writable regardless requested
                        // usage is Readonly or Writable.
                        assert_matches!(peeked_usage, RequestedUsage::Writable);
                        if !new_task.is_higher_priority(peeked_task) {
                            return Err(());
                        }
                        Ok(())
                    }
                    (Some(PriorityUsage::Readonly(current_tasks)), RequestedUsage::Writable) => {
                        // First, we need to determine whether the write-requesting new_task could
                        // reblock current read-only tasks _very efficiently while bounded under
                        // the worst case_, to prevent large number of low priority tasks from
                        // consuming undue amount of cpu cycles for nothing.

                        // Use extract_if once stablized to remove Vec creation and the repeating
                        // remove()s...
                        let task_indexes = current_tasks
                            .range(new_task.task_id()..)
                            .filter_map(|(&task_id, task)| {
                                task.try_reblock(token).then_some(task_id)
                            })
                            .collect::<Vec<OrderedTaskId>>();
                        for task_id in task_indexes.into_iter() {
                            let reblocked_task = current_tasks.remove(&task_id).unwrap();
                            blocked_usages_from_tasks
                                .insert_usage_from_task((RequestedUsage::Readonly, reblocked_task));
                        }

                        if current_tasks.is_empty() {
                            Usage::take_readable(current_and_requested_usage.0);
                            Ok(())
                        } else {
                            // In this case, new_task will still be inserted as the
                            // highest-priority blocked writable task, nevertheless any of readonly
                            // tasks are reblocked above. That's because all of such tasks should
                            // be of lower-priority than new_task by the very `range()` lookup
                            // above. So, the write-always-follows-read critical invariant is still
                            // intact. So is the assertion in current-and-requested-readonly
                            // match arm.
                            Err(())
                        }
                    }
                }
            }
        }
    }
}

const_assert_eq!(mem::size_of::<TokenCell<UsageQueueInner>>(), 56);

/// Scheduler's internal data for each address ([`Pubkey`](`solana_pubkey::Pubkey`)). Very
/// opaque wrapper type; no methods just with [`::clone()`](Clone::clone) and
/// [`::default()`](Default::default).
///
/// It's the higher layer's responsibility to ensure to associate the same instance of UsageQueue
/// for given Pubkey at the time of [task](Task) creation.
#[derive(Debug, Clone)]
pub struct UsageQueue(Arc<TokenCell<UsageQueueInner>>);
const_assert_eq!(mem::size_of::<UsageQueue>(), 8);

impl UsageQueue {
    pub fn new(capability: &Capability) -> Self {
        Self(Arc::new(TokenCell::new(UsageQueueInner::new(capability))))
    }
}

/// A high-level `struct`, managing the overall scheduling of [tasks](Task), to be used by
/// `solana-unified-scheduler-pool`.
pub struct SchedulingStateMachine {
    unblocked_task_queue: VecDeque<Task>,
    /// The number of all tasks which aren't `deschedule_task()`-ed yet while their ownership has
    /// already been transferred to SchedulingStateMachine by `schedule_or_buffer_task()`. In other
    /// words, they are running right now or buffered by explicit request or by implicit blocking
    /// due to one of other _active_ (= running or buffered) conflicting tasks.
    active_task_count: ShortCounter,
    /// The number of tasks which are running right now.
    running_task_count: ShortCounter,
    /// The maximum number of running tasks at any given moment. While this could be tightly
    /// related to the number of threads, terminology here is intentionally abstracted away to make
    /// this struct purely logic only. As a hypothetical counter-example, tasks could be IO-bound,
    /// in that case max_running_task_count will be coupled with the IO queue depth instead.
    max_running_task_count: CounterInner,
    handled_task_count: ShortCounter,
    unblocked_task_count: ShortCounter,
    total_task_count: ShortCounter,
    count_token: BlockedUsageCountToken,
    usage_queue_token: UsageQueueToken,
}
const_assert_eq!(mem::size_of::<SchedulingStateMachine>(), 56);

impl SchedulingStateMachine {
    pub fn has_no_running_task(&self) -> bool {
        self.running_task_count.is_zero()
    }

    pub fn has_no_active_task(&self) -> bool {
        self.active_task_count.is_zero()
    }

    pub fn has_unblocked_task(&self) -> bool {
        !self.unblocked_task_queue.is_empty()
    }

    pub fn has_runnable_task(&self) -> bool {
        self.has_unblocked_task() && self.is_task_runnable()
    }

    fn is_task_runnable(&self) -> bool {
        self.running_task_count.current() < self.max_running_task_count
    }

    pub fn unblocked_task_queue_count(&self) -> usize {
        self.unblocked_task_queue.len()
    }

    #[cfg(test)]
    fn active_task_count(&self) -> CounterInner {
        self.active_task_count.current()
    }

    #[cfg(test)]
    fn handled_task_count(&self) -> CounterInner {
        self.handled_task_count.current()
    }

    #[cfg(test)]
    fn unblocked_task_count(&self) -> CounterInner {
        self.unblocked_task_count.current()
    }

    #[cfg(test)]
    fn total_task_count(&self) -> CounterInner {
        self.total_task_count.current()
    }

    /// Schedules given `task`, returning it if successful.
    ///
    /// Returns `Some(task)` if it's immediately scheduled. Otherwise, returns `None`,
    /// indicating the scheduled task is blocked currently.
    ///
    /// Note that this function takes ownership of the task to allow for future optimizations.
    #[cfg(any(test, doc))]
    #[must_use]
    pub fn schedule_task(&mut self, task: Task) -> Option<Task> {
        self.schedule_or_buffer_task(task, false)
    }

    /// Adds given `task` to internal buffer, even if it's immediately schedulable otherwise.
    ///
    /// Put differently, buffering means to force the task to be blocked unconditionally after
    /// normal scheduling processing.
    ///
    /// Thus, the task is internally retained inside this [`SchedulingStateMachine`], whether the
    /// task is blocked or not. Eventually, the buffered task will be returned by one of later
    /// invocations [`schedule_next_unblocked_task()`](Self::schedule_next_unblocked_task).
    ///
    /// Note that this function takes ownership of the task to allow for future optimizations.
    pub fn buffer_task(&mut self, task: Task) {
        self.schedule_or_buffer_task(task, true).unwrap_none();
    }

    /// Schedules or buffers given `task`, returning successful one unless buffering is forced.
    ///
    /// Refer to [`schedule_task()`](Self::schedule_task) and
    /// [`buffer_task()`](Self::buffer_task) for the difference between _scheduling_ and
    /// _buffering_ respectively.
    ///
    /// Note that this function takes ownership of the task to allow for future optimizations.
    #[must_use]
    pub fn schedule_or_buffer_task(&mut self, task: Task, force_buffering: bool) -> Option<Task> {
        self.total_task_count.increment_self();
        self.active_task_count.increment_self();
        self.try_lock_usage_queues(task).and_then(|task| {
            // locking succeeded, and then ...
            if !self.is_task_runnable() || force_buffering {
                // ... push to unblocked_task_queue, if buffering is forced.
                self.unblocked_task_count.increment_self();
                self.unblocked_task_queue.push_back(task);
                None
            } else {
                // ... return the task back as schedulable to the caller as-is otherwise.
                self.running_task_count.increment_self();
                Some(task)
            }
        })
    }

    #[must_use]
    pub fn schedule_next_unblocked_task(&mut self) -> Option<Task> {
        if !self.is_task_runnable() {
            return None;
        }

        self.unblocked_task_queue.pop_front().inspect(|_| {
            self.running_task_count.increment_self();
            self.unblocked_task_count.increment_self();
        })
    }

    /// Deschedules given scheduled `task`.
    ///
    /// This must be called exactly once for all scheduled tasks to uphold both
    /// `SchedulingStateMachine` and `UsageQueue` internal state consistency at any given moment of
    /// time. It's serious logic error to call this twice with the same task or none at all after
    /// scheduling. Similarly, calling this with not scheduled task is also forbidden.
    ///
    /// Note that this function intentionally doesn't take ownership of the task to avoid dropping
    /// tasks inside `SchedulingStateMachine` to provide an offloading-based optimization
    /// opportunity for callers.
    pub fn deschedule_task(&mut self, task: &Task) {
        self.running_task_count.decrement_self();
        self.active_task_count.decrement_self();
        self.handled_task_count.increment_self();
        self.unlock_usage_queues(task);
    }

    #[must_use]
    fn try_lock_usage_queues(&mut self, task: Task) -> Option<Task> {
        let mut blocked_usage_count = ShortCounter::zero();

        for context in task.lock_contexts() {
            context.with_usage_queue_mut(&mut self.usage_queue_token, |usage_queue| {
                let lock_result = usage_queue
                    .prepare_lock(&mut self.count_token, &task, context.requested_usage)
                    .and_then(|()| usage_queue.try_lock(&task, context.requested_usage));
                if let Err(()) = lock_result {
                    blocked_usage_count.increment_self();
                    let usage_from_task = (context.requested_usage, task.clone());
                    usage_queue.push_blocked(usage_from_task);
                }
            });
        }

        // no blocked usage count means success
        if blocked_usage_count.is_zero() {
            Some(task)
        } else {
            task.set_blocked_usage_count(&mut self.count_token, blocked_usage_count);
            None
        }
    }

    fn unlock_usage_queues(&mut self, task: &Task) {
        for context in task.lock_contexts() {
            context.with_usage_queue_mut(&mut self.usage_queue_token, |usage_queue| {
                let mut newly_lockable = usage_queue.unlock(task, context.requested_usage);
                while let Some((lockable_usage, lockable_task)) = newly_lockable {
                    usage_queue
                        .try_lock(&lockable_task, lockable_usage)
                        .unwrap();

                    // When `try_unblock()` returns `None` as a failure of unblocking this time,
                    // this means the task is still blocked by other active task's usages. So,
                    // don't push task into unblocked_task_queue yet. It can be assumed that every
                    // task will eventually succeed to be unblocked, and enter in this condition
                    // clause as long as `SchedulingStateMachine` is used correctly.
                    if let Some(unblocked_task) = lockable_task.try_unblock(&mut self.count_token) {
                        self.unblocked_task_queue.push_back(unblocked_task);
                    }

                    // Try to further schedule blocked task for parallelism in the case of readonly
                    // usages
                    newly_lockable = matches!(lockable_usage, RequestedUsage::Readonly)
                        .then(|| usage_queue.pop_lockable_readonly())
                        .flatten();
                }
            });
        }
    }

    /// Creates a new task with [`RuntimeTransaction<SanitizedTransaction>`] with all of
    /// its corresponding [`UsageQueue`]s preloaded.
    ///
    /// Closure (`usage_queue_loader`) is used to delegate the (possibly multi-threaded)
    /// implementation of [`UsageQueue`] look-up by [`pubkey`](Pubkey) to callers. It's the
    /// caller's responsibility to ensure the same instance is returned from the closure, given a
    /// particular pubkey.
    ///
    /// Closure is used here to delegate the responsibility of primary ownership of `UsageQueue`
    /// (and caching/pruning if any) to the caller. `SchedulingStateMachine` guarantees that all of
    /// shared owndership of `UsageQueue`s are released and UsageQueue state is identical to just
    /// after created, if `has_no_active_task()` is `true`. Also note that this is desired for
    /// separation of concern.
    pub fn create_task(
        transaction: RuntimeTransaction<SanitizedTransaction>,
        task_id: OrderedTaskId,
        usage_queue_loader: &mut impl FnMut(Pubkey) -> UsageQueue,
    ) -> Task {
        Self::do_create_task(
            transaction,
            task_id,
            NO_CONSUMED_BLOCK_SIZE,
            MAX_SANITIZED_EPOCH,
            MAX_ALT_INVALIDATION_SLOT,
            usage_queue_loader,
        )
    }

    pub fn create_block_production_task(
        transaction: RuntimeTransaction<SanitizedTransaction>,
        task_id: OrderedTaskId,
        consumed_block_size: BlockSize,
        sanitized_epoch: Epoch,
        alt_invalidation_slot: Slot,
        usage_queue_loader: &mut impl FnMut(Pubkey) -> UsageQueue,
    ) -> Task {
        Self::do_create_task(
            transaction,
            task_id,
            consumed_block_size,
            sanitized_epoch,
            alt_invalidation_slot,
            usage_queue_loader,
        )
    }

    fn do_create_task(
        transaction: RuntimeTransaction<SanitizedTransaction>,
        task_id: OrderedTaskId,
        consumed_block_size: BlockSize,
        sanitized_epoch: Epoch,
        alt_invalidation_slot: Slot,
        usage_queue_loader: &mut impl FnMut(Pubkey) -> UsageQueue,
    ) -> Task {
        // It's crucial for tasks to be validated with
        // `account_locks::validate_account_locks()` prior to the creation.
        // That's because it's part of protocol consensus regarding the
        // rejection of blocks containing malformed transactions
        // (`AccountLoadedTwice` and `TooManyAccountLocks`). Even more,
        // `SchedulingStateMachine` can't properly handle transactions with
        // duplicate addresses (those falling under `AccountLoadedTwice`).
        //
        // However, it's okay for now not to call `::validate_account_locks()`
        // here.
        //
        // Currently `replay_stage` is always calling
        //`::validate_account_locks()` regardless of whether unified-scheduler
        // is enabled or not at the blockstore
        // (`Bank::prepare_sanitized_batch()` is called in
        // `process_entries()`). This verification will be hoisted for
        // optimization when removing
        // `--block-verification-method=blockstore-processor`.
        //
        // As for `banking_stage` with unified scheduler, it will need to run
        // `validate_account_locks()` at least once somewhere in the code path.
        // In the distant future, this function (`create_task()`) should be
        // adjusted so that both stages do the checks before calling this or do
        // the checks here, to simplify the two code paths regarding the
        // essential `validate_account_locks` validation.
        //
        // Lastly, `validate_account_locks()` is currently called in
        // `DefaultTransactionHandler::handle()` via
        // `Bank::prepare_unlocked_batch_from_single_tx()` as well.
        // This redundancy is known. It was just left as-is out of abundance
        // of caution.
        let lock_contexts = transaction
            .message()
            .account_keys()
            .iter()
            .enumerate()
            .map(|(task_id, address)| {
                LockContext::new(
                    usage_queue_loader(*address),
                    if transaction.message().is_writable(task_id) {
                        RequestedUsage::Writable
                    } else {
                        RequestedUsage::Readonly
                    },
                )
            })
            .collect();

        Task::new(TaskInner {
            transaction,
            task_id,
            lock_contexts,
            blocked_usage_count: TokenCell::new(ShortCounter::zero()),
            consumed_block_size,
            sanitized_epoch,
            alt_invalidation_slot,
        })
    }

    /// Rewind the inactive state machine to be initialized
    ///
    /// This isn't called _reset_ to indicate this isn't safe to call this at any given moment.
    /// This panics if the state machine hasn't properly been finished (i.e. there should be no
    /// active task) to uphold invariants of [`UsageQueue`]s.
    ///
    /// This method is intended to reuse SchedulingStateMachine instance (to avoid its `unsafe`
    /// [constructor](SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling)
    /// as much as possible) and its (possibly cached) associated [`UsageQueue`]s for processing
    /// other slots.
    ///
    /// There's a related method called [`clear_and_reinitialize()`](Self::clear_and_reinitialize).
    pub fn reinitialize(&mut self) {
        assert!(self.has_no_active_task());
        assert_eq!(self.running_task_count.current(), 0);
        assert_eq!(self.unblocked_task_queue.len(), 0);
        // nice trick to ensure all fields are handled here if new one is added.
        let Self {
            unblocked_task_queue: _,
            active_task_count,
            running_task_count: _,
            max_running_task_count: _,
            handled_task_count,
            unblocked_task_count,
            total_task_count,
            count_token: _,
            usage_queue_token: _,
            // don't add ".." here
        } = self;
        active_task_count.reset_to_zero();
        handled_task_count.reset_to_zero();
        unblocked_task_count.reset_to_zero();
        total_task_count.reset_to_zero();
    }

    /// Clear all buffered tasks and immediately rewind the state machine to be initialized
    ///
    /// This method _may_ panic if there are tasks which has been scheduled but hasn't been
    /// descheduled yet (called active tasks). This is due to the invocation of
    /// [`reinitialize()`](Self::reinitialize) at last. On the other hand, it's guaranteed not to
    /// panic otherwise. That's because the first clearing step effectively relaxes the runtime
    /// invariant of `reinitialize()` by making the state machine _inactive_ beforehand. After a
    /// successful operation, this method returns the number of cleared tasks.
    ///
    /// Somewhat surprisingly, the clearing logic is same as the normal (de-)scheduling operation
    /// because it is still the fastest way to just clear all tasks, under the consideration of
    /// potential later use of [`UsageQueue`]s. That's because `state_machine` doesn't maintain _the
    /// global list_ of tasks. Maintaining such one would incur a needless overhead on scheduling,
    /// which isn't strictly needed otherwise.
    ///
    /// Moreover, the descheduling operation is rather heavily optimized to begin with. All
    /// collection ops are just O(1) over total N of addresses accessed by all active tasks with
    /// no amortized mem ops.
    ///
    /// Whatever the algorithm is chosen, the ultimate goal of this operation is to clear all usage
    /// queues. Toward to that end, one may create a temporary hash set over [`UsageQueue`]s on the
    /// fly alternatively. However, that would be costlier than the above usual descheduling
    /// approach due to extra mem ops and many lookups/insertions.
    pub fn clear_and_reinitialize(&mut self) -> usize {
        let mut count = ShortCounter::zero();
        while let Some(task) = self.schedule_next_unblocked_task() {
            self.deschedule_task(&task);
            count.increment_self();
        }
        self.reinitialize();
        count.current().try_into().unwrap()
    }

    /// Creates a new instance of [`SchedulingStateMachine`] with its `unsafe` fields created as
    /// well, thus carrying over `unsafe`.
    ///
    /// # Safety
    /// Call this exactly once for each thread. See [`TokenCell`] for details.
    #[must_use]
    pub unsafe fn exclusively_initialize_current_thread_for_scheduling(
        max_running_task_count: Option<usize>,
    ) -> Self {
        // As documented at `CounterInner`, don't expose rather opinionated choice of unsigned
        // integer type (`u32`) to outer world. So, take more conventional `usize` and convert it
        // to `CounterInner` here while uncontroversially treating `None` as no limit effectively.
        let max_running_task_count = max_running_task_count
            .unwrap_or(CounterInner::MAX as usize)
            .try_into()
            .unwrap();

        Self {
            // It's very unlikely this is desired to be configurable, like
            // `UsageQueueInner::blocked_usages_from_tasks`'s cap.
            unblocked_task_queue: VecDeque::with_capacity(1024),
            active_task_count: ShortCounter::zero(),
            running_task_count: ShortCounter::zero(),
            max_running_task_count,
            handled_task_count: ShortCounter::zero(),
            unblocked_task_count: ShortCounter::zero(),
            total_task_count: ShortCounter::zero(),
            count_token: unsafe { BlockedUsageCountToken::assume_exclusive_mutating_thread() },
            usage_queue_token: unsafe { UsageQueueToken::assume_exclusive_mutating_thread() },
        }
    }

    #[cfg(test)]
    unsafe fn exclusively_initialize_current_thread_for_scheduling_for_test() -> Self {
        unsafe { Self::exclusively_initialize_current_thread_for_scheduling(None) }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_instruction::{AccountMeta, Instruction},
        solana_message::Message,
        solana_pubkey::Pubkey,
        solana_transaction::{sanitized::SanitizedTransaction, Transaction},
        std::{
            cell::RefCell,
            collections::HashMap,
            panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
            rc::Rc,
        },
        test_case::test_matrix,
    };

    fn simplest_transaction() -> RuntimeTransaction<SanitizedTransaction> {
        let message = Message::new(&[], Some(&Pubkey::new_unique()));
        let unsigned = Transaction::new_unsigned(message);
        RuntimeTransaction::from_transaction_for_tests(unsigned)
    }

    fn transaction_with_readonly_address(
        address: Pubkey,
    ) -> RuntimeTransaction<SanitizedTransaction> {
        transaction_with_readonly_address_with_payer(address, &Pubkey::new_unique())
    }

    fn transaction_with_readonly_address_with_payer(
        address: Pubkey,
        payer: &Pubkey,
    ) -> RuntimeTransaction<SanitizedTransaction> {
        let instruction = Instruction {
            program_id: Pubkey::default(),
            accounts: vec![AccountMeta::new_readonly(address, false)],
            data: vec![],
        };
        let message = Message::new(&[instruction], Some(payer));
        let unsigned = Transaction::new_unsigned(message);
        RuntimeTransaction::from_transaction_for_tests(unsigned)
    }

    fn transaction_with_writable_address(
        address: Pubkey,
    ) -> RuntimeTransaction<SanitizedTransaction> {
        transaction_with_writable_address_with_payer(address, &Pubkey::new_unique())
    }

    fn transaction_with_writable_address_with_payer(
        address: Pubkey,
        payer: &Pubkey,
    ) -> RuntimeTransaction<SanitizedTransaction> {
        let instruction = Instruction {
            program_id: Pubkey::default(),
            accounts: vec![AccountMeta::new(address, false)],
            data: vec![],
        };
        let message = Message::new(&[instruction], Some(payer));
        let unsigned = Transaction::new_unsigned(message);
        RuntimeTransaction::from_transaction_for_tests(unsigned)
    }

    fn create_address_loader(
        usage_queues: Option<Rc<RefCell<HashMap<Pubkey, UsageQueue>>>>,
        capability: &Capability,
    ) -> impl FnMut(Pubkey) -> UsageQueue + use<'_> {
        let usage_queues = usage_queues.unwrap_or_default();
        move |address| {
            usage_queues
                .borrow_mut()
                .entry(address)
                .or_insert_with(|| UsageQueue::new(capability))
                .clone()
        }
    }

    #[test]
    fn test_scheduling_state_machine_creation() {
        let state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_eq!(state_machine.active_task_count(), 0);
        assert_eq!(state_machine.total_task_count(), 0);
        assert!(state_machine.has_no_active_task());
    }

    #[test]
    fn test_scheduling_state_machine_good_reinitialization() {
        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        state_machine.total_task_count.increment_self();
        assert_eq!(state_machine.total_task_count(), 1);
        state_machine.reinitialize();
        assert_eq!(state_machine.total_task_count(), 0);
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    #[should_panic(expected = "assertion failed: self.has_no_active_task()")]
    fn test_scheduling_state_machine_bad_reinitialization(capability: Capability) {
        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        let address_loader = &mut create_address_loader(None, &capability);
        let task = SchedulingStateMachine::create_task(simplest_transaction(), 3, address_loader);
        state_machine.schedule_task(task.clone()).unwrap();
        let bad_reinitialize = catch_unwind(AssertUnwindSafe(|| state_machine.reinitialize()));

        // Avoid leaks as dutifully detected by Miri; Namely, tasks could be leaked due to
        // transient circular references of active tasks by PriorityUsage at stack unwinding, which
        // only happens under known panic conditions.
        // To avoid that deschedule the task after the panic. Doing this beforehand won't cause the
        // panic, which we'd like to test here....
        state_machine.deschedule_task(&task);
        if let Err(some_panic) = bad_reinitialize {
            resume_unwind(some_panic);
        }
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    fn test_create_task(capability: Capability) {
        let sanitized = simplest_transaction();
        let signature = *sanitized.signature();
        let task = SchedulingStateMachine::create_task(sanitized, 3, &mut |_| {
            UsageQueue::new(&capability)
        });
        assert_eq!(task.task_id(), 3);
        assert_eq!(task.transaction().signature(), &signature);
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    fn test_non_conflicting_task_related_counts(capability: Capability) {
        let sanitized = simplest_transaction();
        let address_loader = &mut create_address_loader(None, &capability);
        let task = SchedulingStateMachine::create_task(sanitized, 3, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        let task = state_machine.schedule_task(task).unwrap();
        assert_eq!(state_machine.active_task_count(), 1);
        assert_eq!(state_machine.total_task_count(), 1);
        state_machine.deschedule_task(&task);
        assert_eq!(state_machine.active_task_count(), 0);
        assert_eq!(state_machine.total_task_count(), 1);
        assert!(state_machine.has_no_active_task());
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    fn test_conflicting_task_related_counts(capability: Capability) {
        let sanitized = simplest_transaction();
        let address_loader = &mut create_address_loader(None, &capability);
        let task1 = SchedulingStateMachine::create_task(sanitized.clone(), 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized.clone(), 102, address_loader);
        let task3 = SchedulingStateMachine::create_task(sanitized.clone(), 103, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_id()),
            Some(101)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);

        state_machine.deschedule_task(&task1);
        assert!(state_machine.has_unblocked_task());
        assert_eq!(state_machine.unblocked_task_queue_count(), 1);

        // unblocked_task_count() should be incremented
        assert_eq!(state_machine.unblocked_task_count(), 0);
        assert_eq!(
            state_machine
                .schedule_next_unblocked_task()
                .map(|t| t.task_id()),
            Some(102)
        );
        assert_eq!(state_machine.unblocked_task_count(), 1);

        // there's no blocked task anymore; calling schedule_next_unblocked_task should be noop and
        // shouldn't increment the unblocked_task_count().
        assert!(!state_machine.has_unblocked_task());
        assert_matches!(state_machine.schedule_next_unblocked_task(), None);
        assert_eq!(state_machine.unblocked_task_count(), 1);

        assert_eq!(state_machine.unblocked_task_queue_count(), 0);
        state_machine.deschedule_task(&task2);

        assert_matches!(
            state_machine
                .schedule_task(task3.clone())
                .map(|task| task.task_id()),
            Some(103)
        );
        state_machine.deschedule_task(&task3);
        assert!(state_machine.has_no_active_task());
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    fn test_existing_blocking_task_then_newly_scheduled_task(capability: Capability) {
        let sanitized = simplest_transaction();
        let address_loader = &mut create_address_loader(None, &capability);
        let task1 = SchedulingStateMachine::create_task(sanitized.clone(), 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized.clone(), 102, address_loader);
        let task3 = SchedulingStateMachine::create_task(sanitized.clone(), 103, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_id()),
            Some(101)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);

        assert_eq!(state_machine.unblocked_task_queue_count(), 0);
        state_machine.deschedule_task(&task1);
        assert_eq!(state_machine.unblocked_task_queue_count(), 1);

        // new task is arriving after task1 is already descheduled and task2 got unblocked
        assert_matches!(state_machine.schedule_task(task3.clone()), None);

        assert_eq!(state_machine.unblocked_task_count(), 0);
        assert_matches!(
            state_machine
                .schedule_next_unblocked_task()
                .map(|t| t.task_id()),
            Some(102)
        );
        assert_eq!(state_machine.unblocked_task_count(), 1);

        state_machine.deschedule_task(&task2);

        assert_matches!(
            state_machine
                .schedule_next_unblocked_task()
                .map(|t| t.task_id()),
            Some(103)
        );
        assert_eq!(state_machine.unblocked_task_count(), 2);

        state_machine.deschedule_task(&task3);
        assert!(state_machine.has_no_active_task());
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    fn test_multiple_readonly_task_and_counts(capability: Capability) {
        let conflicting_address = Pubkey::new_unique();
        let sanitized1 = transaction_with_readonly_address(conflicting_address);
        let sanitized2 = transaction_with_readonly_address(conflicting_address);
        let address_loader = &mut create_address_loader(None, &capability);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 102, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        // both of read-only tasks should be immediately runnable
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_id()),
            Some(101)
        );
        assert_matches!(
            state_machine
                .schedule_task(task2.clone())
                .map(|t| t.task_id()),
            Some(102)
        );

        assert_eq!(state_machine.active_task_count(), 2);
        assert_eq!(state_machine.handled_task_count(), 0);
        assert_eq!(state_machine.unblocked_task_queue_count(), 0);
        state_machine.deschedule_task(&task1);
        assert_eq!(state_machine.active_task_count(), 1);
        assert_eq!(state_machine.handled_task_count(), 1);
        assert_eq!(state_machine.unblocked_task_queue_count(), 0);
        state_machine.deschedule_task(&task2);
        assert_eq!(state_machine.active_task_count(), 0);
        assert_eq!(state_machine.handled_task_count(), 2);
        assert!(state_machine.has_no_active_task());
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    fn test_all_blocking_readable_tasks_block_writable_task(capability: Capability) {
        let conflicting_address = Pubkey::new_unique();
        let sanitized1 = transaction_with_readonly_address(conflicting_address);
        let sanitized2 = transaction_with_readonly_address(conflicting_address);
        let sanitized3 = transaction_with_writable_address(conflicting_address);
        let address_loader = &mut create_address_loader(None, &capability);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 102, address_loader);
        let task3 = SchedulingStateMachine::create_task(sanitized3, 103, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_id()),
            Some(101)
        );
        assert_matches!(
            state_machine
                .schedule_task(task2.clone())
                .map(|t| t.task_id()),
            Some(102)
        );
        assert_matches!(state_machine.schedule_task(task3.clone()), None);

        assert_eq!(state_machine.active_task_count(), 3);
        assert_eq!(state_machine.handled_task_count(), 0);
        assert_eq!(state_machine.unblocked_task_queue_count(), 0);
        state_machine.deschedule_task(&task1);
        assert_eq!(state_machine.active_task_count(), 2);
        assert_eq!(state_machine.handled_task_count(), 1);
        assert_eq!(state_machine.unblocked_task_queue_count(), 0);
        assert_matches!(state_machine.schedule_next_unblocked_task(), None);
        state_machine.deschedule_task(&task2);
        assert_eq!(state_machine.active_task_count(), 1);
        assert_eq!(state_machine.handled_task_count(), 2);
        assert_eq!(state_machine.unblocked_task_queue_count(), 1);
        // task3 is finally unblocked after all of readable tasks (task1 and task2) is finished.
        assert_matches!(
            state_machine
                .schedule_next_unblocked_task()
                .map(|t| t.task_id()),
            Some(103)
        );
        state_machine.deschedule_task(&task3);
        assert!(state_machine.has_no_active_task());
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    fn test_readonly_then_writable_then_readonly_linearized(capability: Capability) {
        let conflicting_address = Pubkey::new_unique();
        let sanitized1 = transaction_with_readonly_address(conflicting_address);
        let sanitized2 = transaction_with_writable_address(conflicting_address);
        let sanitized3 = transaction_with_readonly_address(conflicting_address);
        let address_loader = &mut create_address_loader(None, &capability);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 102, address_loader);
        let task3 = SchedulingStateMachine::create_task(sanitized3, 103, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_id()),
            Some(101)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);
        assert_matches!(state_machine.schedule_task(task3.clone()), None);

        assert_matches!(state_machine.schedule_next_unblocked_task(), None);
        state_machine.deschedule_task(&task1);
        assert_matches!(
            state_machine
                .schedule_next_unblocked_task()
                .map(|t| t.task_id()),
            Some(102)
        );
        assert_matches!(state_machine.schedule_next_unblocked_task(), None);
        state_machine.deschedule_task(&task2);
        assert_matches!(
            state_machine
                .schedule_next_unblocked_task()
                .map(|t| t.task_id()),
            Some(103)
        );
        assert_matches!(state_machine.schedule_next_unblocked_task(), None);
        state_machine.deschedule_task(&task3);
        assert!(state_machine.has_no_active_task());
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    fn test_readonly_then_writable(capability: Capability) {
        let conflicting_address = Pubkey::new_unique();
        let sanitized1 = transaction_with_readonly_address(conflicting_address);
        let sanitized2 = transaction_with_writable_address(conflicting_address);
        let address_loader = &mut create_address_loader(None, &capability);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 102, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_id()),
            Some(101)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);

        // descheduling read-locking task1 should equate to unblocking write-locking task2
        state_machine.deschedule_task(&task1);
        assert_matches!(
            state_machine
                .schedule_next_unblocked_task()
                .map(|t| t.task_id()),
            Some(102)
        );
        state_machine.deschedule_task(&task2);
        assert!(state_machine.has_no_active_task());
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    fn test_blocked_tasks_writable_2_readonly_then_writable(capability: Capability) {
        let conflicting_address = Pubkey::new_unique();
        let sanitized1 = transaction_with_writable_address(conflicting_address);
        let sanitized2 = transaction_with_readonly_address(conflicting_address);
        let sanitized3 = transaction_with_readonly_address(conflicting_address);
        let sanitized4 = transaction_with_writable_address(conflicting_address);
        let address_loader = &mut create_address_loader(None, &capability);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 102, address_loader);
        let task3 = SchedulingStateMachine::create_task(sanitized3, 103, address_loader);
        let task4 = SchedulingStateMachine::create_task(sanitized4, 104, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_id()),
            Some(101)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);
        assert_matches!(state_machine.schedule_task(task3.clone()), None);
        assert_matches!(state_machine.schedule_task(task4.clone()), None);

        state_machine.deschedule_task(&task1);
        assert_matches!(
            state_machine
                .schedule_next_unblocked_task()
                .map(|t| t.task_id()),
            Some(102)
        );
        assert_matches!(
            state_machine
                .schedule_next_unblocked_task()
                .map(|t| t.task_id()),
            Some(103)
        );
        // the above deschedule_task(task1) call should only unblock task2 and task3 because these
        // are read-locking. And shouldn't unblock task4 because it's write-locking
        assert_matches!(state_machine.schedule_next_unblocked_task(), None);

        state_machine.deschedule_task(&task2);
        // still task4 is blocked...
        assert_matches!(state_machine.schedule_next_unblocked_task(), None);

        state_machine.deschedule_task(&task3);
        // finally task4 should be unblocked
        assert_matches!(
            state_machine
                .schedule_next_unblocked_task()
                .map(|t| t.task_id()),
            Some(104)
        );
        state_machine.deschedule_task(&task4);
        assert!(state_machine.has_no_active_task());
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    fn test_gradual_locking(capability: Capability) {
        let conflicting_address = Pubkey::new_unique();
        let sanitized1 = transaction_with_writable_address(conflicting_address);
        let sanitized2 = transaction_with_writable_address(conflicting_address);
        let usage_queues = Rc::new(RefCell::new(HashMap::new()));
        let address_loader = &mut create_address_loader(Some(usage_queues.clone()), &capability);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 102, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_id()),
            Some(101)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);
        let usage_queues = usage_queues.borrow_mut();
        let usage_queue = usage_queues.get(&conflicting_address).unwrap();
        usage_queue
            .0
            .with_borrow_mut(&mut state_machine.usage_queue_token, |usage_queue| {
                assert_matches!(usage_queue.current_usage(), Some(RequestedUsage::Writable));
            });
        // task2's fee payer should have been locked already even if task2 is blocked still via the
        // above the schedule_task(task2) call
        let fee_payer = task2.transaction().message().fee_payer();
        let usage_queue = usage_queues.get(fee_payer).unwrap();
        usage_queue
            .0
            .with_borrow_mut(&mut state_machine.usage_queue_token, |usage_queue| {
                assert_matches!(usage_queue.current_usage(), Some(RequestedUsage::Writable));
            });
        state_machine.deschedule_task(&task1);
        assert_matches!(
            state_machine
                .schedule_next_unblocked_task()
                .map(|t| t.task_id()),
            Some(102)
        );
        state_machine.deschedule_task(&task2);
        assert!(state_machine.has_no_active_task());
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    #[should_panic(expected = "internal error: entered unreachable code")]
    fn test_unreachable_unlock_conditions1(capability: Capability) {
        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        let usage_queue = UsageQueue::new(&capability);
        let sanitized = simplest_transaction();
        let task = &SchedulingStateMachine::create_task(sanitized, 3, &mut |_| {
            UsageQueue::new(&capability)
        });
        usage_queue
            .0
            .with_borrow_mut(&mut state_machine.usage_queue_token, |usage_queue| {
                let _ = usage_queue.unlock(task, RequestedUsage::Writable);
            });
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    #[should_panic(
        expected = "assertion failed: `Readonly` does not match `RequestedUsage::Writable`"
    )]
    fn test_unreachable_unlock_conditions2(capability: Capability) {
        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        let usage_queue = UsageQueue::new(&capability);
        let sanitized = simplest_transaction();
        let task = &SchedulingStateMachine::create_task(sanitized, 3, &mut |_| {
            UsageQueue::new(&capability)
        });
        usage_queue
            .0
            .with_borrow_mut(&mut state_machine.usage_queue_token, |usage_queue| {
                usage_queue.update_current_usage(RequestedUsage::Writable, task);
                let _ = usage_queue.unlock(task, RequestedUsage::Readonly);
            });
    }

    #[test_matrix([Capability::FifoQueueing, Capability::PriorityQueueing])]
    #[should_panic(expected = "internal error: entered unreachable code")]
    fn test_unreachable_unlock_conditions3(capability: Capability) {
        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        let usage_queue = UsageQueue::new(&capability);
        let sanitized = simplest_transaction();
        let task = &SchedulingStateMachine::create_task(sanitized, 3, &mut |_| {
            UsageQueue::new(&capability)
        });
        usage_queue
            .0
            .with_borrow_mut(&mut state_machine.usage_queue_token, |usage_queue| {
                usage_queue.update_current_usage(RequestedUsage::Readonly, task);
                let _ = usage_queue.unlock(task, RequestedUsage::Writable);
            });
    }

    mod reblocking {
        use super::{RequestedUsage::*, *};

        #[track_caller]
        fn assert_task_index(actual: Option<Task>, expected: Option<OrderedTaskId>) {
            assert_eq!(actual.map(|task| task.task_id()), expected);
        }

        macro_rules! assert_task_index {
            ($left:expr, $right:expr) => {
                assert_task_index($left, $right);
            };
        }

        fn setup() -> (
            SchedulingStateMachine,
            impl FnMut((RequestedUsage, Pubkey), OrderedTaskId) -> Task,
            Task,
        ) {
            let mut state_machine = unsafe {
                SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
            };

            let payer = Pubkey::new_unique();
            let mut address_loader = create_address_loader(None, &Capability::PriorityQueueing);

            let mut create_task = move |(requested_usage, address), task_id| match requested_usage {
                RequestedUsage::Readonly => SchedulingStateMachine::create_task(
                    transaction_with_readonly_address_with_payer(address, &payer),
                    task_id,
                    &mut address_loader,
                ),
                RequestedUsage::Writable => SchedulingStateMachine::create_task(
                    transaction_with_writable_address_with_payer(address, &payer),
                    task_id,
                    &mut address_loader,
                ),
            };

            let t0_block_others = create_task((Writable, Pubkey::new_unique()), 100);
            assert_task_index!(
                state_machine.schedule_task(t0_block_others.clone()),
                Some(100)
            );

            (state_machine, create_task, t0_block_others)
        }

        #[test]
        fn test_reblocked_tasks_lower_write_then_higher_write() {
            let (mut s, mut create_task, t0_block_others) = setup();

            let reblocked_address = Pubkey::new_unique();
            let t1_reblocked = create_task((Writable, reblocked_address), 102);
            let t2_force_locked = create_task((Writable, reblocked_address), 10);

            assert_task_index!(s.schedule_task(t1_reblocked.clone()), None);
            assert_task_index!(s.schedule_task(t2_force_locked.clone()), None);

            s.deschedule_task(&t0_block_others);
            assert_task_index!(s.schedule_next_unblocked_task(), Some(10));
            s.deschedule_task(&t2_force_locked);
            assert_task_index!(s.schedule_next_unblocked_task(), Some(102));
            s.deschedule_task(&t1_reblocked);
            assert!(s.has_no_active_task());
        }

        #[test]
        fn test_reblocked_tasks_lower_write_then_higher_read() {
            let (mut s, mut create_task, t0_block_others) = setup();

            let reblocked_address = Pubkey::new_unique();
            let t1_reblocked = create_task((Writable, reblocked_address), 102);
            let t2_force_locked = create_task((Readonly, reblocked_address), 10);

            assert_task_index!(s.schedule_task(t1_reblocked.clone()), None);
            assert_task_index!(s.schedule_task(t2_force_locked.clone()), None);

            s.deschedule_task(&t0_block_others);
            assert_task_index!(s.schedule_next_unblocked_task(), Some(10));
            s.deschedule_task(&t2_force_locked);
            assert_task_index!(s.schedule_next_unblocked_task(), Some(102));
            s.deschedule_task(&t1_reblocked);
            assert!(s.has_no_active_task());
        }

        #[test]
        fn test_reblocked_tasks_lower_read_then_higher_read() {
            let (mut s, mut create_task, t0_block_others) = setup();

            let reblocked_address = Pubkey::new_unique();
            let t1_not_reblocked = create_task((Readonly, reblocked_address), 102);
            let t2_skipped = create_task((Writable, reblocked_address), 103);
            let t3_force_locked = create_task((Readonly, reblocked_address), 10);

            assert_task_index!(s.schedule_task(t1_not_reblocked.clone()), None);
            assert_task_index!(s.schedule_task(t2_skipped.clone()), None);
            assert_task_index!(s.schedule_task(t3_force_locked.clone()), None);

            s.deschedule_task(&t0_block_others);
            assert_task_index!(s.schedule_next_unblocked_task(), Some(10));
            s.deschedule_task(&t3_force_locked);
            assert_task_index!(s.schedule_next_unblocked_task(), Some(102));
            s.deschedule_task(&t1_not_reblocked);
            assert_task_index!(s.schedule_next_unblocked_task(), Some(103));
            s.deschedule_task(&t2_skipped);
            assert!(s.has_no_active_task());
        }

        #[test]
        fn test_reblocked_tasks_lower_read_then_higher_write_full() {
            let (mut s, mut create_task, t0_block_others) = setup();

            let reblocked_address = Pubkey::new_unique();
            let t1_reblocked = create_task((Readonly, reblocked_address), 102);
            let t2_force_locked = create_task((Writable, reblocked_address), 10);

            assert_task_index!(s.schedule_task(t1_reblocked.clone()), None);
            assert_task_index!(s.schedule_task(t2_force_locked.clone()), None);

            s.deschedule_task(&t0_block_others);
            assert_task_index!(s.schedule_next_unblocked_task(), Some(10));
            s.deschedule_task(&t2_force_locked);
            assert_task_index!(s.schedule_next_unblocked_task(), Some(102));
            s.deschedule_task(&t1_reblocked);
            assert!(s.has_no_active_task());
        }
    }
}
