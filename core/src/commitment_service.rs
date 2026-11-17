use {
    agave_votor::commitment::{CommitmentAggregationData, CommitmentType},
    crossbeam_channel::{RecvTimeoutError, Sender, bounded},
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_metrics::datapoint_info,
    solana_rpc::rpc_subscriptions::RpcSubscriptions,
    solana_runtime::commitment::{BlockCommitmentCache, CommitmentSlots},
    std::{
        sync::{
            Arc, RwLock,
            atomic::{AtomicBool, Ordering},
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

pub struct AggregateCommitmentService {
    thread: JoinHandle<()>,
}

impl AggregateCommitmentService {
    pub fn new(
        exit: Arc<AtomicBool>,
        block_commitment_cache: Arc<RwLock<BlockCommitmentCache>>,
        subscriptions: Option<Arc<RpcSubscriptions>>,
    ) -> (Sender<CommitmentAggregationData>, Self) {
        // Votor normally emits no more than two commitment updates per slot.
        // Keep a generous bound for catch-up and standstill recovery.
        let (sender, receiver) = bounded::<CommitmentAggregationData>(1_000);
        let thread = Builder::new()
            .name("solAggCommitSvc".to_string())
            .spawn(move || {
                while !exit.load(Ordering::Relaxed) {
                    let data = match receiver.recv_timeout(Duration::from_secs(1)) {
                        Ok(data) => receiver.try_iter().last().unwrap_or(data),
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => break,
                    };

                    let mut aggregate_commitment_time = Measure::start("aggregate-commitment-ms");
                    let commitment_slots = update_commitment_cache(
                        &block_commitment_cache,
                        data.commitment_type,
                        data.slot,
                    );
                    aggregate_commitment_time.stop();

                    datapoint_info!(
                        "block-commitment-cache",
                        (
                            "aggregate-commitment-ms",
                            aggregate_commitment_time.as_ms() as i64,
                            i64
                        ),
                        (
                            "highest-super-majority-root",
                            commitment_slots.highest_super_majority_root as i64,
                            i64
                        ),
                        (
                            "highest-confirmed-slot",
                            commitment_slots.highest_confirmed_slot as i64,
                            i64
                        ),
                    );

                    if let Some(subscriptions) = subscriptions.as_deref() {
                        subscriptions.notify_subscribers(commitment_slots);
                    }
                }
            })
            .unwrap();
        (sender, Self { thread })
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread.join()
    }
}

fn update_commitment_cache(
    block_commitment_cache: &RwLock<BlockCommitmentCache>,
    update_type: CommitmentType,
    slot: Slot,
) -> CommitmentSlots {
    let mut cache = block_commitment_cache.write().unwrap();
    match update_type {
        CommitmentType::Notarize => cache.set_slot(slot),
        CommitmentType::Rooted => {
            // Alpenglow has one finalized/rooted commitment level.
            cache.set_highest_confirmed_slot(slot);
            cache.set_root(slot);
            cache.set_highest_super_majority_root(slot);
        }
    }
    cache.commitment_slots()
}
