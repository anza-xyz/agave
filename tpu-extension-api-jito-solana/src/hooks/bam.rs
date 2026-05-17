use {
    agave_banking_stage_ingress_types::BankingPacketReceiver,
    agave_tpu_extension_api::{
        BankingSchedulerMode, BankingStageContext, BankingWorkerPoolFactory, BatchCommitMode,
    },
    crossbeam_channel::RecvTimeoutError,
    std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, JoinHandle},
        time::Duration,
    },
};

/// Reference stand-in for Jito BAM's banking worker pool.
///
/// Production BAM owns an alternate non-vote scheduling path. The API point is
/// the important part here: Agave supplies cloned packet receivers and an exit
/// signal through `BankingStageContext`; Jito supplies the worker-pool factory.
pub struct BamWorkerPoolFactory {
    worker_count: usize,
    replace_internal_scheduler: bool,
}

impl BamWorkerPoolFactory {
    /// Create a pool with an explicit worker count.
    pub fn new(worker_count: usize) -> Self {
        Self {
            worker_count,
            replace_internal_scheduler: false,
        }
    }

    pub fn replacing_internal_scheduler(mut self) -> Self {
        self.replace_internal_scheduler = true;
        self
    }
}

impl Default for BamWorkerPoolFactory {
    /// Creates a pool sized to the machine's available parallelism.
    fn default() -> Self {
        let worker_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self::new(worker_count)
    }
}

impl BankingWorkerPoolFactory for BamWorkerPoolFactory {
    fn scheduler_mode(&self) -> BankingSchedulerMode {
        if self.replace_internal_scheduler {
            BankingSchedulerMode::ReplaceInternal
        } else {
            BankingSchedulerMode::KeepInternal
        }
    }

    fn spawn_threads(&self, context: &dyn BankingStageContext) -> Vec<JoinHandle<()>> {
        let exit = context.worker_exit_signal();
        let non_vote_receiver = context.non_vote_receiver();
        let commit_mode = context.commit_mode();
        (0..self.worker_count)
            .map(|index| {
                spawn_bam_worker(
                    index,
                    Arc::clone(&exit),
                    non_vote_receiver.clone(),
                    commit_mode,
                )
            })
            .collect()
    }
}

fn spawn_bam_worker(
    index: usize,
    exit: Arc<AtomicBool>,
    non_vote_receiver: BankingPacketReceiver,
    commit_mode: BatchCommitMode,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name(format!("jitoBamWorker{index:02}"))
        .spawn(move || {
            let _commit_mode = commit_mode;
            while !exit.load(Ordering::Acquire) {
                match non_vote_receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(_packet_batch) => {
                        // Reference stub: production BAM parses, schedules, executes,
                        // and replies with per-batch results here.
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
        })
        .expect("jitoBamWorker spawn failed")
}
