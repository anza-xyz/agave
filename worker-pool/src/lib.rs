//! A fixed set of named threads draining one wake-channel job queue.
//!
//! The pool suits services that feed a steady stream of jobs to long-lived workers:
//! - Idle workers sleep on a futex. By contrast, rayon workers spin and yield before sleeping, which
//!   costs CPU when jobs arrive in small, frequent rounds.
//! - A bounded FIFO queue applies backpressure to senders. A sender continues as soon as its job is
//!   queued, while a rayon scope blocks until its tasks finish.
//! - Each worker owns state built once when it starts, such as a socket or a cache handle.

#![cfg(feature = "agave-unstable-api")]

use {
    agave_wake_channel::{Sender, bounded},
    log::error,
    std::{
        io,
        num::NonZeroUsize,
        thread::{self, JoinHandle},
    },
};

mod results;

pub use results::{ResultReceiver, ResultRecvError};

const WORKER_STACK_SIZE: usize = 2 * 1024 * 1024;

/// Builds the job handler of each worker. A worker consumes jobs `J` and generates outputs `O`
/// through its handler. The handler also holds the state of that worker.
///
/// Implemented for closures `Fn(worker_id) -> impl FnMut(job) -> output`.
///
/// [`WorkerPoolBuilder::spawn`] only takes handlers without an output, `O = ()`. Outputs reach
/// the caller only through [`WorkerPoolBuilder::spawn_with_results`], which sends each of them to a
/// [`ResultReceiver`].
pub trait MakeJobHandler<J, O = ()> {
    type Handler: FnMut(J) -> O + Send + 'static;

    /// Builds the handler of the worker with `worker_id`. The id is unique among the running
    /// workers of the pool, and a new worker may reuse the id of an exited one.
    fn make_job_handler(&self, worker_id: usize) -> Self::Handler;
}

impl<J, O, F, H> MakeJobHandler<J, O> for F
where
    F: Fn(usize) -> H,
    H: FnMut(J) -> O + Send + 'static,
{
    type Handler = H;

    fn make_job_handler(&self, worker_id: usize) -> H {
        self(worker_id)
    }
}

/// Pool of worker threads draining one job queue. A worker exits once the [`PoolSender`] of the
/// pool is dropped and the queue is drained.
///
/// Dropping the pool joins the workers and logs a worker panic, so its owner must drop the
/// [`PoolSender`] first. A drop during a panic detaches the workers instead.
pub struct WorkerPool {
    worker_handles: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    /// Starts configuring a pool of `num_workers` threads named
    /// `{thread_name_prefix}{worker_id:02}`.
    pub fn builder(thread_name_prefix: &str, num_workers: NonZeroUsize) -> WorkerPoolBuilder {
        WorkerPoolBuilder {
            thread_name_prefix: thread_name_prefix.to_string(),
            num_workers,
            job_queue_capacity: num_workers,
        }
    }

    /// Returns the current number of workers.
    pub fn num_workers(&self) -> usize {
        self.worker_handles.len()
    }

    /// Waits for all workers to exit, which blocks until the [`PoolSender`] is dropped.
    ///
    /// Returns the panic payload of the first worker that panicked.
    #[must_use = "join returns an error if a worker thread panicked"]
    pub fn join(mut self) -> thread::Result<()> {
        self.join_workers()
    }

    fn join_workers(&mut self) -> thread::Result<()> {
        let mut result = Ok(());
        for worker_handle in self.worker_handles.drain(..) {
            let worker_result = worker_handle.join();
            if result.is_ok() {
                result = worker_result;
            }
        }
        result
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        if !thread::panicking()
            && let Err(err) = self.join_workers()
        {
            error!("worker thread failed: {err:?}");
        }
    }
}

pub struct WorkerPoolBuilder {
    thread_name_prefix: String,
    num_workers: NonZeroUsize,
    job_queue_capacity: NonZeroUsize,
}

impl WorkerPoolBuilder {
    /// Sets how many jobs the queue holds before [`PoolSender::send`] blocks. Defaults to the number
    /// of workers.
    pub fn job_queue_capacity(mut self, job_queue_capacity: NonZeroUsize) -> Self {
        self.job_queue_capacity = job_queue_capacity;
        self
    }

    /// Spawns the workers, each running its jobs through the handler `make_job_handler` builds.
    ///
    /// Returns the error of the first thread that fails to spawn. The workers spawned before it
    /// exit on their own, since their queue closes.
    pub fn spawn<J: Send + 'static>(
        self,
        make_job_handler: impl MakeJobHandler<J>,
    ) -> io::Result<(PoolSender<J>, WorkerPool)> {
        let Self {
            thread_name_prefix,
            num_workers,
            job_queue_capacity,
        } = self;
        let (job_sender, job_receiver) = bounded::<J>(job_queue_capacity.get());
        let worker_handles = (0..num_workers.get())
            .map(|worker_id| {
                let job_receiver = job_receiver.clone();
                let mut handle_job = make_job_handler.make_job_handler(worker_id);
                thread::Builder::new()
                    .name(format!("{thread_name_prefix}{worker_id:02}"))
                    .stack_size(WORKER_STACK_SIZE)
                    .spawn(move || {
                        while let Ok(job) = job_receiver.recv() {
                            handle_job(job);
                        }
                    })
            })
            .collect::<io::Result<_>>()?;
        Ok((PoolSender(job_sender), WorkerPool { worker_handles }))
    }
}

pub struct PoolSender<J>(Sender<J>);

impl<J> PoolSender<J> {
    /// Queues `job`, blocking while the queue is full.
    ///
    /// Panics if every worker has exited. Workers exit early only by panicking.
    pub fn send(&self, job: J) {
        self.0
            .send(job)
            .expect("worker threads exited unexpectedly");
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::{collections::HashSet, sync::mpsc},
    };

    const NUM_WORKERS: NonZeroUsize = NonZeroUsize::new(4).unwrap();

    type Report = (usize, usize, Option<String>);

    fn report_job_handler(
        result_sender: mpsc::Sender<Report>,
    ) -> impl Fn(usize) -> Box<dyn FnMut(usize) + Send> {
        move |worker_id| {
            let result_sender = result_sender.clone();
            Box::new(move |index| {
                let thread_name = thread::current().name().map(str::to_string);
                result_sender.send((index, worker_id, thread_name)).unwrap();
            })
        }
    }

    fn panic_job(_job: ()) {
        panic!("worker job panicked");
    }

    #[test]
    fn test_worker_pool_runs_every_job() {
        const NUM_JOBS: usize = 1000;
        let (result_sender, result_receiver) = mpsc::channel();
        let (sender, pool) = WorkerPool::builder("solTestPool", NUM_WORKERS)
            .job_queue_capacity(NonZeroUsize::new(8).unwrap())
            .spawn(report_job_handler(result_sender))
            .unwrap();
        assert_eq!(pool.num_workers(), NUM_WORKERS.get());
        for index in 0..NUM_JOBS {
            sender.send(index);
        }
        drop(sender);
        pool.join().unwrap();

        let results: Vec<_> = result_receiver.into_iter().collect();
        let indices: HashSet<_> = results.iter().map(|(index, ..)| *index).collect();
        assert_eq!(indices, (0..NUM_JOBS).collect());
        let worker_ids: HashSet<_> = results.iter().map(|(_, worker_id, _)| *worker_id).collect();
        assert!(worker_ids.len() <= NUM_WORKERS.get());
        for (_, worker_id, thread_name) in results {
            assert_eq!(thread_name, Some(format!("solTestPool{worker_id:02}")));
        }
    }

    #[test]
    fn test_worker_pool_join_reports_panic() {
        let (sender, pool) = WorkerPool::builder("solTestPool", NonZeroUsize::MIN)
            .spawn(|_| panic_job)
            .unwrap();
        sender.send(());
        drop(sender);
        let payload = pool.join().unwrap_err();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"worker job panicked"));
    }

    #[test]
    fn test_worker_pool_drop_finishes_queued_jobs() {
        const NUM_JOBS: usize = 10;
        let (result_sender, result_receiver) = mpsc::channel();
        let (sender, pool) = WorkerPool::builder("solTestPool", NonZeroUsize::MIN)
            .job_queue_capacity(NonZeroUsize::new(NUM_JOBS).unwrap())
            .spawn(report_job_handler(result_sender))
            .unwrap();
        for index in 0..NUM_JOBS {
            sender.send(index);
        }
        drop(sender);
        drop(pool);
        assert_eq!(result_receiver.try_iter().count(), NUM_JOBS);
    }

    #[test]
    fn test_worker_pool_drop_survives_worker_panic() {
        let (sender, pool) = WorkerPool::builder("solTestPool", NonZeroUsize::MIN)
            .spawn(|_| panic_job)
            .unwrap();
        sender.send(());
        drop(sender);
        drop(pool);
    }

    #[test]
    fn test_worker_pool_drop_during_panic_detaches_workers() {
        let (sender, pool) = WorkerPool::builder("solTestPool", NonZeroUsize::MIN)
            .spawn(|_| |()| {})
            .unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _pool = pool;
            panic!("owner panicked");
        }));
        assert!(result.is_err());
        drop(sender);
    }
}
