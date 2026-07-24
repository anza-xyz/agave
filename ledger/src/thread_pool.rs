use {
    crossbeam_channel::Sender,
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
        let (job_sender, job_receiver) = crossbeam_channel::bounded::<J>(job_queue_capacity);
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
        let (tmp, _) = crossbeam_channel::bounded(0);
        drop(mem::replace(&mut self.job_sender, tmp));
        for worker_handle in self.worker_handles.drain(..) {
            if let Err(err) = worker_handle.join() {
                error!("worker thread failed: {err:?}");
            }
        }
    }
}
