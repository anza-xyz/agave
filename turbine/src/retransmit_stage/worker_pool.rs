use {
    agave_wake_channel::{Sender, bounded},
    log::error,
    std::thread::{self, JoinHandle},
};

pub(super) trait WorkerJob: Send + 'static {
    fn run(self, worker_id: usize);
}

pub(super) struct WorkerPool {
    worker_handles: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    pub(super) fn build<J: WorkerJob>(
        thread_name_prefix: &str,
        num_workers: usize,
        job_queue_capacity: usize,
    ) -> (PoolSender<J>, Self) {
        assert_ne!(num_workers, 0, "worker pool must have at least one worker");
        let (job_sender, job_receiver) = bounded::<J>(job_queue_capacity);
        let worker_handles = (0..num_workers)
            .map(|worker_id| {
                let job_receiver = job_receiver.clone();
                thread::Builder::new()
                    .name(format!("{thread_name_prefix}{worker_id:02}"))
                    .stack_size(2 * 1024 * 1024)
                    .spawn(move || {
                        while let Ok(job) = job_receiver.recv() {
                            job.run(worker_id);
                        }
                    })
                    .expect("failed to spawn worker thread")
            })
            .collect();
        (PoolSender(job_sender), Self { worker_handles })
    }

    #[must_use = "join returns an error if a worker thread panicked"]
    pub(super) fn join(mut self) -> thread::Result<()> {
        let mut result = Ok(());

        for worker_handle in self.worker_handles.drain(..) {
            if let Err(err) = worker_handle.join() {
                error!("worker thread failed: {err:?}");
                if result.is_ok() {
                    result = Err(err);
                }
            }
        }
        // to panic in tests even when join result is ignored.
        debug_assert!(result.is_ok(), "retransmit worker thread panicked");
        result
    }
}

pub(super) struct PoolSender<J: WorkerJob>(Sender<J>);

impl<J: WorkerJob> PoolSender<J> {
    pub(super) fn send(&self, job: J) {
        self.0
            .send(job)
            .expect("worker threads exited unexpectedly");
    }
}
