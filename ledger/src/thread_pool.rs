#[cfg(feature = "shuttle-test")]
use shuttle::sync::atomic::AtomicUsize;
#[cfg(not(feature = "shuttle-test"))]
use std::sync::atomic::AtomicUsize;
use {
    crossbeam_channel::{RecvError, SendError, TryRecvError},
    crossbeam_utils::Backoff,
    log::error,
    std::{
        mem,
        sync::{
            Arc,
            atomic::{AtomicU32, Ordering, fence},
        },
        thread::{self, JoinHandle},
    },
};

/// Helper to coordinate sleeping and waking between senders and receivers.
///
/// When throughput is high and perf matters the most, this code is basically zero cost. When
/// throughput is low, the futex based protocol described below kicks in, which is much more
/// performant than crossbeam's `SyncWaker` (which, among other things, uses a mutex contended at
/// every wake up by all senders and receivers).
///
/// # Synchronization protocol
///
/// Receivers sleep waiting for new messages when a channel is empty. After the underlying channel's
/// `try_recv()` returns `Empty`, a receiver registers a waiter and calls `try_recv()` again before
/// sleeping. If the channel is not empty anymore, the receiver returns the message immediately. If
/// the channel is still empty, the receiver sleeps until a sender wakes it up.
///
/// When a sender sends a message, it enqueues it into the channel and calls `wake_one()`. If no
/// receivers are sleeping, `wake_one()` _does nothing_. If any receivers are sleeping, one is woken
/// up so it can process the message.
///
/// The fences in `register_waiter()` and `wake_*()` allow the "`wake_one()` does nothing"
/// optimization, guaranteeing that either a receiver's second `try_recv()` observes a message, or
/// the sender observes the registered waiter and so can wake it.
///
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
        // see Receiver::recv() on why this is needed
        fence(Ordering::SeqCst);
        WakeWaiter {
            event: self,
            cookie,
        }
    }

    fn wake_one(&self) {
        // see Receiver::recv() on why this is needed
        fence(Ordering::SeqCst);
        if self.waiters.load(Ordering::Relaxed) != 0 {
            self.cookie.fetch_add(1, Ordering::Relaxed);
            atomic_wait::wake_one(&self.cookie);
        }
    }

    fn wake_all(&self) {
        // see Receiver::recv() on why this is needed
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

struct Shared {
    wake_event: WakeEvent,
    // Used to keep track of the number of senders. Each sender drops its underlying channel handle
    // before decrementing this count, so the last sender to decrement it can wake all waiters with
    // the channel already disconnected.
    num_senders: AtomicUsize,
}

/// Wrapper around [`crossbeam_channel::Sender`] that avoids contention by using a futex to wake
/// sleeping receivers when a message is sent.
struct Sender<T> {
    inner: crossbeam_channel::Sender<T>,
    shared: Arc<Shared>,
}

impl<T> Sender<T> {
    fn send(&self, value: T) -> Result<(), SendError<T>> {
        self.inner.send(value)?;
        self.shared.wake_event.wake_one();
        Ok(())
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

/// Wrapper around [`crossbeam_channel::Receiver`] that avoids contention by using a futex to sleep
/// waiting for messages when the channel is empty.
struct Receiver<T> {
    inner: crossbeam_channel::Receiver<T>,
    shared: Arc<Shared>,
}

impl<T> Receiver<T> {
    fn recv(&self) -> Result<T, RecvError> {
        loop {
            let backoff = Backoff::new();
            loop {
                match self.inner.try_recv() {
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
            let waiter = self.shared.wake_event.register_waiter();
            match self.inner.try_recv() {
                Ok(value) => return Ok(value),
                Err(TryRecvError::Disconnected) => return Err(RecvError),
                Err(TryRecvError::Empty) => waiter.wait(),
            }
        }
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

fn bounded<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert_ne!(capacity, 0, "channel capacity must be nonzero");
    let (sender, receiver) = crossbeam_channel::bounded(capacity);
    let shared = Arc::new(Shared {
        wake_event: WakeEvent::default(),
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

pub(crate) trait WorkerJob: Send + 'static {
    fn run(self);
}

pub(crate) struct WorkerPool<J: WorkerJob> {
    job_sender: Sender<J>,
    worker_handles: Vec<JoinHandle<()>>,
}

impl<J: WorkerJob> WorkerPool<J> {
    pub(crate) fn new(
        thread_name_prefix: &str,
        num_workers: usize,
        job_queue_capacity: usize,
    ) -> Self {
        assert_ne!(num_workers, 0, "worker pool must have at least one worker");
        let (job_sender, job_receiver) = bounded::<J>(job_queue_capacity);
        let worker_handles = (0..num_workers)
            .map(|index| {
                let job_receiver = job_receiver.clone();
                thread::Builder::new()
                    .name(format!("{thread_name_prefix}{index:02}"))
                    .stack_size(2 * 1024 * 1024)
                    .spawn(move || {
                        while let Ok(job) = job_receiver.recv() {
                            job.run();
                        }
                    })
                    .expect("failed to spawn worker thread")
            })
            .collect();
        Self {
            job_sender,
            worker_handles,
        }
    }

    pub(crate) fn send(&self, job: J) {
        self.job_sender
            .send(job)
            .expect("worker threads exited unexpectedly");
    }

    pub(crate) fn num_workers(&self) -> usize {
        self.worker_handles.len()
    }
}

impl<J: WorkerJob> Drop for WorkerPool<J> {
    fn drop(&mut self) {
        // drop the sender so the workers exit
        let (tmp, _) = bounded(1);
        drop(mem::replace(&mut self.job_sender, tmp));
        for worker_handle in self.worker_handles.drain(..) {
            if let Err(err) = worker_handle.join() {
                error!("worker thread failed: {err:?}");
            }
        }
    }
}

#[cfg(all(test, not(feature = "shuttle-test")))]
mod tests {
    use {
        super::*,
        std::{sync::Barrier, thread},
    };

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

        let (sender, receiver) = bounded(NUM_RECEIVERS);
        let sender1 = sender.clone();
        drop(sender);
        let barrier = Arc::new(Barrier::new(NUM_RECEIVERS + 1));
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

        while receiver.shared.wake_event.waiters.load(Ordering::Relaxed) != NUM_RECEIVERS {
            thread::yield_now();
        }
        for _ in 0..NUM_RECEIVERS {
            sender1.send(()).unwrap();
        }
        barrier.wait();
        drop(sender1);

        for handle in handles {
            handle.join().unwrap();
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
