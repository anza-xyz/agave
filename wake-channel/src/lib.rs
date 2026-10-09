//! Futex-based wakeups for [`crossbeam_channel`] channels.
//!
//! Blocking crossbeam receivers park through a per-channel `SyncWaker`, whose mutex every sender
//! must take to wake them. When consumers keep draining the channels and going back to sleep,
//! which can happen even at high throughput, that mutex can become a bottleneck. Here receivers
//! only call `try_recv()` and sleep on a [`WakeEvent`] instead.
//!
//! [`Sender::send`] can still block on crossbeam when the queue is full. Several channels can feed
//! one [`WakeEvent`], so a consumer can wait on all of them without `select!`.
//!
//! # Multiple channels
//!
//! [`WakeEvent::recv_with`] takes a poll closure, so a consumer can poll any number of channels of
//! different types and fold them into one result. Every channel polled inside one `recv_with` call
//! must have been created on that same event: a channel on another event never wakes this event's
//! sleepers.
//!
//! Every sleeping consumer on an event must also poll every channel on that event. `wake_one()` can
//! wake any sleeper; if that consumer does not poll the channel with new data, it can go back to
//! sleep while the data's intended consumer remains asleep. Use separate events for consumers that
//! drain different sets of channels. Shared-event channels return a [`SharedEventReceiver`], which
//! supports polling but does not expose a blocking `recv()` method.
//!
//! # Synchronization protocol
//!
//! Receivers sleep waiting for new messages when the channels are empty. After the caller-supplied
//! `poll()` returns `Empty`, a receiver registers a waiter and calls `poll()` again before
//! sleeping. If any channel is not empty anymore, the receiver returns the message immediately. If
//! the channels are still empty, the receiver sleeps until a sender wakes it up.
//!
//! When a sender sends a message, it enqueues it into one of the channels and calls `wake_one()`.
//! If no receivers are sleeping, `wake_one()` _does nothing_. If any receivers are sleeping, one is
//! woken up so it can process the message.
//!
//! The fences in `register_waiter()` and `wake_*()` allow the "`wake_one()` does nothing"
//! optimization, guaranteeing that either a receiver's second `poll()` observes a message, or the
//! sender observes the registered waiter and so can wake it.

#![cfg(feature = "agave-unstable-api")]

pub use crossbeam_channel::{RecvError, SendError, TryRecvError, TrySendError};
#[cfg(feature = "shuttle-test")]
use shuttle::sync::atomic::AtomicUsize;
#[cfg(not(feature = "shuttle-test"))]
use std::sync::atomic::AtomicUsize;
use {
    crossbeam_utils::Backoff,
    std::{
        marker::PhantomData,
        mem,
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering, fence},
        },
    },
};

/// Helper to coordinate sleeping and waking between senders and receivers.
///
/// Consumers block in [`WakeEvent::recv_with`]; [`Receiver::recv`] is the single-channel instance.
/// Several channels can share one event through [`bounded_with_wake_event`]; every consumer
/// sleeping on the event must then poll all of them, since a wake can go to any of them.
///
/// See the crate docs for the synchronization protocol.
#[derive(Default)]
pub struct WakeEvent {
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
        // see WakeEvent::recv_with() on why this is needed
        fence(Ordering::SeqCst);
        WakeWaiter {
            event: self,
            cookie,
        }
    }

    /// Wakes one sleeping receiver, if any. Call it _after_ making visible the change the
    /// receiver's poll looks for.
    pub fn wake_one(&self) {
        // see WakeEvent::recv_with() on why this is needed
        fence(Ordering::SeqCst);
        if self.waiters.load(Ordering::Relaxed) != 0 {
            self.cookie.fetch_add(1, Ordering::Relaxed);
            atomic_wait::wake_one(&self.cookie);
        }
    }

    /// Wakes every sleeping receiver, if any. For conditions all receivers must observe, such as a
    /// channel disconnect or an exit flag. Call it _after_ making the condition visible.
    pub fn wake_all(&self) {
        // see WakeEvent::recv_with() on why this is needed
        fence(Ordering::SeqCst);
        if self.waiters.load(Ordering::Relaxed) != 0 {
            self.cookie.fetch_add(1, Ordering::Relaxed);
            atomic_wait::wake_all(&self.cookie);
        }
    }

    /// Blocks until `poll` returns something other than `Err(TryRecvError::Empty)`.
    ///
    /// `poll` must observe every condition that should end the wait: data, a disconnect, an exit
    /// flag. Every producer of such a condition must call [`wake_one`](Self::wake_one) or
    /// [`wake_all`](Self::wake_all) on this event after making it visible.
    /// `Err(TryRecvError::Disconnected)` from `poll` ends the wait with `Err(RecvError)`.
    ///
    /// All consumers sleeping on this event must poll the same set of channels. Otherwise a send
    /// can wake a consumer that cannot receive the message, leaving the intended consumer asleep.
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
            let waiter = self.register_waiter();
            match poll() {
                Ok(value) => return Ok(value),
                Err(TryRecvError::Disconnected) => return Err(RecvError),
                Err(TryRecvError::Empty) => waiter.wait(),
            }
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

struct Shared {
    wake_event: Arc<WakeEvent>,
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
        self.shared.wake_event.wake_one();
        Ok(())
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.inner.try_send(value)?;
        self.shared.wake_event.wake_one();
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
            self.shared.wake_event.wake_all();
        }
    }
}

pub struct SingleChannelEvent;
pub struct MultiChannelEvent;

/// Receiver for a channel with its own wake event, created by [`bounded`].
pub type Receiver<T> = ReceiverImpl<T, SingleChannelEvent>;

/// Receiver for a channel sharing a wake event, created by [`bounded_with_wake_event`].
pub type SharedEventReceiver<T> = ReceiverImpl<T, MultiChannelEvent>;

pub struct ReceiverImpl<T, Mode> {
    inner: crossbeam_channel::Receiver<T>,
    shared: Arc<Shared>,
    _mode: PhantomData<Mode>,
}

impl<T, Mode> ReceiverImpl<T, Mode> {
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.inner.try_recv()
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
}

impl<T> Receiver<T> {
    /// Blocks until a message is available or all senders are disconnected.
    pub fn recv(&self) -> Result<T, RecvError> {
        self.shared.wake_event.recv_with(|| self.inner.try_recv())
    }
}

impl<T> SharedEventReceiver<T> {
    /// Returns the event shared by this channel and its peers.
    pub fn wake_event(&self) -> &Arc<WakeEvent> {
        &self.shared.wake_event
    }
}

impl<T, Mode> Clone for ReceiverImpl<T, Mode> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shared: Arc::clone(&self.shared),
            _mode: PhantomData,
        }
    }
}

/// Creates a channel of the given capacity on its own [`WakeEvent`].
///
/// Panics if `capacity` is zero.
pub fn bounded<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    bounded_inner(capacity, Arc::new(WakeEvent::default()))
}

/// Creates a channel of the given capacity on `event`.
///
/// Every consumer must poll all channels on this event through [`WakeEvent::recv_with`].
/// The returned [`SharedEventReceiver`] only supports nonblocking receives.
///
/// Panics if `capacity` is zero: a zero-capacity crossbeam channel hands each message directly to a
/// receiver, and this crate wakes a receiver only after the send has completed.
pub fn bounded_with_wake_event<T>(
    capacity: usize,
    event: Arc<WakeEvent>,
) -> (Sender<T>, SharedEventReceiver<T>) {
    bounded_inner(capacity, event)
}

fn bounded_inner<T, Mode>(
    capacity: usize,
    event: Arc<WakeEvent>,
) -> (Sender<T>, ReceiverImpl<T, Mode>) {
    assert_ne!(capacity, 0, "channel capacity must be nonzero");
    let (sender, receiver) = crossbeam_channel::bounded(capacity);
    let shared = Arc::new(Shared {
        wake_event: event,
        num_senders: AtomicUsize::new(1),
    });
    (
        Sender {
            inner: sender,
            shared: Arc::clone(&shared),
        },
        ReceiverImpl {
            inner: receiver,
            shared,
            _mode: PhantomData,
        },
    )
}

#[cfg(all(test, not(feature = "shuttle-test")))]
mod tests {
    use {
        super::*,
        std::{sync::atomic::AtomicBool, thread},
    };

    const NUM_RECEIVERS: usize = 4;

    fn wait_for_waiters(event: &WakeEvent, expected: usize) {
        while event.waiters.load(Ordering::Relaxed) != expected {
            thread::yield_now();
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
        let (sender, receiver) = bounded(NUM_RECEIVERS);
        // Testing sender-clone lifecycle (num_senders).
        let sender1 = sender.clone();
        drop(sender);
        let spawn_consumers = || {
            (0..NUM_RECEIVERS)
                .map(|_| {
                    let receiver = receiver.clone();
                    thread::spawn(move || receiver.recv())
                })
                .collect::<Vec<_>>()
        };

        let consumers = spawn_consumers();
        wait_for_waiters(&receiver.shared.wake_event, NUM_RECEIVERS);
        for _ in 0..NUM_RECEIVERS {
            sender1.send(()).unwrap();
        }
        for consumer in consumers {
            assert_eq!(consumer.join().unwrap(), Ok(()));
        }

        let consumers = spawn_consumers();
        wait_for_waiters(&receiver.shared.wake_event, NUM_RECEIVERS);
        drop(sender1);
        for consumer in consumers {
            assert_eq!(consumer.join().unwrap(), Err(RecvError));
        }
    }

    // Exercise sends racing with receiver sleep: the receiver may find a message on its second
    // poll, observe a changed cookie before blocking, or be woken from a kernel wait. Scheduling
    // determines which paths occur; this checks progress, not coverage of every path.
    #[test]
    fn test_repeated_sends_after_waiter_registration() {
        const NUM_MESSAGES: usize = 1_000;

        let (sender, receiver) = bounded(1);
        let shared = Arc::clone(&receiver.shared);
        let producer = thread::spawn(move || {
            for i in 0..NUM_MESSAGES {
                wait_for_waiters(&shared.wake_event, 1);
                sender.send(i).unwrap();
            }
        });
        let consumer = thread::spawn(move || std::iter::from_fn(|| receiver.recv().ok()).count());
        assert_eq!(consumer.join().unwrap(), NUM_MESSAGES);
        producer.join().unwrap();
    }

    #[test]
    fn test_exit_flag_wakes_all_receivers() {
        let event = Arc::new(WakeEvent::default());
        let exit = Arc::new(AtomicBool::new(false));
        let consumers = (0..NUM_RECEIVERS)
            .map(|_| {
                let event = Arc::clone(&event);
                let exit = Arc::clone(&exit);
                thread::spawn(move || {
                    event.recv_with::<()>(|| {
                        if exit.load(Ordering::Relaxed) {
                            Err(TryRecvError::Disconnected)
                        } else {
                            Err(TryRecvError::Empty)
                        }
                    })
                })
            })
            .collect::<Vec<_>>();

        wait_for_waiters(&event, NUM_RECEIVERS);
        exit.store(true, Ordering::Relaxed);
        event.wake_all();
        for consumer in consumers {
            assert_eq!(consumer.join().unwrap(), Err(RecvError));
        }
    }
    #[derive(Debug, PartialEq, Eq)]
    enum Work {
        A(u32),
        B(&'static str),
    }

    fn poll_both(
        ra: &SharedEventReceiver<u32>,
        rb: &SharedEventReceiver<&'static str>,
    ) -> Result<Work, TryRecvError> {
        match ra.try_recv() {
            Ok(value) => Ok(Work::A(value)),
            Err(TryRecvError::Empty) => rb.try_recv().map(Work::B),
            Err(err) => Err(err),
        }
    }

    #[test]
    fn test_receivers_wake_on_either_channel() {
        let event = Arc::new(WakeEvent::default());
        let (sa, ra) = bounded_with_wake_event::<u32>(NUM_RECEIVERS, Arc::clone(&event));
        let (sb, rb) = bounded_with_wake_event::<&'static str>(NUM_RECEIVERS, Arc::clone(&event));

        // Each consumer receives once, so every registered consumer must make progress.
        for expected in [Work::A(7), Work::B("b")] {
            let consumers = (0..NUM_RECEIVERS)
                .map(|_| {
                    let event = Arc::clone(&event);
                    let (ra, rb) = (ra.clone(), rb.clone());
                    thread::spawn(move || event.recv_with(|| poll_both(&ra, &rb)).unwrap())
                })
                .collect::<Vec<_>>();

            wait_for_waiters(&event, NUM_RECEIVERS);
            for _ in 0..NUM_RECEIVERS {
                match expected {
                    Work::A(value) => sa.try_send(value).unwrap(),
                    Work::B(value) => sb.try_send(value).unwrap(),
                }
            }
            for consumer in consumers {
                assert_eq!(consumer.join().unwrap(), expected);
            }
        }
    }

    #[test]
    fn test_channel_disconnect_wakes_all_shared_event_receivers() {
        let event = Arc::new(WakeEvent::default());
        let (_sa, ra) = bounded_with_wake_event::<u32>(1, Arc::clone(&event));
        let (sb, rb) = bounded_with_wake_event::<&'static str>(1, Arc::clone(&event));
        let consumers = (0..NUM_RECEIVERS)
            .map(|_| {
                let event = Arc::clone(&event);
                let (ra, rb) = (ra.clone(), rb.clone());
                thread::spawn(move || event.recv_with(|| poll_both(&ra, &rb)))
            })
            .collect::<Vec<_>>();

        wait_for_waiters(&event, NUM_RECEIVERS);
        // Channel A is still connected; dropping B's last sender must wake every consumer.
        drop(sb);
        for consumer in consumers {
            assert_eq!(consumer.join().unwrap(), Err(RecvError));
        }
    }

    #[test]
    fn test_exit_flag_wakes_all_shared_event_receivers() {
        let event = Arc::new(WakeEvent::default());
        let (_sa, ra) = bounded_with_wake_event::<u32>(1, Arc::clone(&event));
        let (_sb, rb) = bounded_with_wake_event::<&'static str>(1, Arc::clone(&event));
        let exit = Arc::new(AtomicBool::new(false));
        let consumers = (0..NUM_RECEIVERS)
            .map(|_| {
                let event = Arc::clone(&event);
                let exit = Arc::clone(&exit);
                let (ra, rb) = (ra.clone(), rb.clone());
                thread::spawn(move || {
                    event.recv_with(|| {
                        if exit.load(Ordering::Relaxed) {
                            Err(TryRecvError::Disconnected)
                        } else {
                            poll_both(&ra, &rb)
                        }
                    })
                })
            })
            .collect::<Vec<_>>();

        wait_for_waiters(&event, NUM_RECEIVERS);
        exit.store(true, Ordering::Relaxed);
        event.wake_all();
        for consumer in consumers {
            assert_eq!(consumer.join().unwrap(), Err(RecvError));
        }
    }
}

#[cfg(all(test, feature = "shuttle-test"))]
mod shuttle_tests {
    use {super::*, shuttle::thread};

    #[test]
    fn test_disconnect_is_visible_before_wake() {
        shuttle::check_dfs(
            || {
                let (sender, receiver) = bounded::<()>(1);
                let sender1 = sender.clone();
                let observer_receiver = receiver.clone();
                let _waiter = receiver.shared.wake_event.register_waiter();

                let sender_drop = thread::spawn(move || drop(sender));
                let sender1_drop = thread::spawn(move || drop(sender1));
                let observer = thread::spawn(move || {
                    // A changed cookie means wake_all() has run. The underlying channel must have
                    // been disconnected before the wake became visible.
                    if observer_receiver
                        .shared
                        .wake_event
                        .cookie
                        .load(Ordering::Relaxed)
                        != 0
                    {
                        assert_eq!(
                            observer_receiver.inner.try_recv(),
                            Err(TryRecvError::Disconnected),
                        );
                    }
                });

                sender_drop.join().unwrap();
                sender1_drop.join().unwrap();
                observer.join().unwrap();
                assert_ne!(receiver.shared.wake_event.cookie.load(Ordering::Relaxed), 0);
                assert_eq!(receiver.inner.try_recv(), Err(TryRecvError::Disconnected));
            },
            None,
        );
    }
}
