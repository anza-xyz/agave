use {
    crossbeam_channel::{bounded, Sender},
    std::{
        fmt,
        marker::PhantomData,
        mem,
        panic::{catch_unwind, AssertUnwindSafe},
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Condvar, Mutex,
        },
        thread::{self, sleep, JoinHandle},
        time::Duration,
    },
    thiserror::Error,
};

#[derive(Default)]
struct ThreadPoolState {
    pending: AtomicUsize,
    join_lock: Mutex<()>,
    join_cvar: Condvar,
}

impl ThreadPoolState {
    #[inline]
    pub fn submit(&self) {
        self.pending.fetch_add(1, Ordering::SeqCst);
    }

    #[inline]
    pub fn complete(&self) {
        // Mark the current job complete.
        let pending_old = self.pending.fetch_sub(1, Ordering::SeqCst);

        // If that was the last job, wake joiners.
        if pending_old == 1 {
            let _guard = self.join_lock.lock().unwrap();
            self.join_cvar.notify_one()
        }
    }

    #[inline]
    pub fn join(&self) {
        let mut lock = self.join_lock.lock().unwrap();

        while self.pending.load(Ordering::SeqCst) > 0 {
            lock = self.join_cvar.wait(lock).unwrap();
        }
    }
}

#[derive(Debug, Error)]
pub enum ThreadPoolError {
    #[error("requested index {0} is out of bounds {1}")]
    ThreadIxOutOfBounds(usize, usize),
}

type Job<'a> = Box<dyn FnOnce() + Send + 'a>;

enum Message<'a> {
    Job(Job<'a>),
    Stop,
}

pub struct ThreadPool {
    senders: Vec<Sender<Message<'static>>>,
    workers: Vec<JoinHandle<()>>,
    pool_state: Arc<ThreadPoolState>,
}

impl ThreadPool {
    pub fn new<N>(name: N, num_workers: usize, channel_bound: usize) -> Self
    where
        N: fmt::Display,
    {
        let pool_state = Arc::new(ThreadPoolState::default());
        let (senders, workers) = (0..num_workers)
            .map(|i| {
                let (sender, receiver) = bounded(channel_bound);
                let pool_state = Arc::clone(&pool_state);
                let worker = thread::Builder::new()
                    .name(format!("{name}{i:02}"))
                    .spawn(move || loop {
                        const RECV_TIMEOUT: Duration = Duration::from_nanos(1000);
                        if let Ok(msg) = receiver.try_recv() {
                            match msg {
                                Message::Job(job) => {
                                    let _ = catch_unwind(AssertUnwindSafe(job));
                                    pool_state.complete();
                                }
                                Message::Stop => break,
                            }
                        } else {
                            sleep(RECV_TIMEOUT);
                        }
                    })
                    .unwrap();
                (sender, worker)
            })
            .collect();

        Self {
            senders,
            workers,
            pool_state,
        }
    }

    pub fn scope<'pool, 'scope, F, R>(&'pool self, f: F) -> R
    where
        F: FnOnce(&Scope<'pool, 'scope>) -> R,
    {
        let scope = Scope {
            pool: self,
            _scope: PhantomData,
        };
        f(&scope)
    }

    pub fn join(&self) {
        self.pool_state.join();
    }
}

impl Drop for ThreadPool {
    fn drop(&mut self) {
        for sender in self.senders.iter() {
            sender.send(Message::Stop).unwrap();
        }
        let workers = mem::take(&mut self.workers);
        for worker in workers {
            worker.join().unwrap();
        }
    }
}

pub struct Scope<'pool, 'scope> {
    pool: &'pool ThreadPool,
    _scope: PhantomData<&'scope mut ()>,
}

impl<'scope> Scope<'_, 'scope> {
    pub fn spawn<F>(&self, thread_index: usize, f: F) -> Result<(), ThreadPoolError>
    where
        F: FnOnce() + Send + 'scope,
    {
        // This lazy evaluation avoids calling `len()` every time.
        #[allow(clippy::unnecessary_lazy_evaluations)]
        let sender = self.pool.senders.get(thread_index).ok_or_else(|| {
            ThreadPoolError::ThreadIxOutOfBounds(thread_index, self.pool.workers.len())
        })?;

        self.pool.pool_state.submit();

        // SAFETY: The receivers of the workers channels are owned by threads.
        // That leads into the requirement of every element sent through these
        // channels to be 'static.
        // However, our `Drop` implementation for `Scope` makes sure that all
        // the current jobs are finished. Practically, that means no job can
        // outlive the scope and we can guarantee the 'scope lifetime of the
        // job to the caller.
        let f = unsafe { mem::transmute::<Job<'scope>, Job<'static>>(Box::new(f)) };
        sender.send(Message::Job(f)).unwrap();

        Ok(())
    }
}

impl Drop for Scope<'_, '_> {
    fn drop(&mut self) {
        self.pool.join();
    }
}

#[cfg(test)]
mod test {
    use {super::*, boxcar::Vec as BoxcarVec};

    /// Checks whether worker states are actually modified by jobs.
    #[test]
    fn test_thread_pool() {
        let counter = AtomicUsize::default();
        let num_workers = num_cpus::get();
        let thread_pool = ThreadPool::new("foo", num_workers, 10);
        thread_pool.scope(|s| {
            for i in 0..1000 {
                s.spawn(i % num_workers, || {
                    counter.fetch_add(1, Ordering::Relaxed);
                })
                .unwrap();
            }
        });

        assert_eq!(counter.load(Ordering::Relaxed), 1000);
    }

    #[test]
    fn test_thread_pool_boxcar() {
        let input = vec!["Anne", "Darius", "Elena", "Henry", "Tara", "Will"];
        let res = BoxcarVec::new();
        let num_workers = num_cpus::get();
        let thread_pool = ThreadPool::new("foo", num_workers, 1);
        thread_pool.scope(|s| {
            for (i, name) in input.iter().enumerate() {
                s.spawn(i % num_workers, || {
                    res.push(*name);
                })
                .unwrap();
            }
        });
        let mut names: Vec<_> = res.into_iter().collect();
        names.sort();
        assert_eq!(names, input)
    }
}
