use std::{
    cell::Cell,
    fmt,
    marker::PhantomData,
    mem,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
};

type Job<'a, T> = Box<dyn FnOnce(&mut T) + Send + 'a>;

enum Message<'a, T> {
    Job(Job<'a, T>),
    Stop,
}

/// Represents a scoped thread pool running with per-thread mutable state.
pub struct Scope<'scope, T> {
    senders: Vec<Sender<Message<'static, T>>>,
    workers: Vec<JoinHandle<()>>,
    next_worker: AtomicUsize,
    _marker: PhantomData<Cell<&'scope mut ()>>,
}

impl<'scope, T> Scope<'scope, T>
where
    T: Send + 'scope,
{
    pub fn spawn<F>(&self, f: F)
    where
        F: FnOnce(&mut T) + Send + 'scope,
    {
        let index = self.next_worker.fetch_add(1, Ordering::Relaxed) % self.senders.len();
        let job = unsafe { mem::transmute::<Job<'scope, T>, Job<'static, T>>(Box::new(f)) };
        self.senders[index]
            .send(Message::Job(job))
            .expect("worker thread has crashed");
    }
}

impl<T> Drop for Scope<'_, T> {
    fn drop(&mut self) {
        for sender in &self.senders {
            let _ = sender.send(Message::Stop);
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker<T>(rx: usize, worker_state: usize)
where
    T: Send,
{
    let rx = rx as *mut Receiver<Message<'static, T>>;
    // SAFETY: This raw pointer was previously leaked by the main thread. This
    // thread is the only actual used of this channel receiver.
    let rx = unsafe { Box::from_raw(rx) };
    while let Ok(msg) = rx.recv() {
        match msg {
            Message::Job(job) => {
                // SAFETY: The `worker_state` comes from the `scope_with`
                // caller and is guaranteed to have 'scope lifetime.
                //
                // We make sure that the thread doesn't outlive 'scope by
                // waiting for the termination of worker threads before
                // dropping the `Scope`.
                let state = unsafe { &mut *(worker_state as *mut T) };

                let _ = catch_unwind(AssertUnwindSafe(|| job(state)));
            }
            Message::Stop => break,
        }
    }
}

/// Creates a scoped thread pool with per-thread mutable state.
///
/// All threads and jobs will be joined before this function returns.
pub fn scope_with<'scope, N, T, F, R>(name: N, worker_states: &mut [T], f: F) -> R
where
    N: fmt::Display,
    T: Send,
    F: FnOnce(&Scope<'scope, T>) -> R,
{
    let (senders, workers) = worker_states
        .iter_mut()
        .enumerate()
        .map(|(i, worker_state)| {
            let (tx, rx) = mpsc::channel();

            let worker_state_ptr = worker_state as *mut T as usize;

            let rx = Box::new(rx);
            let rx = Box::into_raw(rx) as usize;

            let handle = thread::Builder::new()
                .name(format!("{name}{i:02}"))
                .spawn(move || worker::<T>(rx, worker_state_ptr))
                .expect("failed to spawn thread");

            (tx, handle)
        })
        .collect();

    let scope = Scope {
        senders,
        workers,
        next_worker: AtomicUsize::new(0),
        _marker: PhantomData,
    };

    f(&scope)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_thread_pool_num_threads() {
        const NUM_THREADS: usize = 4;
        let mut states: Vec<()> = (0..NUM_THREADS).map(|_| ()).collect();

        scope_with("foo", &mut states, |s| {
            assert_eq!(s.senders.len(), NUM_THREADS);
            assert_eq!(s.workers.len(), NUM_THREADS);
        });
    }

    /// Checks whether scoped tasks can use immutable references.
    #[test]
    fn test_thread_pool_scope_ref() {
        let mut states: Vec<Vec<u32>> = (0..num_cpus::get())
            .map(|_| Vec::with_capacity(32))
            .collect();
        let v: Vec<_> = (0..10_000).collect();

        scope_with("foo", &mut states, |s| {
            for i in v.iter() {
                s.spawn(move |state| state.push(*i));
            }
        });

        let mut res: Vec<u32> = states.into_iter().flatten().collect();
        res.sort();
        assert_eq!(v, res);
    }

    /// Checks whether scoped tasks can use mutable references.
    #[test]
    fn test_thread_pool_scope_ref_mut() {
        let mut states: Vec<()> = (0..num_cpus::get()).map(|_| ()).collect();
        let mut v: Vec<u32> = (0..10_000).collect();

        scope_with("foo", &mut states, |s| {
            for i in v.iter_mut() {
                s.spawn(|_| *i = i.saturating_add(1));
            }
        });

        assert_eq!(v, (1..=10_000).collect::<Vec<u32>>());
    }
}
