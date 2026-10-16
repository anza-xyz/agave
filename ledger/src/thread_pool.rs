use {
    agave_wake_channel::{Sender, bounded},
    crossbeam_channel::TrySendError,
    log::error,
    std::{
        mem,
        thread::{self, JoinHandle},
    },
};

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

    pub(crate) fn try_send(&self, job: J) -> Result<(), TrySendError<J>> {
        self.job_sender.try_send(job)
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        std::sync::{Arc, Barrier, mpsc},
    };

    struct TestJob {
        started_sender: Option<mpsc::Sender<()>>,
        release: Option<Arc<Barrier>>,
    }

    impl WorkerJob for TestJob {
        fn run(self) {
            if let Some(started_sender) = self.started_sender {
                started_sender.send(()).unwrap();
            }
            if let Some(release) = self.release {
                release.wait();
            }
        }
    }

    #[test]
    fn test_try_send_returns_error_when_queue_is_full() {
        let pool = WorkerPool::new("testWorker", 1, 1);
        let (started_sender, started_receiver) = mpsc::channel();
        let release = Arc::new(Barrier::new(2));
        pool.send(TestJob {
            started_sender: Some(started_sender),
            release: Some(Arc::clone(&release)),
        });
        started_receiver.recv().unwrap();

        pool.try_send(TestJob {
            started_sender: None,
            release: None,
        })
        .unwrap();
        assert!(matches!(
            pool.try_send(TestJob {
                started_sender: None,
                release: None,
            }),
            Err(TrySendError::Full(_))
        ));

        release.wait();
    }
}
