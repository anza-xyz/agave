use {
    crate::{MakeJobHandler, PoolSender, WorkerPool, WorkerPoolBuilder},
    agave_wake_channel::{Receiver, Sender, bounded},
    std::{io, num::NonZeroUsize, thread},
};

impl WorkerPoolBuilder {
    /// Same as [`WorkerPoolBuilder::spawn`], plus a queue of `result_queue_capacity` outputs, one
    /// per job, which the handler returns.
    ///
    /// A worker that panics reports it to the [`ResultReceiver`] in place of its job's output.
    pub fn spawn_with_results<J: Send + 'static, O: Send + 'static>(
        self,
        result_queue_capacity: NonZeroUsize,
        make_job_handler: impl MakeJobHandler<J, O>,
    ) -> io::Result<(PoolSender<J>, ResultReceiver<O>, WorkerPool)> {
        let (result_sender, result_receiver) = bounded(result_queue_capacity.get());
        let result_sender = ResultSender(result_sender);
        let (job_sender, workers) = self.spawn(|worker_id| {
            let result_sender = result_sender.clone();
            let mut handle_job = make_job_handler.make_job_handler(worker_id);
            move |job: J| result_sender.send(handle_job(job))
        })?;
        Ok((job_sender, ResultReceiver(result_receiver), workers))
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ResultRecvError {
    /// A worker panicked while running a job.
    WorkerPanicked,
    /// Every worker has exited and the queue is empty.
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

pub struct ResultReceiver<T>(Receiver<Option<T>>);

impl<T> ResultReceiver<T> {
    /// Blocks until a job output or a panic report arrives.
    pub fn recv(&self) -> Result<T, ResultRecvError> {
        match self.0.recv() {
            Ok(Some(output)) => Ok(output),
            Ok(None) => Err(ResultRecvError::WorkerPanicked),
            Err(_) => Err(ResultRecvError::Disconnected),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo(value: Option<usize>) -> usize {
        value.expect("missing value")
    }

    #[test]
    fn test_worker_pool_collects_results() {
        const NUM_JOBS: usize = 16;
        let capacity = NonZeroUsize::new(NUM_JOBS).unwrap();
        let (job_sender, result_receiver, workers) =
            WorkerPool::builder("solTestPool", NonZeroUsize::new(4).unwrap())
                .job_queue_capacity(capacity)
                .spawn_with_results(capacity, |_| echo)
                .unwrap();
        for value in 0..NUM_JOBS {
            job_sender.send(Some(value));
        }
        let mut results: Vec<_> = (0..NUM_JOBS)
            .map(|_| result_receiver.recv().unwrap())
            .collect();
        results.sort_unstable();
        assert_eq!(results, (0..NUM_JOBS).collect::<Vec<_>>());
        drop(job_sender);
        workers.join().unwrap();
        assert_eq!(result_receiver.recv(), Err(ResultRecvError::Disconnected));
    }

    #[test]
    fn test_worker_pool_reports_worker_panic() {
        let (job_sender, result_receiver, workers) =
            WorkerPool::builder("solTestPool", NonZeroUsize::new(2).unwrap())
                .job_queue_capacity(NonZeroUsize::MIN)
                .spawn_with_results(NonZeroUsize::MIN, |_| echo)
                .unwrap();
        job_sender.send(None);
        assert_eq!(result_receiver.recv(), Err(ResultRecvError::WorkerPanicked));
        job_sender.send(Some(7));
        assert_eq!(result_receiver.recv(), Ok(7));
        drop(job_sender);
        assert!(workers.join().is_err());
    }

    #[test]
    fn test_worker_pool_keeps_state_per_worker() {
        const NUM_WORKERS: NonZeroUsize = NonZeroUsize::new(3).unwrap();
        const NUM_JOBS: usize = 30;
        let capacity = NonZeroUsize::new(NUM_JOBS).unwrap();
        let (job_sender, result_receiver, workers) =
            WorkerPool::builder("solTestPool", NUM_WORKERS)
                .job_queue_capacity(capacity)
                .spawn_with_results(capacity, |worker_id| {
                    let mut num_jobs_run = 0;
                    move |()| {
                        num_jobs_run += 1;
                        (worker_id, num_jobs_run)
                    }
                })
                .unwrap();
        for _ in 0..NUM_JOBS {
            job_sender.send(());
        }
        let mut num_jobs_run = [0; NUM_WORKERS.get()];
        for _ in 0..NUM_JOBS {
            let (worker_id, worker_num_jobs_run) = result_receiver.recv().unwrap();
            num_jobs_run[worker_id] += 1;
            assert_eq!(worker_num_jobs_run, num_jobs_run[worker_id]);
        }
        drop(job_sender);
        workers.join().unwrap();
    }
}
