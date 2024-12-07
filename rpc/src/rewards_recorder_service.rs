//! The `RewardsRecorderService` is responsible for receiving rewards data and
//! persisting it into the `Blockstore`.

use {
    crossbeam_channel::TryRecvError,
    solana_ledger::{
        blockstore::{Blockstore, BlockstoreError},
        blockstore_processor::{RewardsMessage, RewardsRecorderReceiver},
    },
    solana_runtime::bank::KeyedRewardsAndNumPartitions,
    solana_transaction_status::{Reward, RewardsAndNumPartitions},
    std::{
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        },
        thread::{self, sleep, Builder, JoinHandle},
        time::Duration,
    },
};

pub struct RewardsRecorderService {
    thread_hdl: JoinHandle<()>,
}

// ReplayStage sends a new item to this service for every frozen Bank. Banks
// are frozen every 400ms at steady state so checking every 100ms is sufficient
const TRY_RECV_INTERVAL: Duration = Duration::from_millis(100);

impl RewardsRecorderService {
    pub fn new(
        rewards_receiver: RewardsRecorderReceiver,
        max_complete_rewards_slot: Arc<AtomicU64>,
        blockstore: Arc<Blockstore>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solRewardsWritr".to_string())
            .spawn(move || {
                info!("RewardsRecorderService has started");
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    let rewards = match rewards_receiver.try_recv() {
                        Ok(rewards) => rewards,
                        Err(TryRecvError::Empty) => {
                            sleep(TRY_RECV_INTERVAL);
                            continue;
                        }
                        Err(err @ TryRecvError::Disconnected) => {
                            info!("RewardsRecorderService is stopping because: {err}");
                            break;
                        }
                    };

                    if let Err(err) =
                        Self::write_rewards(rewards, &max_complete_rewards_slot, &blockstore)
                    {
                        error!("RewardsRecorderService is stopping because: {err}");
                        // Set the exit flag to allow other services to gracefully stop
                        exit.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                info!("RewardsRecorderService has stopped");
            })
            .unwrap();
        Self { thread_hdl }
    }

    fn write_rewards(
        rewards: RewardsMessage,
        max_complete_rewards_slot: &Arc<AtomicU64>,
        blockstore: &Blockstore,
    ) -> Result<(), BlockstoreError> {
        match rewards {
            RewardsMessage::Batch((
                slot,
                KeyedRewardsAndNumPartitions {
                    keyed_rewards: rewards,
                    num_partitions,
                },
            )) => {
                let rpc_rewards = rewards
                    .into_iter()
                    .map(|(pubkey, reward_info)| Reward {
                        pubkey: pubkey.to_string(),
                        lamports: reward_info.lamports,
                        post_balance: reward_info.post_balance,
                        reward_type: Some(reward_info.reward_type),
                        commission: reward_info.commission,
                    })
                    .collect();

                blockstore.write_rewards(
                    slot,
                    RewardsAndNumPartitions {
                        rewards: rpc_rewards,
                        num_partitions,
                    },
                )?;
            }
            RewardsMessage::Complete(slot) => {
                max_complete_rewards_slot.fetch_max(slot, Ordering::SeqCst);
            }
        }

        Ok(())
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}
