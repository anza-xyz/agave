use {
    crate::{MakeJobHandler, WorkerPool, WorkerPoolConfig},
    agave_wake_channel::{Receiver, Sender, bounded},
    std::{io, num::NonZeroUsize, thread},
    thiserror::Error,
};

/// [`WorkerPool`] that queues the output of each job for [`ResultWorkerPool::recv`].
///
/// A worker that panics reports it in place of the output of its job. Dropping the pool discards
/// the unread outputs before it waits for the workers.
pub struct ResultWorkerPool<J, O> {
    result_receiver: Receiver<Option<O>>,
    // Declared after `result_receiver`, so a worker blocked on a full result queue exits before the
    // workers join on drop.
    pool: WorkerPool<J>,
}

impl<J: Send + 'static, O: Send + 'static> ResultWorkerPool<J, O> {
    /// Same as [`WorkerPool::spawn`], plus a queue of `result_queue_capacity` outputs, one per job,
    /// which the handler returns.
    pub fn spawn(
        config: WorkerPoolConfig,
        result_queue_capacity: NonZeroUsize,
        make_job_handler: impl MakeJobHandler<J, O>,
    ) -> io::Result<Self> {
        let (result_sender, result_receiver) = bounded(result_queue_capacity.get());
        let result_sender = ResultSender(result_sender);
        let pool = WorkerPool::spawn(config, |worker_id| {
            let result_sender = result_sender.clone();
            let mut handle_job = make_job_handler.make_job_handler(worker_id);
            move |job: J| result_sender.send(handle_job(job))
        })?;
        Ok(Self {
            result_receiver,
            pool,
        })
    }
}

impl<J, O> ResultWorkerPool<J, O> {
    /// Same as [`WorkerPool::send`].
    pub fn send(&self, job: J) {
        self.pool.send(job);
    }

    /// Blocks until a job output or a panic report arrives.
    pub fn recv(&self) -> Result<O, ResultRecvError> {
        match self.result_receiver.recv() {
            Ok(Some(output)) => Ok(output),
            Ok(None) => Err(ResultRecvError::WorkerPanicked),
            Err(_) => Err(ResultRecvError::Disconnected),
        }
    }

    pub fn num_workers(&self) -> usize {
        self.pool.num_workers()
    }

    /// Same as [`WorkerPool::join`], after discarding the unread outputs.
    #[must_use = "join returns an error if a worker thread panicked"]
    pub fn join(self) -> thread::Result<()> {
        let Self {
            result_receiver,
            pool,
        } = self;
        drop(result_receiver);
        pool.join()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ResultRecvError {
    /// A worker panicked while running a job.
    #[error("worker panicked while running a job")]
    WorkerPanicked,
    /// Every worker has exited and the queue is empty.
    #[error("every worker exited")]
    Disconnected,
}

/// Sends a panic report when dropped during a panic, so the receiver never waits for the output of
/// a job that unwound.
struct ResultSender<T>(Sender<Option<T>>);

impl<T> ResultSender<T> {
    fn send(&self, output: T) {
        let _ = self.0.send(Some(output));
    }
}

impl<T> Clone for ResultSender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Drop for ResultSender<T> {
    fn drop(&mut self) {
        if thread::panicking() {
            let _ = self.0.send(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo_or_panic_on_none(value: Option<usize>) -> usize {
        value.expect("missing value")
    }

    #[test]
    fn test_worker_pool_collects_results_and_keeps_state_per_worker() {
        const NUM_WORKERS: NonZeroUsize = NonZeroUsize::new(3).unwrap();
        const NUM_JOBS: usize = 30;
        let capacity = NonZeroUsize::new(NUM_JOBS).unwrap();
        let config = WorkerPoolConfig {
            job_queue_capacity: capacity,
            ..WorkerPoolConfig::new("solTestPool", NUM_WORKERS)
        };
        let pool = ResultWorkerPool::spawn(config, capacity, |worker_id| {
            let mut num_jobs_run = 0;
            move |job: usize| {
                num_jobs_run += 1;
                (job, worker_id, num_jobs_run)
            }
        })
        .unwrap();
        for job in 0..NUM_JOBS {
            pool.send(job);
        }
        let mut jobs = Vec::with_capacity(NUM_JOBS);
        let mut num_jobs_run = [0; NUM_WORKERS.get()];
        for _ in 0..NUM_JOBS {
            let (job, worker_id, worker_num_jobs_run) = pool.recv().unwrap();
            jobs.push(job);
            num_jobs_run[worker_id] += 1;
            assert_eq!(worker_num_jobs_run, num_jobs_run[worker_id]);
        }
        jobs.sort_unstable();
        assert_eq!(jobs, (0..NUM_JOBS).collect::<Vec<_>>());
        pool.join().unwrap();
    }

    #[test]
    fn test_worker_pool_reports_worker_panics() {
        let config = WorkerPoolConfig {
            job_queue_capacity: NonZeroUsize::MIN,
            ..WorkerPoolConfig::new("solTestPool", NonZeroUsize::new(2).unwrap())
        };
        let pool =
            ResultWorkerPool::spawn(config, NonZeroUsize::MIN, |_| echo_or_panic_on_none).unwrap();
        pool.send(None);
        assert_eq!(pool.recv(), Err(ResultRecvError::WorkerPanicked));
        pool.send(Some(7));
        assert_eq!(pool.recv(), Ok(7));
        pool.send(None);
        assert_eq!(pool.recv(), Err(ResultRecvError::WorkerPanicked));
        assert_eq!(pool.recv(), Err(ResultRecvError::Disconnected));
        assert!(pool.join().is_err());
    }

    #[test]
    #[should_panic(expected = "missing value")]
    fn test_worker_pool_drop_discards_unread_results_and_resumes_worker_panic() {
        const NUM_JOBS: usize = 3;
        let config = WorkerPoolConfig::new("solTestPool", NonZeroUsize::MIN);
        let pool =
            ResultWorkerPool::spawn(config, NonZeroUsize::MIN, |_| echo_or_panic_on_none).unwrap();
        for value in 0..NUM_JOBS {
            pool.send((value != NUM_JOBS - 1).then_some(value));
        }
        drop(pool);
    }
}
