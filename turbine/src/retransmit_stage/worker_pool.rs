use {
    agave_wake_channel::{Receiver, Sender, bounded},
    log::error,
    solana_ledger::shred::Payload,
    solana_streamer::evicting_sender::EvictingSender,
    std::{
        sync::Arc,
        thread::{self, JoinHandle},
    },
};

pub type ShredBatch = Vec<Payload>;
pub type RetransmitSender = EvictingSender<ShredBatch, Sender<ShredBatch>, Receiver<ShredBatch>>;

pub(super) struct WorkerPool {
    worker_handles: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    pub(super) fn build<F>(
        thread_name_prefix: &str,
        num_workers: usize,
        job_queue_capacity: usize,
        run_batch: F,
    ) -> (RetransmitSender, Self)
    where
        F: Fn(ShredBatch, usize) + Send + Sync + 'static,
    {
        assert_ne!(num_workers, 0, "worker pool must have at least one worker");
        let run_batch = Arc::new(run_batch);
        let (job_sender, job_receiver) = bounded::<ShredBatch>(job_queue_capacity);
        let worker_handles = (0..num_workers)
            .map(|worker_id| {
                let job_receiver = job_receiver.clone();
                let run_batch = Arc::clone(&run_batch);
                thread::Builder::new()
                    .name(format!("{thread_name_prefix}{worker_id:02}"))
                    .stack_size(2 * 1024 * 1024)
                    .spawn(move || {
                        while let Ok(shreds) = job_receiver.recv() {
                            run_batch(shreds, worker_id);
                        }
                    })
                    .expect("failed to spawn worker thread")
            })
            .collect();
        (
            RetransmitSender::new(job_sender, job_receiver),
            Self { worker_handles },
        )
    }

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

        result
    }
}
