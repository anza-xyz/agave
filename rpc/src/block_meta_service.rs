//! The `BlockMetaService` is responsible for persisting block metadata from
//! banks into the `Blockstore`

pub use solana_ledger::blockstore_processor::BlockMetaSender;
use {
    crossbeam_channel::{Receiver, RecvTimeoutError},
    solana_ledger::blockstore::{Blockstore, BlockstoreError},
    solana_runtime::bank::{Bank, KeyedRewardsAndNumPartitions},
    solana_transaction_status::{Reward, RewardsAndNumPartitions},
    std::{
        sync::{
            atomic::{AtomicBool, AtomicU64, Ordering},
            Arc,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

pub type BlockMetaReceiver = Receiver<Arc<Bank>>;

pub struct BlockMetaService {
    thread_hdl: JoinHandle<()>,
}

impl BlockMetaService {
    pub fn new(
        block_meta_receiver: BlockMetaReceiver,
        blockstore: Arc<Blockstore>,
        max_complete_rewards_slot: Arc<AtomicU64>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solBlockMeta".to_string())
            .spawn(move || {
                info!("BlockMetaService has started");
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    let bank = match block_meta_receiver.recv_timeout(Duration::from_secs(1)) {
                        Ok(bank) => bank,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(err @ RecvTimeoutError::Disconnected) => {
                            info!("BlockMetaService is stopping because: {err}");
                            break;
                        }
                    };

                    if let Err(err) =
                        Self::write_block_meta(&bank, &blockstore, &max_complete_rewards_slot)
                    {
                        error!("BlockMetaService is stopping because: {err}");
                        // Set the exit flag to allow other services to gracefully stop
                        exit.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                info!("BlockMetaService has stopped");
            })
            .unwrap();
        Self { thread_hdl }
    }

    fn write_block_meta(
        bank: &Bank,
        blockstore: &Blockstore,
        max_complete_rewards_slot: &Arc<AtomicU64>,
    ) -> Result<(), BlockstoreError> {
        let slot = bank.slot();

        blockstore.set_block_time(slot, bank.clock().unix_timestamp)?;
        blockstore.set_block_height(slot, bank.block_height())?;

        let rewards = bank.get_rewards_and_num_partitions();
        if rewards.should_record() {
            let KeyedRewardsAndNumPartitions {
                keyed_rewards,
                num_partitions,
            } = rewards;
            let rewards = keyed_rewards
                .into_iter()
                .map(|(pubkey, reward_info)| Reward {
                    pubkey: pubkey.to_string(),
                    lamports: reward_info.lamports,
                    post_balance: reward_info.post_balance,
                    reward_type: Some(reward_info.reward_type),
                    commission: reward_info.commission,
                })
                .collect();
            let blockstore_rewards = RewardsAndNumPartitions {
                rewards,
                num_partitions,
            };

            blockstore.write_rewards(slot, blockstore_rewards)?;
        }
        max_complete_rewards_slot.fetch_max(slot, Ordering::SeqCst);

        Ok(())
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}
