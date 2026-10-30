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
    agave_wake_channel::{Sender, TrySendError, bounded},
    std::{
        io,
        num::NonZeroUsize,
        panic,
        thread::{self, JoinHandle},
    },
};

mod results;

pub use results::{ResultRecvError, ResultWorkerPool};

/// Byte length limit of a Linux thread name. Linux truncates longer names.
pub const MAX_THREAD_NAME_LEN: usize = 15;

/// Builds the job handler of each worker. A worker consumes jobs `J` and generates outputs `O`
/// through its handler. The handler also holds the state of that worker.
///
/// Implemented for closures `Fn(worker_id) -> impl FnMut(job) -> output`.
///
/// [`WorkerPool::spawn`] only takes handlers without an output, `O = ()`. Outputs reach the caller
/// only through [`ResultWorkerPool`].
pub trait MakeJobHandler<J, O = ()> {
    type Handler: FnMut(J) -> O + Send + 'static;

    /// Builds the handler of the worker with `worker_id` in `0..num_workers`.
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

#[derive(Clone, Debug)]
pub struct WorkerPoolConfig {
    /// Names the workers `{thread_name_prefix}{worker_id}`, with the id zero-padded to the digits of
    /// the largest id. Each name must fit in the [`MAX_THREAD_NAME_LEN`] bytes of a Linux thread
    /// name.
    pub thread_name_prefix: &'static str,
    pub num_workers: NonZeroUsize,
    /// Number of jobs the queue holds before [`WorkerPool::send`] blocks.
    pub job_queue_capacity: NonZeroUsize,
    /// Stack size of each worker in bytes. `None` keeps the default of [`std::thread`].
    pub stack_size: Option<usize>,
}

impl WorkerPoolConfig {
    /// Returns the config with `num_workers` workers.
    ///
    /// The job queue can buffer up to `num_workers` jobs. Each worker thread is created with the
    /// default stack size of [`std::thread`].
    pub fn new(thread_name_prefix: &'static str, num_workers: NonZeroUsize) -> Self {
        Self {
            thread_name_prefix,
            num_workers,
            job_queue_capacity: num_workers,
            stack_size: None,
        }
    }
}

/// Pool of worker threads draining one queue of jobs `J`.
///
/// Dropping the pool closes the queue, waits for the workers to finish the queued jobs and resumes
/// the panic of the first worker that panicked. A drop during a panic detaches the workers instead.
pub struct WorkerPool<J> {
    job_sender: Sender<J>,
    // Declared after `job_sender`, so the queue closes before the workers join on drop.
    workers: Workers,
}

impl<J: Send + 'static> WorkerPool<J> {
    /// Spawns the workers. Each worker runs its own handler from `make_job_handler` to process jobs.
    ///
    /// Returns the error of the first thread that fails to spawn.
    ///
    /// Panics if a worker name exceeds [`MAX_THREAD_NAME_LEN`].
    pub fn spawn(
        config: WorkerPoolConfig,
        make_job_handler: impl MakeJobHandler<J>,
    ) -> io::Result<Self> {
        let WorkerPoolConfig {
            thread_name_prefix,
            num_workers,
            job_queue_capacity,
            stack_size,
        } = config;
        let largest_id = num_workers.get().saturating_sub(1);
        let id_width = largest_id.to_string().len();
        let longest_thread_name = format!("{thread_name_prefix}{largest_id}");
        assert!(
            longest_thread_name.len() <= MAX_THREAD_NAME_LEN,
            "thread name {longest_thread_name} exceeds {MAX_THREAD_NAME_LEN} bytes",
        );
        let (job_sender, job_receiver) = bounded::<J>(job_queue_capacity.get());
        let worker_handles = (0..num_workers.get())
            .map(|worker_id| {
                let job_receiver = job_receiver.clone();
                let mut handle_job = make_job_handler.make_job_handler(worker_id);
                let mut builder = thread::Builder::new()
                    .name(format!("{thread_name_prefix}{worker_id:0id_width$}"));
                if let Some(stack_size) = stack_size {
                    builder = builder.stack_size(stack_size);
                }
                builder.spawn(move || {
                    while let Ok(job) = job_receiver.recv() {
                        handle_job(job);
                    }
                })
            })
            // On an error, `job_sender` drops and closes the queue, so the spawned workers exit.
            .collect::<io::Result<_>>()?;
        Ok(Self {
            job_sender,
            workers: Workers(worker_handles),
        })
    }
}

impl<J> WorkerPool<J> {
    /// Queues `job`, blocking while the queue is full.
    ///
    /// Panics if every worker has exited. Workers exit early only by panicking.
    pub fn send(&self, job: J) {
        self.job_sender
            .send(job)
            .expect("worker threads exited unexpectedly");
    }

    /// Queues `job` without blocking.
    ///
    /// Returns the job if the queue is full or every worker has exited.
    pub fn try_send(&self, job: J) -> Result<(), TrySendError<J>> {
        self.job_sender.try_send(job)
    }

    pub fn num_workers(&self) -> usize {
        self.workers.0.len()
    }

    /// Closes the queue and waits for the workers to finish the queued jobs.
    ///
    /// Returns the panic payload of the first worker that panicked.
    #[must_use = "join returns an error if a worker thread panicked"]
    pub fn join(self) -> thread::Result<()> {
        let Self {
            job_sender,
            mut workers,
        } = self;
        drop(job_sender);
        workers.join()
    }
}

struct Workers(Vec<JoinHandle<()>>);

impl Workers {
    fn join(&mut self) -> thread::Result<()> {
        let mut result = Ok(());
        for worker_handle in self.0.drain(..) {
            let worker_result = worker_handle.join();
            if result.is_ok() {
                result = worker_result;
            }
        }
        result
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        if !thread::panicking()
            && let Err(payload) = self.join()
        {
            panic::resume_unwind(payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::{collections::HashSet, hint::black_box, sync::mpsc},
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

    fn panic_on_true(should_panic: bool) {
        if should_panic {
            panic!("worker job panicked");
        }
    }

    #[test]
    fn test_worker_pool_runs_every_job() {
        const NUM_JOBS: usize = 1000;
        let (result_sender, result_receiver) = mpsc::channel();
        let config = WorkerPoolConfig {
            job_queue_capacity: NonZeroUsize::new(8).unwrap(),
            ..WorkerPoolConfig::new("solTestPool", NUM_WORKERS)
        };
        let pool = WorkerPool::spawn(config, report_job_handler(result_sender)).unwrap();
        assert_eq!(pool.num_workers(), NUM_WORKERS.get());
        for index in 0..NUM_JOBS {
            pool.send(index);
        }
        pool.join().unwrap();

        let results: Vec<_> = result_receiver.into_iter().collect();
        let indices: HashSet<_> = results.iter().map(|(index, ..)| *index).collect();
        assert_eq!(indices, (0..NUM_JOBS).collect());
        let worker_ids: HashSet<_> = results.iter().map(|(_, worker_id, _)| *worker_id).collect();
        assert!(worker_ids.len() <= NUM_WORKERS.get());
        for (_, worker_id, thread_name) in results {
            assert_eq!(thread_name, Some(format!("solTestPool{worker_id}")));
        }
    }

    #[test]
    fn test_worker_pool_pads_worker_ids_to_largest_id() {
        let (result_sender, result_receiver) = mpsc::channel();
        let num_workers = NonZeroUsize::new(11).unwrap();
        let config = WorkerPoolConfig::new("solTestPool", num_workers);
        let pool = WorkerPool::spawn(config, report_job_handler(result_sender)).unwrap();
        pool.send(0);
        pool.join().unwrap();
        let (_, worker_id, thread_name) = result_receiver.recv().unwrap();
        assert_eq!(thread_name, Some(format!("solTestPool{worker_id:02}")));
    }

    #[test]
    fn test_worker_pool_fits_long_prefix_to_few_workers() {
        let config = WorkerPoolConfig::new("solTestPoolMax", NonZeroUsize::new(10).unwrap());
        WorkerPool::spawn(config, |_| |()| {})
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "thread name solTestPoolMax10 exceeds 15 bytes")]
    fn test_worker_pool_rejects_truncated_thread_names() {
        let config = WorkerPoolConfig::new("solTestPoolMax", NonZeroUsize::new(11).unwrap());
        let _ = WorkerPool::spawn(config, |_| |()| {});
    }

    #[test]
    fn test_worker_pool_join_reports_panic() {
        let config = WorkerPoolConfig::new("solTestPool", NonZeroUsize::MIN);
        let pool = WorkerPool::spawn(config, |_| panic_job).unwrap();
        pool.send(());
        let payload = pool.join().unwrap_err();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"worker job panicked"));
    }

    #[test]
    fn test_worker_pool_sets_stack_size() {
        const STACK_SIZE: usize = 16 * 1024 * 1024;
        let config = WorkerPoolConfig {
            stack_size: Some(STACK_SIZE),
            ..WorkerPoolConfig::new("solTestPool", NonZeroUsize::MIN)
        };
        let pool = WorkerPool::spawn(config, |_| {
            |()| {
                let buffer = [0u8; STACK_SIZE / 4];
                black_box(&buffer);
            }
        })
        .unwrap();
        pool.send(());
        pool.join().unwrap();
    }

    #[test]
    fn test_worker_pool_drop_finishes_queued_jobs() {
        const NUM_JOBS: usize = 10;
        let (result_sender, result_receiver) = mpsc::channel();
        let config = WorkerPoolConfig {
            job_queue_capacity: NonZeroUsize::new(NUM_JOBS).unwrap(),
            ..WorkerPoolConfig::new("solTestPool", NonZeroUsize::MIN)
        };
        let pool = WorkerPool::spawn(config, report_job_handler(result_sender)).unwrap();
        for index in 0..NUM_JOBS {
            pool.send(index);
        }
        drop(pool);
        assert_eq!(result_receiver.try_iter().count(), NUM_JOBS);
    }

    #[test]
    #[should_panic(expected = "worker job panicked")]
    fn test_worker_pool_drop_resumes_panic_of_one_worker() {
        const NUM_JOBS: usize = 100;
        let config = WorkerPoolConfig::new("solTestPool", NUM_WORKERS);
        let pool = WorkerPool::spawn(config, |_| panic_on_true).unwrap();
        for index in 0..NUM_JOBS {
            pool.send(index == NUM_JOBS / 2);
        }
        drop(pool);
    }

    #[test]
    fn test_worker_pool_drop_during_panic_detaches_workers() {
        let config = WorkerPoolConfig::new("solTestPool", NonZeroUsize::MIN);
        let pool = WorkerPool::spawn(config, |_| {
            |release: mpsc::Receiver<()>| {
                let _ = release.recv();
            }
        })
        .unwrap();
        let (release_sender, release_receiver) = mpsc::channel();
        pool.send(release_receiver);
        let result = panic::catch_unwind(panic::AssertUnwindSafe(move || {
            let _pool = pool;
            panic!("owner panicked");
        }));
        assert!(result.is_err());
        release_sender.send(()).unwrap();
    }
}
