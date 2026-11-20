use {
    agave_votor::commitment::{CommitmentAggregationData, CommitmentType},
    crossbeam_channel::{RecvTimeoutError, Sender, bounded},
    solana_clock::Slot,
    solana_measure::measure::Measure,
    solana_metrics::datapoint_info,
    solana_rpc::rpc_subscriptions::RpcSubscriptions,
    solana_runtime::{
        bank::BankId,
        commitment::{BlockCommitmentCache, CommitmentSlots},
    },
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
                        data.bank_id,
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
    bank_id: BankId,
) -> CommitmentSlots {
    let mut cache = block_commitment_cache.write().unwrap();
    match update_type {
        CommitmentType::Notarize => cache.set_slot(slot, bank_id),
        CommitmentType::Rooted => {
            if slot >= cache.slot() {
                cache.set_slot(slot, bank_id);
            }
            // Alpenglow has one finalized/rooted commitment level.
            cache.set_highest_confirmed_slot(slot);
            cache.set_root(slot);
            cache.set_highest_super_majority_root(slot);
        }
    }
    cache.commitment_slots()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alpenglow_update_commitment_cache_tracks_processed_bank_id() {
        let block_commitment_cache = RwLock::new(BlockCommitmentCache::new_for_tests());
        let processed_bank_id = BankId::new(42);
        update_commitment_cache(
            &block_commitment_cache,
            CommitmentType::Notarize,
            3,
            processed_bank_id,
        );

        {
            let cache = block_commitment_cache.read().unwrap();
            assert_eq!(cache.slot(), 3);
            assert_eq!(cache.bank_id(), processed_bank_id);
        }

        for (slot, bank_id, expected_slot, expected_bank_id) in [
            (2, BankId::new(43), 3, processed_bank_id),
            (3, BankId::new(44), 3, BankId::new(44)),
            (4, BankId::new(45), 4, BankId::new(45)),
        ] {
            update_commitment_cache(
                &block_commitment_cache,
                CommitmentType::Rooted,
                slot,
                bank_id,
            );
            let cache = block_commitment_cache.read().unwrap();
            assert_eq!(cache.slot(), expected_slot);
            assert_eq!(cache.bank_id(), expected_bank_id);
            assert_eq!(cache.highest_confirmed_slot(), slot);
            assert_eq!(cache.root(), slot);
            assert_eq!(cache.highest_super_majority_root(), slot);
        }
    }
}
