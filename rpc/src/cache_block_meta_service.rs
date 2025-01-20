//! The `CacheBlockMetaService` is responsible for persisting block metadata
//! from banks into the `Blockstore`

pub use solana_ledger::blockstore_processor::CacheBlockMetaSender;
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

pub type CacheBlockMetaReceiver = Receiver<Arc<Bank>>;

pub struct CacheBlockMetaService {
    thread_hdl: JoinHandle<()>,
}

impl CacheBlockMetaService {
    pub fn new(
        cache_block_meta_receiver: CacheBlockMetaReceiver,
        blockstore: Arc<Blockstore>,
        max_complete_rewards_slot: Arc<AtomicU64>,
        exit: Arc<AtomicBool>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solCacheBlkTime".to_string())
            .spawn(move || {
                info!("CacheBlockMetaService has started");
                loop {
                    if exit.load(Ordering::Relaxed) {
                        break;
                    }

                    let bank = match cache_block_meta_receiver.recv_timeout(Duration::from_secs(1))
                    {
                        Ok(bank) => bank,
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(err @ RecvTimeoutError::Disconnected) => {
                            info!("CacheBlockMetaService is stopping because: {err}");
                            break;
                        }
                    };

                    if let Err(err) =
                        Self::cache_block_meta(&bank, &blockstore, &max_complete_rewards_slot)
                    {
                        error!("CacheBlockMetaService is stopping because: {err}");
                        // Set the exit flag to allow other services to gracefully stop
                        exit.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                info!("CacheBlockMetaService has stopped");
            })
            .unwrap();
        Self { thread_hdl }
    }

    fn cache_block_meta(
        bank: &Bank,
        blockstore: &Blockstore,
        max_complete_rewards_slot: &Arc<AtomicU64>,
    ) -> Result<(), BlockstoreError> {
        let slot = bank.slot();

        blockstore.cache_block_time(slot, bank.clock().unix_timestamp)?;
        blockstore.cache_block_height(slot, bank.block_height())?;

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
