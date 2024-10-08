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
//! of the exeuction via [`::deschedule_task()`](SchedulingStateMachine::deschedule_task), so that
//! conflicting tasks can be returned from
//! [`::schedule_next_buffered_task()`](SchedulingStateMachine::schedule_next_buffered_task) as
//! newly-unblocked runnable ones.
//!
//! The design principle of this crate (`solana-unified-scheduler-logic`) is simplicity for the
//! separation of concern. It is interacted only with a few of its public API by
//! `solana-unified-scheduler-pool`. This crate doesn't know about banks, slots, solana-runtime,
//! threads, crossbeam-channel at all. Becasue of this, it's deterministic, easy-to-unit-test, and
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
//! degration.
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
//! schduler thread (i.e.  constant; no allocations nor syscalls), while being proportional to the
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
    more_asserts::assert_gt,
    solana_sdk::{pubkey::Pubkey, scheduling::SchedulingMode, transaction::SanitizedTransaction},
    static_assertions::const_assert_eq,
    std::{
        mem,
        sync::Arc,
    },
};
use std::collections::BTreeSet;
use std::cell::Cell;

/// Internal utilities. Namely this contains [`ShortCounter`] and [`TokenCell`].
mod utils {
    use std::{
        any::{self, TypeId},
        cell::{RefCell, UnsafeCell},
        collections::BTreeSet,
        marker::PhantomData,
        thread,
    };

    /// A really tiny counter to hide `.checked_{add,sub}` all over the place.
    ///
    /// It's caller's reponsibility to ensure this (backed by [`u32`]) never overflow.
    #[derive(Debug, Clone, Copy)]
    pub(super) struct ShortCounter(u32);

    impl ShortCounter {
        pub(super) fn zero() -> Self {
            Self(0)
        }

        pub(super) fn one() -> Self {
            Self(1)
        }

        pub(super) fn is_zero(&self) -> bool {
            self.0 == 0
        }

        pub(super) fn current(&self) -> u32 {
            self.0
        }

        #[must_use]
        #[track_caller]
        pub(super) fn increment(self) -> Self {
            Self(self.0 + 1)
        }

        #[must_use]
        #[track_caller]
        pub(super) fn decrement(self) -> Self {
            Self(self.0 - 1)
        }

        #[track_caller]
        pub(super) fn increment_self(&mut self) -> &mut Self {
            *self = self.increment();
            self
        }

        #[track_caller]
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
    #[derive(Debug)]
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
#[derive(Clone, Debug)]
pub struct Task(rclite::Rc<TaskInner>);

unsafe impl enum_ptr::Aligned for Task {
    const ALIGNMENT: usize = std::mem::align_of::<TaskInner>();
}

unsafe impl Sync for Task {}
unsafe impl Send for Task {}

impl Task {
    fn new(task: TaskInner) -> Self {
        Self(rclite::Rc::new(task))
    }

    #[must_use]
    fn try_unblock(self, token: &mut BlockedUsageCountToken) -> Option<Task> {
        let did_unblock = self
            .blocked_usage_count
            .with_borrow_mut(token, |counter_with_status| {
                let c = counter_with_status.count();
                counter_with_status.set_count(c - 1);
                c == 1
            });
        did_unblock.then_some(self)
    }
}

impl std::ops::Deref for Task {
    type Target = TaskInner;
    fn deref(&self) -> &<Self as std::ops::Deref>::Target { &*self.0 }
}

const_assert_eq!(mem::size_of::<Task>(), 8);

/// [`Token`] for [`UsageQueue`].
type UsageQueueToken = Token<UsageQueueInner>;
const_assert_eq!(mem::size_of::<UsageQueueToken>(), 0);

/// [`Token`] for [task](Task)'s [internal mutable data](`TaskInner::blocked_usage_count`).
type BlockedUsageCountToken = Token<CounterWithStatus>;
const_assert_eq!(mem::size_of::<BlockedUsageCountToken>(), 0);

pub type Index = u128;

#[derive(Debug, PartialEq, Clone, Copy)]
#[repr(u8)]
enum TaskStatus {
    Buffered,
    Executed,
    Unlocked,
}

impl TaskStatus {
    const fn into_bits(self) -> u8 {
        self as _
    }
    const fn from_bits(value: u8) -> Self {
        match value {
            0 => Self::Buffered,
            1 => Self::Executed,
            _ => Self::Unlocked,
        }
    }
}

use bitfield_struct::bitfield;
#[bitfield(u32)]
struct CounterWithStatus {
    #[bits(2)]
    status: TaskStatus,
    #[bits(30)]
    count: u32,
}

#[repr(C, packed)]
struct PackedTaskInner {
    index: Index,
    lock_context_and_transaction: Box<(Vec<Cell<Compact<LockContext>>>, Box<SanitizedTransaction>)>,
}
const_assert_eq!(mem::size_of::<PackedTaskInner>(), 24);

impl std::fmt::Debug for PackedTaskInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let index = self.index;
        f.debug_struct("PackedTaskInner")
            .field("index", &index)
            //.field("lock_contexts", &self.lock_context_and_transaction.0)
            .field("transaction", &self.lock_context_and_transaction.1)
            .finish()
    }
}

/// Internal scheduling data about a particular task.
#[derive(Debug)]
pub struct TaskInner {
    /// The index of a transaction in ledger entries; not used by SchedulingStateMachine by itself.
    /// Carrying this along with the transaction is needed to properly record the execution result
    /// of it.
    packed_task_inner: PackedTaskInner,
    blocked_usage_count: TokenCell<CounterWithStatus>,
}

struct RcInnerDemo {
  data: TaskInner,
  counter: std::cell::Cell<u32>,
}
const_assert_eq!(mem::size_of::<RcInnerDemo>(), 32);

impl TaskInner {
    pub fn task_index(&self) -> Index {
        self.index()
    }

    pub fn transaction(&self) -> &SanitizedTransaction {
        &self.packed_task_inner.lock_context_and_transaction.1
    }

    pub fn index(&self) -> Index {
        self.packed_task_inner.index
    }

    fn lock_contexts(&self) -> &[Cell<Compact<LockContext>>] {
        &self.packed_task_inner.lock_context_and_transaction.0
    }

    fn blocked_usage_count(&self, token: &mut BlockedUsageCountToken) -> u32 {
        self.blocked_usage_count
            .with_borrow_mut(token, |counter_with_status| counter_with_status.count())
    }

    fn has_blocked_usage(&self, token: &mut BlockedUsageCountToken) -> bool {
        self.blocked_usage_count(token) > 0
    }

    fn set_blocked_usage_count(&self, token: &mut BlockedUsageCountToken, count: ShortCounter) {
        self.blocked_usage_count
            .with_borrow_mut(token, |(counter_with_status)| {
                counter_with_status.set_count(count.current());
            })
    }

    fn increment_blocked_usage_count(&self, token: &mut BlockedUsageCountToken) {
        self.blocked_usage_count
            .with_borrow_mut(token, |counter_with_status| {
                counter_with_status.set_count(counter_with_status.count() + 1)
            })
    }

    fn mark_as_executed(&self, token: &mut BlockedUsageCountToken) {
        self.blocked_usage_count
            .with_borrow_mut(token, |counter_with_status| {
                counter_with_status.set_status(TaskStatus::Executed);
            })
    }

    fn mark_as_buffered(&self, token: &mut BlockedUsageCountToken) {
        self.blocked_usage_count
            .with_borrow_mut(token, |counter_with_status| {
                counter_with_status.set_status(TaskStatus::Buffered);
            })
    }

    fn mark_as_unlocked(&self, token: &mut BlockedUsageCountToken) {
        self.blocked_usage_count
            .with_borrow_mut(token, |counter_with_status| {
                counter_with_status.set_status(TaskStatus::Unlocked);
            })
    }

    fn is_buffered(&self, token: &mut BlockedUsageCountToken) -> bool {
        self.blocked_usage_count
            .with_borrow_mut(token, |counter_with_status| {
                matches!(counter_with_status.status(), TaskStatus::Buffered)
            })
    }

    fn is_executed(&self, token: &mut BlockedUsageCountToken) -> bool {
        self.blocked_usage_count
            .with_borrow_mut(token, |counter_with_status| {
                matches!(counter_with_status.status(), TaskStatus::Executed)
            })
    }

    fn is_unlocked(&self, token: &mut BlockedUsageCountToken) -> bool {
        self.blocked_usage_count
            .with_borrow_mut(token, |counter_with_status| {
                matches!(counter_with_status.status(), TaskStatus::Unlocked)
            })
    }

    fn status(&self, token: &mut BlockedUsageCountToken) -> TaskStatus {
        self.blocked_usage_count
            .with_borrow_mut(token, |counter_with_status| {
                counter_with_status.status()
            })
    }
}

/// [`Task`]'s per-address context to lock a [usage_queue](UsageQueue) with [certain kind of
/// request](RequestedUsage).
#[derive(Debug, EnumPtr)]
#[repr(C, usize)]
enum LockContext {
    Readonly(UsageQueue),
    Writable(UsageQueue),
    Readonly2(UsageQueue),
    Writable2(UsageQueue),
}
const_assert_eq!(mem::size_of::<LockContext>(), 16);
const_assert_eq!(mem::size_of::<Compact<LockContext>>(), 8);

impl LockContext {
    fn new(usage_queue: UsageQueue, requested_usage: RequestedUsage) -> Self {
        match requested_usage {
            RequestedUsage::Readonly => Self::Readonly(usage_queue),
            RequestedUsage::Writable => Self::Writable(usage_queue),
        }
    }

    fn requested_usage2(&self) -> RequestedUsage {
        match self {
            Self::Readonly(_) => RequestedUsage::Readonly,
            Self::Writable(_) => RequestedUsage::Writable,
            Self::Readonly2(_) => RequestedUsage::Readonly,
            Self::Writable2(_) => RequestedUsage::Writable,
        }
    }

    fn usage_from_task(&self, task: Task) -> UsageFromTask {
        match self {
            Self::Readonly(_) => UsageFromTask::Readonly(task),
            Self::Writable(_) => UsageFromTask::Writable(task),
            Self::Readonly2(_) => UsageFromTask::Readonly(task),
            Self::Writable2(_) => UsageFromTask::Writable(task),
        }
    }

    fn usage_queue(&self) -> &UsageQueue {
        match self {
            Self::Readonly(u) | Self::Writable(u) => &u,
            Self::Readonly2(u) | Self::Writable2(u) => &u,
        }
    }

    fn with_usage_queue_mut<R>(
        &self,
        usage_queue_token: &mut UsageQueueToken,
        f: impl FnOnce(&mut UsageQueueInner) -> R,
    ) -> R {
        self.usage_queue().0.with_borrow_mut(usage_queue_token, f)
    }
}

use std::cmp::Reverse;

/// Status about how the [`UsageQueue`] is used currently.
#[derive(Debug)]
enum Usage {
    Readonly(ShortCounter),
    Writable(Task),
}

/// Status about how a task is requesting to use a particular [`UsageQueue`].
#[derive(Clone, Copy, Debug, PartialEq)]
enum RequestedUsage {
    Readonly,
    Writable,
}

//use std::collections::binary_heap::PeekMut;
use dary_heap::PeekMut;

/// Internal scheduling data about a particular address.
///
/// Specifically, it holds the current [`Usage`] (or no usage with [`Usage::Unused`]) and which
/// [`Task`]s are blocked to be executed after the current task is notified to be finished via
/// [`::deschedule_task`](`SchedulingStateMachine::deschedule_task`)
#[derive(Debug)]
struct UsageQueueInner {
    current_usage: Option<Usage>,
    current_readonly_tasks:  dary_heap::OctonaryHeap<Reverse<Task>>,
    blocked_usages_from_tasks:  dary_heap::OctonaryHeap<Compact<UsageFromTask>>,
}

use enum_ptr::EnumPtr;
use enum_ptr::Compact;

#[repr(C, usize)]
#[derive(Debug, EnumPtr)]
enum UsageFromTask {
    Readonly(Task),
    Writable(Task),
}
const_assert_eq!(mem::size_of::<UsageFromTask>(), 16);
const_assert_eq!(mem::size_of::<Compact<UsageFromTask>>(), 8);

impl UsageFromTask {
    fn index(&self) -> Index {
        match self {
            Self::Readonly(t) => t.index(),
            Self::Writable(t) => t.index(),
        }
    }

    fn usage(&self) -> RequestedUsage {
        match self {
            Self::Readonly(_t) => RequestedUsage::Readonly,
            Self::Writable(_t) => RequestedUsage::Writable,
        }
    }

    fn task(&self) -> &Task {
        match self {
            Self::Readonly(t) | Self::Writable(t) => t,
        }
    }
}

impl From<(RequestedUsage, Task)> for UsageFromTask {
    fn from((usage, task): (RequestedUsage, Task)) -> Self {
        match usage {
            RequestedUsage::Readonly => Self::Readonly(task),
            RequestedUsage::Writable => Self::Writable(task),
        }
    }
}

impl Ord for Task {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.index().cmp(&self.index())
        //self.index.cmp(&other.index)
    }
}

impl PartialOrd for Task {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for Task {}
impl PartialEq<Task> for Task {
    fn eq(&self, other: &Self) -> bool {
        self.index() == other.index()
    }
}

impl Ord for UsageFromTask {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other.index().cmp(&self.index())
        //self.index().cmp(&other.index())
    }
}

impl PartialOrd for UsageFromTask {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for UsageFromTask {}
impl PartialEq<UsageFromTask> for UsageFromTask {
    fn eq(&self, other: &Self) -> bool {
        self.index() == other.index()
    }
}

impl Default for UsageQueueInner {
    fn default() -> Self {
        Self {
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
            current_readonly_tasks:  dary_heap::OctonaryHeap::with_capacity(128),
            blocked_usages_from_tasks:  dary_heap::OctonaryHeap::with_capacity(128),
        }
    }
}

impl UsageQueueInner {
    fn try_lock(&mut self, requested_usage: RequestedUsage, task: &Task) -> LockResult {
        match &mut self.current_usage {
            None => {
                match requested_usage {
                    RequestedUsage::Readonly => {
                        self.current_usage = Some(Usage::Readonly(ShortCounter::one()));
                        self.current_readonly_tasks.push(Reverse(task.clone()));
                    },
                    RequestedUsage::Writable => {
                        self.current_usage = Some(Usage::Writable(task.clone()));
                    },
                }
                Ok(())
            }
            Some(Usage::Readonly(count)) => match requested_usage {
                RequestedUsage::Readonly => {
                    //dbg!(&self.current_readonly_tasks.keys());
                    self.current_readonly_tasks.push(Reverse(task.clone()));
                    count.increment_self();
                    Ok(())
                }
                RequestedUsage::Writable => Err(()),
            },
            Some(Usage::Writable(_current_task)) => Err(()),
        }
    }

    #[must_use]
    fn unlock(
        &mut self,
        unlocked_task_context: &LockContext,
        unlocked_task_index: Index,
        token: &mut BlockedUsageCountToken,
    ) -> Option<UsageFromTask> {
        let mut is_unused_now = false;
        match &mut self.current_usage {
            Some(Usage::Readonly(count)) => match unlocked_task_context {
                LockContext::Readonly(_) | LockContext::Readonly2(_) => {
                    count.decrement_self();
                    // todo test this for unbounded growth of inifnite readable only locks....
                    //dbg!(self.current_readonly_tasks.len());
                    while let Some(peeked_task) = self.current_readonly_tasks.peek_mut() {
                        if peeked_task.0.is_unlocked(token) {
                            PeekMut::pop(peeked_task);
                        } else {
                            break;
                        }
                    }
                    if count.is_zero() {
                        is_unused_now = true;
                    }
                    //dbg!(is_unused_now);
                }
                LockContext::Writable(_) | LockContext::Writable2(_) => unreachable!(),
            },
            Some(Usage::Writable(blocking_task)) => {
                assert_eq!((unlocked_task_index, unlocked_task_context.requested_usage2()), (blocking_task.index(), RequestedUsage::Writable));
                is_unused_now = true;
            },
            None => unreachable!(),
        }

        if is_unused_now {
            self.current_usage = None;
            self.blocked_usages_from_tasks
                .pop()
                .map(|uft| uft.into())
        } else {
            None
        }
    }

    fn insert_blocked_usage_from_task(&mut self, uft: UsageFromTask) {
        self
            .blocked_usages_from_tasks
            .push(uft.into());
    }

    fn first_blocked_task_index(&self) -> Option<Index> {
        self.blocked_usages_from_tasks.peek().map(|uft| uft.map_ref(|u| u.index()))
    }

    #[must_use]
    fn pop_buffered_readonly_usage_from_task(&mut self) -> Option<UsageFromTask> {
        if matches!(
            self.blocked_usages_from_tasks
                .peek()
                .map(|uft| uft.map_ref(|u| u.usage())),
            Some(RequestedUsage::Readonly)
        ) {
            self.blocked_usages_from_tasks
                .pop()
                .map(|uft| uft.into())
        } else {
            None
        }
    }

    fn has_no_blocked_usage(&self) -> bool {
        self.blocked_usages_from_tasks.is_empty()
    }
}

//const_assert_eq!(mem::size_of::<TokenCell<UsageQueueInner>>(), 40);

/// Scheduler's internal data for each address ([`Pubkey`](`solana_sdk::pubkey::Pubkey`)). Very
/// opaque wrapper type; no methods just with [`::clone()`](Clone::clone) and
/// [`::default()`](Default::default).
#[derive(Debug, Clone, Default)]
pub struct UsageQueue(Arc<TokenCell<UsageQueueInner>>);
const_assert_eq!(mem::size_of::<UsageQueue>(), 8);

unsafe impl enum_ptr::Aligned for UsageQueue {
    const ALIGNMENT: usize = std::mem::align_of::<TokenCell<UsageQueueInner>>();
}

/// A high-level `struct`, managing the overall scheduling of [tasks](Task), to be used by
/// `solana-unified-scheduler-pool`.
#[derive(Debug)]
pub struct SchedulingStateMachine {
    buffered_task_queue:  dary_heap::OctonaryHeap<Task>,
    alive_tasks: BTreeSet<Task>,
    alive_task_count: ShortCounter,
    executing_task_count: ShortCounter,
    max_executing_task_count: u32,
    executed_task_total: ShortCounter,
    buffered_task_total: ShortCounter,
    blocked_task_count: ShortCounter,
    reblocked_lock_total: ShortCounter,
    task_total: ShortCounter,
    count_token: BlockedUsageCountToken,
    usage_queue_token: UsageQueueToken,
    scheduling_mode: SchedulingMode,
}

#[cfg(test)]
impl Drop for SchedulingStateMachine {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.reinitialize_for_test();
        }
    }
}
//const_assert_eq!(mem::size_of::<SchedulingStateMachine>(), 56);

impl SchedulingStateMachine {
    pub fn mode(&self) -> SchedulingMode {
        self.scheduling_mode
    }

    pub fn has_no_alive_task(&self) -> bool {
        self.alive_task_count.is_zero()
    }

    pub fn has_buffered_task(&mut self) -> bool {
        while let Some(task) = self.buffered_task_queue.peek_mut() {
            let status = task.status(&mut self.count_token);
            if task.has_blocked_usage(&mut self.count_token) {
                PeekMut::pop(task);
                continue;
            } else if status == TaskStatus::Executed || status == TaskStatus::Unlocked {
                PeekMut::pop(task);
                continue;
            } else {
                return true;
            }
        }
        match self.mode() {
            SchedulingMode::BlockVerification => {
                false
            },
            SchedulingMode::BlockProduction => {
                for task in self.alive_tasks.range(..) {
                    for context in task.lock_contexts() {
                        context.map_ref(|context| {
                        });
                    }
                }
                panic!();
            },
        }
    }

    pub fn has_runnable_task(&mut self) -> bool {
        self.is_task_runnable() && self.has_buffered_task()
    }

    pub fn has_no_executing_task(&self) -> bool {
        self.executing_task_count.current() == 0
    }

    pub fn is_task_runnable(&self) -> bool {
        self.executing_task_count.current() < self.max_executing_task_count
    }

    pub fn buffered_task_queue_count(&self) -> usize {
        self.buffered_task_queue.len()
    }

    pub fn alive_task_count(&self) -> u32 {
        self.alive_task_count.current()
    }

    pub fn executed_task_total(&self) -> u32 {
        self.executed_task_total.current()
    }

    pub fn buffered_task_total(&self) -> u32 {
        self.buffered_task_total.current()
    }

    pub fn blocked_task_count(&self) -> u32 {
        self.blocked_task_count.current()
    }

    pub fn reblocked_lock_total(&self) -> u32 {
        self.reblocked_lock_total.current()
    }
    pub fn task_total(&self) -> u32 {
        self.task_total.current()
    }

    /// Schedules given `task`, returning it if successful.
    ///
    /// Returns `Some(task)` if it's immediately scheduled. Otherwise, returns `None`,
    /// indicating the scheduled task is blocked currently.
    ///
    /// Note that this function takes ownership of the task to allow for future optimizations.
    #[must_use]
    pub fn schedule_task(&mut self, task: Task) -> Option<Task> {
        self.task_total.increment_self();
        self.alive_task_count.increment_self();
        self.alive_tasks.insert(task.clone()).then_some(()).or_else(|| panic!());
        self.try_lock_usage_queues(task).and_then(|task| {
            if self.is_task_runnable() {
                self.executing_task_count.increment_self();
                task.mark_as_executed(&mut self.count_token);
                Some(task)
            } else {
                self.buffered_task_total.increment_self();
                self.buffered_task_queue.push(task);
                None
            }
        })
    }

    pub fn rebuffer_executing_task(&mut self, task: Task) {
        self.executing_task_count.decrement_self();
        self.buffered_task_total.increment_self();
        task.mark_as_buffered(&mut self.count_token);
        self.buffered_task_queue.push(task);
    }

    #[must_use]
    pub fn schedule_next_buffered_task(&mut self) -> Option<Task> {
        while let Some(task) = self.buffered_task_queue.pop() {
            if task.has_blocked_usage(&mut self.count_token) {
                continue;
            } else {
                self.executing_task_count.increment_self();
                task.mark_as_executed(&mut self.count_token);
                return Some(task);
            }
        }
        None
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
        task.mark_as_unlocked(&mut self.count_token);
        self.executing_task_count.decrement_self();
        self.alive_task_count.decrement_self();
        self.alive_tasks.remove(task).then_some(()).or_else(|| panic!());
        self.executed_task_total.increment_self();
        self.unlock_usage_queues(task);
    }

    fn try_reblock_task(blocking_task: &Task, blocked_task_count: &mut ShortCounter, token: &mut BlockedUsageCountToken) -> bool {
        if blocking_task.has_blocked_usage(token) {
            true
        } else if blocking_task.is_buffered(token) {
            blocked_task_count.increment_self();
            true
        } else {
            false
        }
    }

    #[must_use]
    fn try_lock_usage_queues(&mut self, new_task: Task) -> Option<Task> {
        let mut blocked_usage_count = ShortCounter::zero();

        for context in new_task.lock_contexts() {
            context.map_ref(|context| {
            context.with_usage_queue_mut(&mut self.usage_queue_token, |usage_queue| {
                let lock_result = (match usage_queue.current_usage.as_mut() {
                    Some(mut current_usage) => {
                        match (&mut current_usage, context.requested_usage2()) {
                            (Usage::Writable(blocking_task), RequestedUsage::Writable) => {
                                if new_task.index() < blocking_task.index() && Self::try_reblock_task(blocking_task, &mut self.blocked_task_count, &mut self.count_token) {
                                    let old_usage = std::mem::replace(current_usage, Usage::Writable(new_task.clone()));
                                    let Usage::Writable(reblocked_task) = old_usage else { panic!() };
                                    reblocked_task.increment_blocked_usage_count(&mut self.count_token);
                                    usage_queue.insert_blocked_usage_from_task(
                                        UsageFromTask::Writable(reblocked_task),
                                    );
                                    self.reblocked_lock_total.increment_self();
                                    Some(Ok(()))
                                } else {
                                    None
                                }
                            }
                            (Usage::Writable(blocking_task), RequestedUsage::Readonly) => {
                                if new_task.index() < blocking_task.index() && Self::try_reblock_task(blocking_task, &mut self.blocked_task_count, &mut self.count_token) {
                                    let old_usage = std::mem::replace(current_usage, Usage::Readonly(ShortCounter::one()));
                                    let Usage::Writable(reblocked_task) = old_usage else { panic!() };
                                    reblocked_task.increment_blocked_usage_count(&mut self.count_token);
                                    usage_queue.current_readonly_tasks.push(Reverse(new_task.clone()));
                                    usage_queue.insert_blocked_usage_from_task(
                                        UsageFromTask::Writable(reblocked_task),
                                    );
                                    self.reblocked_lock_total.increment_self();
                                    Some(Ok(()))
                                } else {
                                    None
                                }
                            }
                            (Usage::Readonly(_count), RequestedUsage::Readonly) => {
                                let first_blocked_task_index = usage_queue.first_blocked_task_index();
                                if let Some(first_blocked_task_index) = first_blocked_task_index {
                                    if new_task.index() < first_blocked_task_index {
                                        usage_queue
                                            .try_lock(context.requested_usage2(), &new_task)
                                            .unwrap();
                                        Some(Ok(()))
                                        // even the following passes the unit tests... think about this
                                        /*
                                        if usage_queue.has_no_blocked_usage() {
                                            usage_queue.try_lock(context.requested_usage, &new_task)
                                        } else {
                                            Err(())
                                        }
                                        */
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            }
                            (Usage::Readonly(count), RequestedUsage::Writable) => {
                                let mut reblocked_tasks = vec![];
                                while let Some(blocking_task) = usage_queue.current_readonly_tasks.peek_mut() {
                                    let index = blocking_task.0.0.index();
                                    if new_task.index() < index || blocking_task.0.is_unlocked(&mut self.count_token) {
                                        let blocking_task = PeekMut::pop(blocking_task).0;

                                        if Self::try_reblock_task(&blocking_task, &mut self.blocked_task_count, &mut self.count_token) {
                                            count.decrement_self();
                                            reblocked_tasks.push(blocking_task);
                                        }
                                    } else {
                                        break;
                                    }
                                }
                                if !reblocked_tasks.is_empty() {
                                    let lock_result = if count.is_zero() {
                                        *current_usage = Usage::Writable(new_task.clone());
                                        Ok(())
                                    } else {
                                        Err(())
                                    };
                                    for reblocked_task in reblocked_tasks {
                                        reblocked_task.increment_blocked_usage_count(&mut self.count_token);
                                        usage_queue.insert_blocked_usage_from_task(
                                            UsageFromTask::Readonly(reblocked_task),
                                        );
                                        self.reblocked_lock_total.increment_self();
                                    }
                                    Some(lock_result)
                                } else {
                                    None
                                }
                            }
                        }
                    }
                    _ => {
                        None
                    }
                }).unwrap_or_else(|| {
                    if usage_queue.has_no_blocked_usage() {
                        usage_queue.try_lock(context.requested_usage2(), &new_task)
                    } else {
                        Err(())
                    }
                });

                if let Err(()) = lock_result {
                    blocked_usage_count.increment_self();
                    let usage_from_task = context.usage_from_task(new_task.clone());
                    usage_queue.insert_blocked_usage_from_task(usage_from_task.into());
                }
            });
            });
        }

        // no blocked usage count means success
        if blocked_usage_count.is_zero() {
            Some(new_task)
        } else {
            self.blocked_task_count.increment_self();
            new_task.set_blocked_usage_count(&mut self.count_token, blocked_usage_count);
            None
        }
    }

    fn unlock_usage_queues(&mut self, task: &Task) {
        for context in task.lock_contexts() {
            context.map_ref(|context| {
            context.with_usage_queue_mut(&mut self.usage_queue_token, |usage_queue| {
                let mut buffered_task_from_queue =
                    usage_queue.unlock(context, task.index(), &mut self.count_token);

                while let Some(buffered_task_from_queue2) = buffered_task_from_queue {
                    // When `try_unblock()` returns `None` as a failure of unblocking this time,
                    // this means the task is still blocked by other active task's usages. So,
                    // don't push task into buffered_task_queue yet. It can be assumed that every
                    // task will eventually succeed to be unblocked, and enter in this condition
                    // clause as long as `SchedulingStateMachine` is used correctly.
                    if let Some(task) = buffered_task_from_queue2.task()
                        .clone()
                        .try_unblock(&mut self.count_token)
                    {
                        self.blocked_task_count.decrement_self();
                        self.buffered_task_total.increment_self();
                        self.buffered_task_queue.push(task);
                    }

                    match usage_queue.try_lock(
                        buffered_task_from_queue2.usage(),
                        &buffered_task_from_queue2.task(), /* was `task` and had bug.. write test...*/
                    ) {
                        LockResult::Ok(()) => {
                            // Try to further schedule blocked task for parallelism in the case of
                            // readonly usages
                            buffered_task_from_queue =
                                if matches!(buffered_task_from_queue2.usage(), RequestedUsage::Readonly) {
                                    usage_queue.pop_buffered_readonly_usage_from_task()
                                } else {
                                    None
                                };
                        }
                        LockResult::Err(()) => panic!("should never fail in this context"),
                    }
                }
            });
            });
        }
    }

    /// Creates a new task with [`SanitizedTransaction`] with all of its corresponding
    /// [`UsageQueue`]s preloaded.
    ///
    /// Closure (`usage_queue_loader`) is used to delegate the (possibly multi-threaded)
    /// implementation of [`UsageQueue`] look-up by [`pubkey`](Pubkey) to callers. It's the
    /// caller's responsibility to ensure the same instance is returned from the closure, given a
    /// particular pubkey.
    ///
    /// Closure is used here to delegate the responsibility of primary ownership of `UsageQueue`
    /// (and caching/pruning if any) to the caller. `SchedulingStateMachine` guarantees that all of
    /// shared owndership of `UsageQueue`s are released and UsageQueue state is identical to just
    /// after created, if `has_no_alive_task()` is `true`. Also note that this is desired for
    /// separation of concern.
    pub fn create_task(
        transaction: SanitizedTransaction,
        index: Index,
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
            .map(|(index, address)| {
                LockContext::new(
                    usage_queue_loader(*address),
                    if transaction.message().is_writable(index) {
                        RequestedUsage::Writable
                    } else {
                        RequestedUsage::Readonly
                    },
                ).into()
            })
            .collect();

        Task::new(TaskInner {
            packed_task_inner: PackedTaskInner { lock_context_and_transaction: Box::new((lock_contexts, Box::new(transaction))), index},
            blocked_usage_count: TokenCell::new(CounterWithStatus::default()),
        })
    }

    pub fn reset_task_total(&mut self) {
        self.task_total.reset_to_zero();
    }

    pub fn reset_executed_task_total(&mut self) {
        self.executed_task_total.reset_to_zero();
    }

    /// Rewind the inactive state machine to be initialized
    ///
    /// This isn't called _reset_ to indicate this isn't safe to call this at any given moment.
    /// This panics if the state machine hasn't properly been finished (i.e.  there should be no
    /// active task) to uphold invariants of [`UsageQueue`]s.
    ///
    /// This method is intended to reuse SchedulingStateMachine instance (to avoid its `unsafe`
    /// [constructor](SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling)
    /// as much as possible) and its (possibly cached) associated [`UsageQueue`]s for processing
    /// other slots.
    pub fn reinitialize(&mut self, mode: SchedulingMode) {
        assert!(self.has_no_alive_task());
        assert_eq!(self.executing_task_count.current(), 0);
        assert_eq!(self.buffered_task_queue.len(), 0);
        assert_eq!(self.alive_tasks.len(), 0);
        assert_eq!(self.blocked_task_count(), 0);

        self.reset_task_total();
        self.reset_executed_task_total();
        // nice trick to ensure all fields are handled here if new one is added.
        let Self {
            buffered_task_queue: _,
            alive_tasks: _,
            alive_task_count,
            executing_task_count,
            max_executing_task_count: _,
            executed_task_total: _,
            buffered_task_total,
            blocked_task_count: _,
            reblocked_lock_total,
            task_total: _,
            count_token: _,
            usage_queue_token: _,
            scheduling_mode,
            // don't add ".." here
        } = self;
        alive_task_count.reset_to_zero();
        executing_task_count.reset_to_zero();
        buffered_task_total.reset_to_zero();
        reblocked_lock_total.reset_to_zero();
        *scheduling_mode = mode;
    }

    pub fn reinitialize_for_test(&mut self) {
        self.reinitialize(SchedulingMode::BlockProduction);
    }

    /// Creates a new instance of [`SchedulingStateMachine`] with its `unsafe` fields created as
    /// well, thus carrying over `unsafe`.
    ///
    /// # Safety
    /// Call this exactly once for each thread. See [`TokenCell`] for details.
    #[must_use]
    pub unsafe fn exclusively_initialize_current_thread_for_scheduling(
        scheduling_mode: SchedulingMode,
    ) -> Self {
        Self {
            // It's very unlikely this is desired to be configurable, like
            // `UsageQueueInner::blocked_usages_from_tasks`'s cap.
            buffered_task_queue:  dary_heap::OctonaryHeap::with_capacity(1024), // BTreeMap::new(), //VecDeque::with_capacity(1024),
            alive_tasks: BTreeSet::default(),
            alive_task_count: ShortCounter::zero(),
            executing_task_count: ShortCounter::zero(),
            max_executing_task_count: 200,
            executed_task_total: ShortCounter::zero(),
            buffered_task_total: ShortCounter::zero(),
            blocked_task_count: ShortCounter::zero(),
            reblocked_lock_total: ShortCounter::zero(),
            task_total: ShortCounter::zero(),
            count_token: unsafe { BlockedUsageCountToken::assume_exclusive_mutating_thread() },
            usage_queue_token: unsafe { UsageQueueToken::assume_exclusive_mutating_thread() },
            scheduling_mode,
        }
    }

    pub unsafe fn exclusively_initialize_current_thread_for_scheduling_for_test() -> Self {
        Self::exclusively_initialize_current_thread_for_scheduling(SchedulingMode::BlockVerification)
    }

    pub unsafe fn exclusively_initialize_current_thread_for_scheduling_for_test2() -> Self {
        Self::exclusively_initialize_current_thread_for_scheduling(SchedulingMode::BlockProduction)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_sdk::{
            instruction::{AccountMeta, Instruction},
            message::Message,
            pubkey::Pubkey,
            signature::Signer,
            signer::keypair::Keypair,
            transaction::{SanitizedTransaction, Transaction},
        },
        std::{cell::RefCell, collections::HashMap, rc::Rc},
    };

    fn simplest_transaction() -> SanitizedTransaction {
        let payer = Keypair::new();
        let message = Message::new(&[], Some(&payer.pubkey()));
        let unsigned = Transaction::new_unsigned(message);
        SanitizedTransaction::from_transaction_for_tests(unsigned)
    }

    fn transaction_with_readonly_address(address: Pubkey) -> SanitizedTransaction {
        let instruction = Instruction {
            program_id: Pubkey::default(),
            accounts: vec![AccountMeta::new_readonly(address, false)],
            data: vec![],
        };
        let message = Message::new(&[instruction], Some(&Pubkey::new_unique()));
        let unsigned = Transaction::new_unsigned(message);
        SanitizedTransaction::from_transaction_for_tests(unsigned)
    }

    fn transaction_with_writable_address(address: Pubkey) -> SanitizedTransaction {
        let instruction = Instruction {
            program_id: Pubkey::default(),
            accounts: vec![AccountMeta::new(address, false)],
            data: vec![],
        };
        let message = Message::new(&[instruction], Some(&Pubkey::new_unique()));
        let unsigned = Transaction::new_unsigned(message);
        SanitizedTransaction::from_transaction_for_tests(unsigned)
    }

    fn transaction_with_writable_address2(
        address: Pubkey,
        address2: Pubkey,
    ) -> SanitizedTransaction {
        let instruction = Instruction {
            program_id: Pubkey::default(),
            accounts: vec![
                AccountMeta::new(address, false),
                AccountMeta::new(address2, false),
            ],
            data: vec![],
        };
        let message = Message::new(&[instruction], Some(&Pubkey::new_unique()));
        let unsigned = Transaction::new_unsigned(message);
        SanitizedTransaction::from_transaction_for_tests(unsigned)
    }

    fn transaction_with_writable_read2(address: Pubkey, address2: Pubkey) -> SanitizedTransaction {
        let instruction = Instruction {
            program_id: Pubkey::default(),
            accounts: vec![
                AccountMeta::new(address, false),
                AccountMeta::new_readonly(address2, false),
            ],
            data: vec![],
        };
        let message = Message::new(&[instruction], Some(&Pubkey::new_unique()));
        let unsigned = Transaction::new_unsigned(message);
        SanitizedTransaction::from_transaction_for_tests(unsigned)
    }

    fn create_address_loader(
        usage_queues: Option<Rc<RefCell<HashMap<Pubkey, UsageQueue>>>>,
    ) -> impl FnMut(Pubkey) -> UsageQueue {
        let usage_queues = usage_queues.unwrap_or_default();
        move |address| {
            usage_queues
                .borrow_mut()
                .entry(address)
                .or_default()
                .clone()
        }
    }

    #[test]
    fn test_scheduling_state_machine_creation() {
        let state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_eq!(state_machine.alive_task_count(), 0);
        assert_eq!(state_machine.task_total(), 0);
        assert!(state_machine.has_no_alive_task());
    }

    #[test]
    fn test_scheduling_state_machine_good_reinitialization() {
        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        state_machine.task_total.increment_self();
        assert_eq!(state_machine.task_total(), 1);
        state_machine.reinitialize_for_test();
        assert_eq!(state_machine.task_total(), 0);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    #[should_panic(expected = "assertion failed: self.has_no_alive_task()")]
    fn test_scheduling_state_machine_bad_reinitialization() {
        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        let address_loader = &mut create_address_loader(None);
        let task = SchedulingStateMachine::create_task(simplest_transaction(), 3, address_loader);
        state_machine.schedule_task(task).unwrap();
        state_machine.reinitialize_for_test();
    }

    #[test]
    fn test_create_task() {
        let sanitized = simplest_transaction();
        let task = SchedulingStateMachine::create_task(sanitized.clone(), 3, &mut |_| {
            UsageQueue::default()
        });
        assert_eq!(task.task_index(), 3);
        assert_eq!(task.transaction(), &sanitized);
    }

    #[test]
    fn test_non_conflicting_task_related_counts() {
        let sanitized = simplest_transaction();
        let address_loader = &mut create_address_loader(None);
        let task = SchedulingStateMachine::create_task(sanitized.clone(), 3, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        let task = state_machine.schedule_task(task).unwrap();
        assert_eq!(state_machine.alive_task_count(), 1);
        assert_eq!(state_machine.task_total(), 1);
        state_machine.deschedule_task(&task);
        assert_eq!(state_machine.alive_task_count(), 0);
        assert_eq!(state_machine.task_total(), 1);
        assert!(state_machine.has_no_alive_task());
    }

    #[test]
    fn test_conflicting_task_related_counts() {
        let sanitized = simplest_transaction();
        let address_loader = &mut create_address_loader(None);
        let task1 = SchedulingStateMachine::create_task(sanitized.clone(), 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized.clone(), 102, address_loader);
        let task3 = SchedulingStateMachine::create_task(sanitized.clone(), 103, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_index()),
            Some(101)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);

        state_machine.deschedule_task(&task1);
        assert!(state_machine.has_buffered_task());
        assert_eq!(state_machine.buffered_task_queue_count(), 1);

        assert_eq!(state_machine.buffered_task_total(), 1);
        assert_eq!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(102)
        );
        // buffered_task_total() should be incremented
        assert_eq!(state_machine.buffered_task_total(), 1);

        // there's no blocked task anymore; calling schedule_next_buffered_task should be noop and
        // shouldn't increment the buffered_task_total().
        assert!(!state_machine.has_buffered_task());
        assert_matches!(state_machine.schedule_next_buffered_task(), None);
        assert_eq!(state_machine.buffered_task_total(), 1);

        assert_eq!(state_machine.buffered_task_queue_count(), 0);
        state_machine.deschedule_task(&task2);

        assert_matches!(
            state_machine
                .schedule_task(task3.clone())
                .map(|task| task.task_index()),
            Some(103)
        );
        state_machine.deschedule_task(&task3);
        assert!(state_machine.has_no_alive_task());
    }

    #[test]
    fn test_existing_blocking_task_then_newly_scheduled_task() {
        let sanitized = simplest_transaction();
        let address_loader = &mut create_address_loader(None);
        let task1 = SchedulingStateMachine::create_task(sanitized.clone(), 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized.clone(), 102, address_loader);
        let task3 = SchedulingStateMachine::create_task(sanitized.clone(), 103, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_index()),
            Some(101)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);

        assert_eq!(state_machine.buffered_task_queue_count(), 0);
        state_machine.deschedule_task(&task1);
        assert_eq!(state_machine.buffered_task_queue_count(), 1);

        // new task is arriving after task1 is already descheduled and task2 got unblocked
        assert_matches!(state_machine.schedule_task(task3.clone()), None);

        assert_eq!(state_machine.buffered_task_total(), 1);
        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(102)
        );
        // buffered_task_total() should be incremented
        assert_eq!(state_machine.buffered_task_total(), 1);

        state_machine.deschedule_task(&task2);

        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(103)
        );
        assert_eq!(state_machine.buffered_task_total(), 2);

        state_machine.deschedule_task(&task3);
        assert!(state_machine.has_no_alive_task());
    }

    #[test]
    fn test_multiple_readonly_task_and_counts() {
        let conflicting_address = Pubkey::new_unique();
        let sanitized1 = transaction_with_readonly_address(conflicting_address);
        let sanitized2 = transaction_with_readonly_address(conflicting_address);
        let address_loader = &mut create_address_loader(None);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 102, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        // both of read-only tasks should be immediately runnable
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_index()),
            Some(101)
        );
        assert_matches!(
            state_machine
                .schedule_task(task2.clone())
                .map(|t| t.task_index()),
            Some(102)
        );

        assert_eq!(state_machine.alive_task_count(), 2);
        assert_eq!(state_machine.executed_task_total(), 0);
        assert_eq!(state_machine.buffered_task_queue_count(), 0);
        state_machine.deschedule_task(&task1);
        assert_eq!(state_machine.alive_task_count(), 1);
        assert_eq!(state_machine.executed_task_total(), 1);
        assert_eq!(state_machine.buffered_task_queue_count(), 0);
        state_machine.deschedule_task(&task2);
        assert_eq!(state_machine.alive_task_count(), 0);
        assert_eq!(state_machine.executed_task_total(), 2);
        assert!(state_machine.has_no_alive_task());
    }

    #[test]
    fn test_all_blocking_readable_tasks_block_writable_task() {
        let conflicting_address = Pubkey::new_unique();
        let sanitized1 = transaction_with_readonly_address(conflicting_address);
        let sanitized2 = transaction_with_readonly_address(conflicting_address);
        let sanitized3 = transaction_with_writable_address(conflicting_address);
        let address_loader = &mut create_address_loader(None);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 102, address_loader);
        let task3 = SchedulingStateMachine::create_task(sanitized3, 103, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_index()),
            Some(101)
        );
        assert_matches!(
            state_machine
                .schedule_task(task2.clone())
                .map(|t| t.task_index()),
            Some(102)
        );
        assert_matches!(state_machine.schedule_task(task3.clone()), None);

        assert_eq!(state_machine.alive_task_count(), 3);
        assert_eq!(state_machine.executed_task_total(), 0);
        assert_eq!(state_machine.buffered_task_queue_count(), 0);
        state_machine.deschedule_task(&task1);
        assert_eq!(state_machine.alive_task_count(), 2);
        assert_eq!(state_machine.executed_task_total(), 1);
        assert_eq!(state_machine.buffered_task_queue_count(), 0);
        assert_matches!(state_machine.schedule_next_buffered_task(), None);
        state_machine.deschedule_task(&task2);
        assert_eq!(state_machine.alive_task_count(), 1);
        assert_eq!(state_machine.executed_task_total(), 2);
        assert_eq!(state_machine.buffered_task_queue_count(), 1);
        // task3 is finally unblocked after all of readable tasks (task1 and task2) is finished.
        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(103)
        );
        state_machine.deschedule_task(&task3);
        assert!(state_machine.has_no_alive_task());
    }

    #[test]
    fn test_readonly_then_writable_then_readonly_linearized() {
        let conflicting_address = Pubkey::new_unique();
        let sanitized1 = transaction_with_readonly_address(conflicting_address);
        let sanitized2 = transaction_with_writable_address(conflicting_address);
        let sanitized3 = transaction_with_readonly_address(conflicting_address);
        let address_loader = &mut create_address_loader(None);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 102, address_loader);
        let task3 = SchedulingStateMachine::create_task(sanitized3, 103, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_index()),
            Some(101)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);
        assert_matches!(state_machine.schedule_task(task3.clone()), None);

        assert_matches!(state_machine.schedule_next_buffered_task(), None);
        state_machine.deschedule_task(&task1);
        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(102)
        );
        assert_matches!(state_machine.schedule_next_buffered_task(), None);
        state_machine.deschedule_task(&task2);
        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(103)
        );
        assert_matches!(state_machine.schedule_next_buffered_task(), None);
        state_machine.deschedule_task(&task3);
        assert!(state_machine.has_no_alive_task());
    }

    #[test]
    fn test_readonly_then_writable() {
        let conflicting_address = Pubkey::new_unique();
        let sanitized1 = transaction_with_readonly_address(conflicting_address);
        let sanitized2 = transaction_with_writable_address(conflicting_address);
        let address_loader = &mut create_address_loader(None);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 102, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_index()),
            Some(101)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);

        // descheduling read-locking task1 should equate to unblocking write-locking task2
        state_machine.deschedule_task(&task1);
        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(102)
        );
        state_machine.deschedule_task(&task2);
        assert!(state_machine.has_no_alive_task());
    }

    #[test]
    fn test_blocked_tasks_writable_2_readonly_then_writable() {
        let conflicting_address = Pubkey::new_unique();
        let sanitized1 = transaction_with_writable_address(conflicting_address);
        let sanitized2 = transaction_with_readonly_address(conflicting_address);
        let sanitized3 = transaction_with_readonly_address(conflicting_address);
        let sanitized4 = transaction_with_writable_address(conflicting_address);
        let address_loader = &mut create_address_loader(None);
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
                .map(|t| t.task_index()),
            Some(101)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);
        assert_matches!(state_machine.schedule_task(task3.clone()), None);
        assert_matches!(state_machine.schedule_task(task4.clone()), None);

        state_machine.deschedule_task(&task1);
        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(102)
        );
        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(103)
        );
        // the above deschedule_task(task1) call should only unblock task2 and task3 because these
        // are read-locking. And shouldn't unblock task4 because it's write-locking
        assert_matches!(state_machine.schedule_next_buffered_task(), None);

        state_machine.deschedule_task(&task2);
        // still task4 is blocked...
        assert_matches!(state_machine.schedule_next_buffered_task(), None);

        state_machine.deschedule_task(&task3);
        // finally task4 should be unblocked
        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(104)
        );
        state_machine.deschedule_task(&task4);
        assert!(state_machine.has_no_alive_task());
    }

    #[test]
    fn test_gradual_locking() {
        let conflicting_address = Pubkey::new_unique();
        let sanitized1 = transaction_with_writable_address(conflicting_address);
        let sanitized2 = transaction_with_writable_address(conflicting_address);
        let usage_queues = Rc::new(RefCell::new(HashMap::new()));
        let address_loader = &mut create_address_loader(Some(usage_queues.clone()));
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 102, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_index()),
            Some(101)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);
        let usage_queues = usage_queues.borrow_mut();
        let usage_queue = usage_queues.get(&conflicting_address).unwrap();
        usage_queue
            .0
            .with_borrow_mut(&mut state_machine.usage_queue_token, |usage_queue| {
                assert_matches!(usage_queue.current_usage, Some(Usage::Writable(_)));
            });
        // task2's fee payer should have been locked already even if task2 is blocked still via the
        // above the schedule_task(task2) call
        let fee_payer = task2.transaction().message().fee_payer();
        let usage_queue = usage_queues.get(fee_payer).unwrap();
        usage_queue
            .0
            .with_borrow_mut(&mut state_machine.usage_queue_token, |usage_queue| {
                assert_matches!(usage_queue.current_usage, Some(Usage::Writable(_)));
            });
        state_machine.deschedule_task(&task1);
        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(102)
        );
        state_machine.deschedule_task(&task2);
        assert!(state_machine.has_no_alive_task());
    }

    #[test]
    fn test_higher_priority_locking_write_read() {
        let conflicting_address1 = Pubkey::new_unique();
        let conflicting_address2 = Pubkey::new_unique();
        let sanitized1 =
            transaction_with_writable_address2(conflicting_address1, conflicting_address2);
        let sanitized2 =
            transaction_with_writable_read2(conflicting_address1, conflicting_address2);
        let sanitized0_1 = transaction_with_writable_address(conflicting_address1);
        //let sanitized0_2 = transaction_with_writable_address(
        let usage_queues = Rc::new(RefCell::new(HashMap::new()));
        let address_loader = &mut create_address_loader(Some(usage_queues.clone()));
        let task0_1 = SchedulingStateMachine::create_task(sanitized0_1, 50, address_loader);
        //let task0_2 = SchedulingStateMachine::create_task(sanitized0_2, 51, address_loader);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 99, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };

        assert_matches!(
            state_machine
                .schedule_task(task0_1.clone())
                .map(|t| t.task_index()),
            Some(50)
        );
        // now
        // addr1: locked by task_0_1, queue: []
        // addr2: unlocked, queue: []

        assert_matches!(state_machine.schedule_task(task1.clone()), None);
        // now
        // addr1: locked by task_0_1, queue: [task1]
        // addr2: locked by task1, queue: []
        //
        assert_matches!(state_machine.schedule_task(task2.clone()), None);
        // now
        // addr1: locked by task_0_1, queue: [task2, task1]
        // addr2: locked by task2, queue: [task1]

        assert!(!state_machine.has_buffered_task());
        state_machine.deschedule_task(&task0_1);
        assert!(state_machine.has_buffered_task());
        // now
        // addr1: locked by task2, queue: [task1]
        // addr2: locked by task2, queue: [task1]

        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(99)
        );

        state_machine.deschedule_task(&task2);
        assert!(state_machine.has_buffered_task());

        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(101)
        );
        state_machine.deschedule_task(&task1);

        dbg!(state_machine);
        // task1
        //      blocked by addr1
        //      locking addr2
        // task2
        //      locking addr1
        //      blocked by addr2
        //
        /*
        assert_matches!(
            state_machine
                .schedule_task(task0_2.clone())
                .map(|t| t.task_index()),
            Some(51)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);
        */
    }

    #[test]
    fn test_higher_priority_locking_write_write_and_read_read() {
        let conflicting_address1 = Pubkey::new_unique();
        let conflicting_address2 = Pubkey::new_unique();
        let sanitized1 =
            transaction_with_writable_address2(conflicting_address1, conflicting_address2);
        let sanitized2 =
            transaction_with_writable_address2(conflicting_address1, conflicting_address2);
        let sanitized0_1 = transaction_with_writable_address(conflicting_address1);
        //let sanitized0_2 = transaction_with_writable_address(
        let usage_queues = Rc::new(RefCell::new(HashMap::new()));
        let address_loader = &mut create_address_loader(Some(usage_queues.clone()));
        let task0_1 = SchedulingStateMachine::create_task(sanitized0_1, 50, address_loader);
        //let task0_2 = SchedulingStateMachine::create_task(sanitized0_2, 51, address_loader);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 99, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };

        assert_matches!(
            state_machine
                .schedule_task(task0_1.clone())
                .map(|t| t.task_index()),
            Some(50)
        );
        // now
        // addr1: locked by task_0_1, queue: []
        // addr2: unlocked, queue: []

        assert_matches!(state_machine.schedule_task(task1.clone()), None);
        // now
        // addr1: locked by task_0_1, queue: [task1]
        // addr2: locked by task1, queue: []
        //
        assert_matches!(state_machine.schedule_task(task2.clone()), None);
        // now
        // addr1: locked by task_0_1, queue: [task2, task1]
        // addr2: locked by task2, queue: [task1]

        assert!(!state_machine.has_buffered_task());
        state_machine.deschedule_task(&task0_1);
        assert!(state_machine.has_buffered_task());
        // now
        // addr1: locked by task2, queue: [task1]
        // addr2: locked by task2, queue: [task1]

        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(99)
        );

        state_machine.deschedule_task(&task2);
        assert!(state_machine.has_buffered_task());

        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(101)
        );
        state_machine.deschedule_task(&task1);

        dbg!(state_machine);
        // task1
        //      blocked by addr1
        //      locking addr2
        // task2
        //      locking addr1
        //      blocked by addr2
        //
        /*
        assert_matches!(
            state_machine
                .schedule_task(task0_2.clone())
                .map(|t| t.task_index()),
            Some(51)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);
        */
    }

    #[test]
    fn test_higher_priority_locking_read_write_simple() {
        let conflicting_address1 = Pubkey::new_unique();
        let conflicting_address2 = Pubkey::new_unique();
        let sanitized1 =
            transaction_with_writable_read2(conflicting_address1, conflicting_address2);
        let sanitized2 =
            transaction_with_writable_address2(conflicting_address1, conflicting_address2);
        let sanitized0_1 = transaction_with_writable_address(conflicting_address1);
        //let sanitized0_2 = transaction_with_writable_address(
        let usage_queues = Rc::new(RefCell::new(HashMap::new()));
        let address_loader = &mut create_address_loader(Some(usage_queues.clone()));
        let task0_1 = SchedulingStateMachine::create_task(sanitized0_1, 50, address_loader);
        //let task0_2 = SchedulingStateMachine::create_task(sanitized0_2, 51, address_loader);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 99, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };

        assert_matches!(
            state_machine
                .schedule_task(task0_1.clone())
                .map(|t| t.task_index()),
            Some(50)
        );
        // now
        // addr1: locked by task_0_1, queue: []
        // addr2: unlocked, queue: []

        assert_matches!(state_machine.schedule_task(task1.clone()), None);
        // now
        // addr1: locked by task_0_1, queue: [task1]
        // addr2: locked by task1, queue: []
        //
        assert_matches!(state_machine.schedule_task(task2.clone()), None);
        // now
        // addr1: locked by task_0_1, queue: [task2, task1]
        // addr2: locked by task2, queue: [task1]

        assert!(!state_machine.has_buffered_task());
        state_machine.deschedule_task(&task0_1);
        assert!(state_machine.has_buffered_task());
        // now
        // addr1: locked by task2, queue: [task1]
        // addr2: locked by task2, queue: [task1]

        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(99)
        );

        state_machine.deschedule_task(&task2);
        assert!(state_machine.has_buffered_task());

        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(101)
        );
        state_machine.deschedule_task(&task1);

        dbg!(state_machine);
        // task1
        //      blocked by addr1
        //      locking addr2
        // task2
        //      locking addr1
        //      blocked by addr2
        //
        /*
        assert_matches!(
            state_machine
                .schedule_task(task0_2.clone())
                .map(|t| t.task_index()),
            Some(51)
        );
        assert_matches!(state_machine.schedule_task(task2.clone()), None);
        */
    }

    #[test]
    fn test_higher_priority_locking_read_write_complex() {
        let conflicting_address1 = Pubkey::new_unique();
        let conflicting_address2 = Pubkey::new_unique();
        let sanitized0_1 = transaction_with_readonly_address(conflicting_address2);
        let sanitized1 = transaction_with_writable_read2(
            *sanitized0_1.message().fee_payer(),
            conflicting_address2,
        );
        let sanitized1_2 =
            transaction_with_writable_read2(conflicting_address1, conflicting_address2);
        let sanitized1_3 =
            transaction_with_writable_read2(conflicting_address1, conflicting_address2);
        let sanitized2 =
            transaction_with_writable_address2(Pubkey::new_unique(), conflicting_address2);
        //let sanitized0_2 = transaction_with_writable_address(
        let usage_queues = Rc::new(RefCell::new(HashMap::new()));
        let address_loader = &mut create_address_loader(Some(usage_queues.clone()));
        let task0_1 = SchedulingStateMachine::create_task(sanitized0_1, 50, address_loader);
        //let task0_2 = SchedulingStateMachine::create_task(sanitized0_2, 51, address_loader);
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task1_2 = SchedulingStateMachine::create_task(sanitized1_2, 103, address_loader);
        let task1_3 = SchedulingStateMachine::create_task(sanitized1_3, 104, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 99, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };

        assert_matches!(
            state_machine
                .schedule_task(task0_1.clone())
                .map(|t| t.task_index()),
            Some(50)
        );
        // now
        // addr1: unlocked, queue: []
        // addr2: locked by task0_1, queue: []

        assert_matches!(state_machine.schedule_task(task1.clone()), None);
        // now
        // addr1: unlocked, queue: []
        // addr2: locked by [task0_1, task1], queue: []

        assert_matches!(
            state_machine
                .schedule_task(task1_2.clone())
                .map(|t| t.task_index()),
            Some(103)
        );
        // now
        // addr1: locked by task1_2, queue: []
        // addr2: locked by [task0_1, task1, task1_2], queue: []

        assert_matches!(
            state_machine
                .schedule_task(task1_3.clone())
                .map(|t| t.task_index()),
            None
        );
        // now
        // addr1: locked by task1_2, queue: [task1_3]
        // addr2: locked by [task0_1, task1, task1_2, task1_3], queue: []

        assert_matches!(state_machine.schedule_task(task2.clone()), None);
        // now
        // addr1: locked by task1_2, queue: [task1_3]
        // addr2: locked by [task0_1, task1_2], queue: [task2, task1, task1_3]

        assert!(!state_machine.has_buffered_task());
        dbg!(state_machine.buffered_task_queue_count());
        state_machine.deschedule_task(&task0_1);
        dbg!(state_machine.buffered_task_queue_count());
        assert!(!state_machine.has_buffered_task());
        // now
        // addr1: locked by task1_2, queue: [task1_3]
        // addr2: locked by task1_2, queue: [task2, task1, task1_3]
        //
        assert!(!state_machine.has_buffered_task());
        state_machine.deschedule_task(&task1_2);
        assert!(state_machine.has_buffered_task());
        // now
        // addr1: unlocked, queue: [task1_3]
        // addr2: unlocked, queue: [task2, task1, task1_3]

        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(99)
        );
        // now
        // addr1: unlocked, queue: [task1_3]
        // addr2: locked by task2, queue: [task1, task1_3]

        assert!(!state_machine.has_buffered_task());
        state_machine.deschedule_task(&task2);
        assert!(state_machine.has_buffered_task());
        // now
        // addr1: unlocked, queue: [task1_3]
        // addr2: unlocked, queue: [task1, task1_3]

        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(101)
        );
        assert_matches!(
            state_machine
                .schedule_next_buffered_task()
                .map(|t| t.task_index()),
            Some(104)
        );
        state_machine.deschedule_task(&task1);
        state_machine.deschedule_task(&task1_3);
    }

    #[test]
    fn test_eager_scheduling() {
        let conflicting_address1 = Pubkey::new_unique();
        let conflicting_address2 = Pubkey::new_unique();
        let conflicting_address3 = Pubkey::new_unique();
        let conflicting_address4 = Pubkey::new_unique();

        let sanitized1 =
            transaction_with_writable_address2(conflicting_address1, conflicting_address2);
        let sanitized2 =
            transaction_with_writable_address2(conflicting_address2, conflicting_address3);
        let sanitized3 =
            transaction_with_writable_address2(conflicting_address3, conflicting_address4);
        let usage_queues = Rc::new(RefCell::new(HashMap::new()));
        let address_loader = &mut create_address_loader(Some(usage_queues.clone()));
        let task1 = SchedulingStateMachine::create_task(sanitized1, 101, address_loader);
        let task2 = SchedulingStateMachine::create_task(sanitized2, 102, address_loader);
        let task3 = SchedulingStateMachine::create_task(sanitized3, 103, address_loader);

        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test2()
        };
        assert!(!state_machine.has_buffered_task());

        assert_matches!(
            state_machine
                .schedule_task(task1.clone())
                .map(|t| t.task_index()),
            Some(101)
        );
        assert!(!state_machine.has_buffered_task());
        assert_matches!(
            state_machine
                .schedule_task(task2.clone())
                .map(|t| t.task_index()),
            None
        );
        assert!(!state_machine.has_buffered_task());
        assert_matches!(
            state_machine
                .schedule_task(task3.clone())
                .map(|t| t.task_index()),
            None
        );
        // now
        // addr1: task1 |
        // addr2: task1 | task2, task3, task4, task5, task6, task7, task8,      , task10
        // addr3:       | task2, task3, task4, task5, task6, task7, task8, task9,       , task11
        // addr4:       |               task4, task5, task6, task7, task8, task9, task10
        assert!(state_machine.has_buffered_task());
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    #[should_panic(expected = "internal error: entered unreachable code")]
    fn test_unreachable_unlock_conditions1() {
        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        let usage_queue = UsageQueue::default();
        usage_queue
            .0
            .with_borrow_mut(&mut state_machine.usage_queue_token, |usage_queue| {
                let _ = usage_queue.unlock(&LockContext::new(UsageQueue::default(), RequestedUsage::Writable), 0, &mut state_machine.count_token);
            });
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    #[should_panic(expected = "assertion `left == right` failed\n  left: (3, Readonly)\n right: (3, Writable)")]
    fn test_unreachable_unlock_conditions2() {
        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        let usage_queue = UsageQueue::default();
        let sanitized = simplest_transaction();
        let task = SchedulingStateMachine::create_task(sanitized.clone(), 3, &mut |_| {
            UsageQueue::default()
        });
        let lock_context = LockContext::new(usage_queue.clone(), RequestedUsage::Readonly);
        usage_queue
            .0
            .with_borrow_mut(&mut state_machine.usage_queue_token, |usage_queue| {
                let task_index = task.index();
                usage_queue.current_usage = Some(Usage::Writable(task));
                let _ = usage_queue.unlock(&lock_context, task_index, &mut state_machine.count_token);
            });
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    #[should_panic(expected = "internal error: entered unreachable code")]
    fn test_unreachable_unlock_conditions3() {
        let mut state_machine = unsafe {
            SchedulingStateMachine::exclusively_initialize_current_thread_for_scheduling_for_test()
        };
        let usage_queue = UsageQueue::default();
        let sanitized = simplest_transaction();
        let task = SchedulingStateMachine::create_task(sanitized.clone(), 3, &mut |_| {
            UsageQueue::default()
        });
        usage_queue
            .0
            .with_borrow_mut(&mut state_machine.usage_queue_token, |usage_queue| {
                let task_index = task.index();
                usage_queue.current_usage = Some(Usage::Readonly(ShortCounter::one()));
                let _ = usage_queue.unlock(&LockContext::new(UsageQueue::default(), RequestedUsage::Writable), task_index, &mut state_machine.count_token);
            });
    }
}
