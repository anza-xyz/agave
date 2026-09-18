//! Futex-based wakeups for [`crossbeam_channel`] channels feeding a pool of consumer threads.
//!
//! Crossbeam's blocking `recv()` and `select!` park threads through a per-channel `SyncWaker`: a
//! mutex-protected list that every blocking receiver joins and leaves, and that every sender must
//! lock to wake anyone, holding the lock across the wake syscall. With many producers and many
//! consumers on a channel that is usually empty, that mutex becomes the bottleneck.
//!
//! Even at high producer throughput, however, consumers that keep up can repeatedly drain the lanes
//! and sleep. Frequent sleep/wake transitions can therefore cause contention even at high throughput,
//! and degrade it. The shared wake group avoids crossbeam's per-channel wake mutexes in this regime.
//!
//! This crate keeps the crossbeam channel as the queue but never blocks on it. Receivers only ever
//! call `try_recv()`, so the channel's waker list stays empty and a sender's notify path is a single
//! atomic load. Sleeping and waking go through a [`WakeGroup`]: a futex word plus a waiter count,
//! owned by the consumer pool rather than by any one channel. Any number of channels ("lanes") can
//! feed one group, which is what lets a consumer wait on several lanes without a `select!`.
//!
//! # Synchronization protocol
//!
//! Receivers sleep waiting for new messages when a channel is empty. After the underlying channel's
//! `try_recv()` returns `Empty`, a receiver registers a waiter and calls `try_recv()` again before
//! sleeping. If the channel is not empty anymore, the receiver returns the message immediately. If
//! the channel is still empty, the receiver sleeps until a sender wakes it up.
//!
//! When a sender sends a message, it enqueues it into the channel and calls `wake_one()`. If no
//! receivers are sleeping, `wake_one()` _does nothing_. If any receivers are sleeping, one is woken
//! up so it can process the message.
//!
//! The fences in `register_waiter()` and `wake_*()` allow the "`wake_one()` does nothing"
//! optimization, guaranteeing that either a receiver's second `try_recv()` observes a message, or
//! the sender observes the registered waiter and so can wake it.
//!
//! # Multiple lanes
//!
//! [`WakeGroup::recv_with`] takes a poll closure instead of a channel, so a consumer can poll any
//! number of lanes of different types and fold them into one result; in the protocol above, that
//! closure takes the place of `try_recv()`. Every lane polled inside one `recv_with` call must have
//! been created on that same group: a lane on another group never wakes this group's sleepers.
//!
//! Every sleeping consumer on a group must also poll every lane on that group. `wake_one()` can
//! wake any sleeper; if that consumer does not poll the lane with new data, it can go back to sleep
//! while the data's intended consumer remains asleep. Use separate groups for consumers that drain
//! different sets of lanes, and only use [`Receiver::recv`] on a group with a single lane.

#![cfg(feature = "agave-unstable-api")]

pub use crossbeam_channel::{RecvError, SendError, TryRecvError, TrySendError};
#[cfg(feature = "shuttle-test")]
use shuttle::sync::atomic::AtomicUsize;
#[cfg(not(feature = "shuttle-test"))]
use std::sync::atomic::AtomicUsize;
use {
    crossbeam_utils::Backoff,
    std::{
        mem,
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering, fence},
        },
    },
};

/// Helper to coordinate sleeping and waking between senders and receivers.
///
/// See the crate docs for the synchronization protocol.
#[derive(Default)]
struct WakeEvent {
    // waiters wait until the cookie changes
    cookie: AtomicU32,
    // number of waiters currently waiting on the cookie
    waiters: AtomicUsize,
}

impl WakeEvent {
    /// Returns a waiter that can be used to wait for this event to be signaled.
    fn register_waiter(&self) -> WakeWaiter<'_> {
        let cookie = self.cookie.load(Ordering::Relaxed);
        self.waiters.fetch_add(1, Ordering::Relaxed);
        // see WakeGroup::recv_with() on why this is needed
        fence(Ordering::SeqCst);
        WakeWaiter {
            event: self,
            cookie,
        }
    }

    fn wake_one(&self) {
        // see WakeGroup::recv_with() on why this is needed
        fence(Ordering::SeqCst);
        if self.waiters.load(Ordering::Relaxed) != 0 {
            self.cookie.fetch_add(1, Ordering::Relaxed);
            atomic_wait::wake_one(&self.cookie);
        }
    }

    fn wake_all(&self) {
        // see WakeGroup::recv_with() on why this is needed
        fence(Ordering::SeqCst);
        if self.waiters.load(Ordering::Relaxed) != 0 {
            self.cookie.fetch_add(1, Ordering::Relaxed);
            atomic_wait::wake_all(&self.cookie);
        }
    }
}

/// A waiter guard that can be used to wait for a wake event to be signaled.
///
/// When dropped, it automatically decrements the number of waiters on the event.
struct WakeWaiter<'a> {
    event: &'a WakeEvent,
    cookie: u32,
}

impl WakeWaiter<'_> {
    fn wait(self) {
        if self.event.cookie.load(Ordering::Relaxed) == self.cookie {
            atomic_wait::wait(&self.event.cookie, self.cookie);
        }
    }
}

impl Drop for WakeWaiter<'_> {
    fn drop(&mut self) {
        self.event.waiters.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The wake point shared by every lane feeding one pool of consumers.
///
/// Create one per consumer pool, build the pool's lanes on it with [`bounded`], and have consumers
/// block in [`WakeGroup::recv_with`] (or [`Receiver::recv`] for a single lane).
/// All sleeping consumers must poll every lane on the group: a wake can go to any of them.
#[derive(Default)]
pub struct WakeGroup {
    wake_event: WakeEvent,
}

impl WakeGroup {
    /// Wakes one sleeping receiver, if any. Call it _after_ making visible the change the
    /// receiver's poll looks for.
    pub fn wake_one(&self) {
        self.wake_event.wake_one();
    }

    /// Wakes every sleeping receiver, if any. For conditions all receivers must observe, such as a
    /// lane disconnect or a pool exit flag. Call it _after_ making the condition visible.
    pub fn wake_all(&self) {
        self.wake_event.wake_all();
    }

    /// Blocks until `poll` returns something other than `Err(TryRecvError::Empty)`.
    ///
    /// `poll` must observe every condition that should end the wait: data on any lane, a
    /// disconnected lane, an exit flag. Every producer of such a condition must call
    /// [`wake_one`](Self::wake_one) or [`wake_all`](Self::wake_all) on this group after making it
    /// visible. `Err(TryRecvError::Disconnected)` from `poll` ends the wait with `Err(RecvError)`.
    ///
    /// All consumers sleeping on this group must poll the same set of lanes. Otherwise a send can
    /// wake a consumer that cannot receive the message, leaving the intended consumer asleep.
    pub fn recv_with<T>(
        &self,
        mut poll: impl FnMut() -> Result<T, TryRecvError>,
    ) -> Result<T, RecvError> {
        loop {
            let backoff = Backoff::new();
            loop {
                match poll() {
                    Ok(value) => return Ok(value),
                    Err(TryRecvError::Disconnected) => return Err(RecvError),
                    Err(TryRecvError::Empty) if backoff.is_completed() => break,
                    Err(TryRecvError::Empty) => backoff.snooze(),
                }
            }

            // There's a potential race in the window between getting `Empty` above, and registering
            // the waiter below. A sender might queue a message in the meantime, `wake_one()` might
            // observe `wake_event.waiters == 0` and skip the wake syscall (optimization to avoid
            // one syscall per message when the channel has large bursts when it's not empty).
            //
            // So below we check again, this time _after_ calling `register_waiter()` and so with
            // `wake_event.waiters` guaranteed to be non zero. The paired fences (see
            // `register_waiter()` and `wake_*()`) guarantee that either this check observes a
            // message into the channel (or `Disconnected`), or the sender sees us in `wait()` and
            // wakes us.
            let waiter = self.wake_event.register_waiter();
            match poll() {
                Ok(value) => return Ok(value),
                Err(TryRecvError::Disconnected) => return Err(RecvError),
                Err(TryRecvError::Empty) => waiter.wait(),
            }
        }
    }
}

struct Shared {
    wake_group: Arc<WakeGroup>,
    // Used to keep track of the number of senders. Each sender drops its underlying channel handle
    // before decrementing this count, so the last sender to decrement it can wake all waiters with
    // the channel already disconnected.
    num_senders: AtomicUsize,
}

/// Wrapper around [`crossbeam_channel::Sender`] that avoids contention by using a futex to wake
/// sleeping receivers when a message is sent.
pub struct Sender<T> {
    inner: crossbeam_channel::Sender<T>,
    shared: Arc<Shared>,
}

impl<T> Sender<T> {
    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        self.inner.send(value)?;
        self.shared.wake_group.wake_one();
        Ok(())
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.inner.try_send(value)?;
        self.shared.wake_group.wake_one();
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn capacity(&self) -> Option<usize> {
        self.inner.capacity()
    }

    pub fn wake_group(&self) -> &Arc<WakeGroup> {
        &self.shared.wake_group
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared.num_senders.fetch_add(1, Ordering::Relaxed);
        Self {
            inner: self.inner.clone(),
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let (replacement, _) = crossbeam_channel::bounded(0);
        drop(mem::replace(&mut self.inner, replacement));
        if self.shared.num_senders.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.wake_group.wake_all();
        }
    }
}

/// Wrapper around [`crossbeam_channel::Receiver`] that avoids contention by using a futex to sleep
/// waiting for messages when the channel is empty.
pub struct Receiver<T> {
    inner: crossbeam_channel::Receiver<T>,
    shared: Arc<Shared>,
}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.inner.try_recv()
    }

    /// Blocking receive on this lane alone. Only use this with a group dedicated to this lane.
    /// Consumers sharing a group across several lanes must all poll those lanes through
    /// [`WakeGroup::recv_with`], so whichever consumer is woken can receive the queued message.
    pub fn recv(&self) -> Result<T, RecvError> {
        self.shared.wake_group.recv_with(|| self.inner.try_recv())
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn capacity(&self) -> Option<usize> {
        self.inner.capacity()
    }

    pub fn wake_group(&self) -> &Arc<WakeGroup> {
        &self.shared.wake_group
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shared: Arc::clone(&self.shared),
        }
    }
}

/// Creates a lane of the given capacity on `group`.
///
/// Panics if `capacity` is zero: a zero-capacity crossbeam channel only completes a send when a
/// receiver is blocked in `recv()`, which never happens here.
pub fn bounded<T>(capacity: usize, group: Arc<WakeGroup>) -> (Sender<T>, Receiver<T>) {
    assert_ne!(capacity, 0, "channel capacity must be nonzero");
    let (sender, receiver) = crossbeam_channel::bounded(capacity);
    let shared = Arc::new(Shared {
        wake_group: group,
        num_senders: AtomicUsize::new(1),
    });
    (
        Sender {
            inner: sender,
            shared: Arc::clone(&shared),
        },
        Receiver {
            inner: receiver,
            shared,
        },
    )
}

#[cfg(all(test, not(feature = "shuttle-test")))]
mod tests {
    use {
        super::*,
        std::{
            sync::{Barrier, atomic::AtomicBool, mpsc},
            thread,
            time::Duration,
        },
    };

    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    /// Like `thread::spawn`, but the result comes back through a channel so a test can wait with
    /// a timeout instead of hanging in `join()` on a lost wakeup.
    fn spawn_with_result<T: Send + 'static>(
        f: impl FnOnce() -> T + Send + 'static,
    ) -> mpsc::Receiver<T> {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(f());
        });
        receiver
    }

    fn wait_for_waiters(group: &WakeGroup, expected: usize) {
        while group.wake_event.waiters.load(Ordering::Relaxed) != expected {
            thread::yield_now();
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Work {
        A(u32),
        B(&'static str),
    }

    fn poll_both(ra: &Receiver<u32>, rb: &Receiver<&'static str>) -> Result<Work, TryRecvError> {
        match ra.try_recv() {
            Ok(value) => Ok(Work::A(value)),
            Err(TryRecvError::Empty) => rb.try_recv().map(Work::B),
            Err(err) => Err(err),
        }
    }

    #[test]
    fn test_wake_before_wait() {
        let event = WakeEvent::default();
        let waiter = event.register_waiter();
        event.wake_one();
        waiter.wait();
    }

    #[test]
    fn test_wake_receivers_and_disconnect() {
        const NUM_RECEIVERS: usize = 4;

        let group = Arc::new(WakeGroup::default());
        let (sender, receiver) = bounded(NUM_RECEIVERS, Arc::clone(&group));
        let sender1 = sender.clone();
        drop(sender);
        let barrier = Arc::new(Barrier::new(NUM_RECEIVERS.saturating_add(1)));
        let handles = (0..NUM_RECEIVERS)
            .map(|_| {
                let receiver = receiver.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    receiver.recv().unwrap();
                    barrier.wait();
                    assert!(receiver.recv().is_err());
                })
            })
            .collect::<Vec<_>>();

        wait_for_waiters(&group, NUM_RECEIVERS);
        for _ in 0..NUM_RECEIVERS {
            sender1.send(()).unwrap();
        }
        barrier.wait();
        drop(sender1);

        for handle in handles {
            handle.join().unwrap();
        }
    }

    // The producer waits for the consumer to be asleep before each send, so every message goes
    // through register, futex wait and wake; a lost wakeup strands the consumer and hits the timeout.
    #[test]
    fn test_sleeping_receiver_is_woken_for_every_message() {
        const NUM_MESSAGES: usize = 1_000;

        let group = Arc::new(WakeGroup::default());
        let (sender, receiver) = bounded(1, Arc::clone(&group));
        let producer = thread::spawn(move || {
            for i in 0..NUM_MESSAGES {
                wait_for_waiters(&group, 1);
                sender.send(i).unwrap();
            }
        });
        let consumer =
            spawn_with_result(move || std::iter::from_fn(|| receiver.recv().ok()).count());
        assert_eq!(
            consumer
                .recv_timeout(TEST_TIMEOUT)
                .expect("consumer stranded"),
            NUM_MESSAGES
        );
        producer.join().unwrap();
    }

    #[test]
    fn test_recv_with_wakes_on_either_lane() {
        let group = Arc::new(WakeGroup::default());
        let (sa, ra) = bounded::<u32>(4, Arc::clone(&group));
        let (sb, rb) = bounded::<&'static str>(4, Arc::clone(&group));

        let spawn_consumer = || {
            let group = Arc::clone(&group);
            let (ra, rb) = (ra.clone(), rb.clone());
            spawn_with_result(move || group.recv_with(|| poll_both(&ra, &rb)).unwrap())
        };

        let consumer = spawn_consumer();
        wait_for_waiters(&group, 1);
        sb.try_send("b").unwrap();
        assert_eq!(
            consumer
                .recv_timeout(TEST_TIMEOUT)
                .expect("consumer stranded"),
            Work::B("b")
        );

        let consumer = spawn_consumer();
        wait_for_waiters(&group, 1);
        sa.try_send(7).unwrap();
        assert_eq!(
            consumer
                .recv_timeout(TEST_TIMEOUT)
                .expect("consumer stranded"),
            Work::A(7)
        );
    }

    #[test]
    fn test_lane_disconnect_wakes_multi_lane_sleeper() {
        let group = Arc::new(WakeGroup::default());
        let (_sa, ra) = bounded::<u32>(4, Arc::clone(&group));
        let (sb, rb) = bounded::<&'static str>(4, Arc::clone(&group));

        let consumer = {
            let group = Arc::clone(&group);
            spawn_with_result(move || group.recv_with(|| poll_both(&ra, &rb)))
        };
        wait_for_waiters(&group, 1);
        // Lane A is still connected; dropping lane B's only sender must still end the wait.
        drop(sb);
        assert_eq!(
            consumer
                .recv_timeout(TEST_TIMEOUT)
                .expect("consumer stranded"),
            Err(RecvError)
        );
    }

    #[test]
    fn test_exit_flag_wakes_sleeper() {
        let group = Arc::new(WakeGroup::default());
        let (_sender, receiver) = bounded::<()>(4, Arc::clone(&group));
        let exit = Arc::new(AtomicBool::new(false));

        let consumer = {
            let group = Arc::clone(&group);
            let exit = Arc::clone(&exit);
            spawn_with_result(move || {
                group.recv_with(|| {
                    if exit.load(Ordering::Relaxed) {
                        Err(TryRecvError::Disconnected)
                    } else {
                        receiver.inner.try_recv()
                    }
                })
            })
        };
        wait_for_waiters(&group, 1);
        exit.store(true, Ordering::Relaxed);
        group.wake_all();
        assert_eq!(
            consumer
                .recv_timeout(TEST_TIMEOUT)
                .expect("consumer stranded"),
            Err(RecvError)
        );
    }
}

#[cfg(all(test, feature = "shuttle-test"))]
mod shuttle_tests {
    use {super::*, shuttle::thread};

    #[test]
    fn test_disconnect_is_visible_before_wake() {
        shuttle::check_dfs(
            || {
                let group = Arc::new(WakeGroup::default());
                let (sender, receiver) = bounded::<()>(1, Arc::clone(&group));
                let sender1 = sender.clone();
                let observer_receiver = receiver.clone();
                let _waiter = group.wake_event.register_waiter();

                let sender_drop = thread::spawn(move || drop(sender));
                let sender1_drop = thread::spawn(move || drop(sender1));
                let observer = thread::spawn({
                    let group = Arc::clone(&group);
                    move || {
                        // A changed cookie means wake_all() has run. The underlying channel must
                        // have been disconnected before the wake became visible.
                        if group.wake_event.cookie.load(Ordering::Relaxed) != 0 {
                            assert_eq!(
                                observer_receiver.inner.try_recv(),
                                Err(TryRecvError::Disconnected),
                            );
                        }
                    }
                });

                sender_drop.join().unwrap();
                sender1_drop.join().unwrap();
                observer.join().unwrap();
                assert_ne!(group.wake_event.cookie.load(Ordering::Relaxed), 0);
                assert_eq!(receiver.inner.try_recv(), Err(TryRecvError::Disconnected));
            },
            None,
        );
    }
}
