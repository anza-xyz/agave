use {
    ahash::AHasher,
    solana_pubkey::Pubkey,
    solana_runtime::bank_forks::BankForks,
    solana_streamer::streamer::StakedNodes,
    std::{
        collections::HashMap,
        hash::Hasher,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, RwLock,
        },
        thread::{self, Builder, JoinHandle},
        time::Duration,
    },
};

const STAKE_REFRESH_CYCLE: Duration = Duration::from_secs(5);

pub struct StakedNodesUpdaterService {
    thread_hdl: JoinHandle<()>,
}

fn hash_overrides(staked_nodes_overrides: &HashMap<Pubkey, u64>) -> Option<u64> {
    if staked_nodes_overrides.is_empty() {
        None
    } else {
        let mut hasher = AHasher::default();
        for (pubkey, &stake) in staked_nodes_overrides {
            hasher.write(pubkey.as_array());
            hasher.write_u64(stake);
        }
        Some(hasher.finish())
    }
}

impl StakedNodesUpdaterService {
    pub fn new(
        exit: Arc<AtomicBool>,
        bank_forks: Arc<RwLock<BankForks>>,
        staked_nodes: Arc<RwLock<StakedNodes>>,
        staked_nodes_overrides: Arc<RwLock<HashMap<Pubkey, u64>>>,
    ) -> Self {
        let thread_hdl = Builder::new()
            .name("solStakedNodeUd".to_string())
            .spawn(move || {
                let mut prev_epoch = None;
                let mut prev_overrides_hash =
                    hash_overrides(&staked_nodes_overrides.read().unwrap());
                while !exit.load(Ordering::Relaxed) {
                    let root_bank = bank_forks.read().unwrap().root_bank();
                    let overrides = staked_nodes_overrides.read().unwrap().clone();
                    let new_overrides_hash = hash_overrides(&overrides);
                    if Some(root_bank.epoch()) != prev_epoch
                        || prev_overrides_hash != new_overrides_hash
                    {
                        let stakes = root_bank.current_epoch_staked_nodes();
                        *staked_nodes.write().unwrap() = StakedNodes::new(stakes, overrides);
                        prev_epoch = Some(root_bank.epoch());
                        prev_overrides_hash = new_overrides_hash;
                    }
                    std::thread::sleep(STAKE_REFRESH_CYCLE);
                }
            })
            .unwrap();

        Self { thread_hdl }
    }

    pub fn join(self) -> thread::Result<()> {
        self.thread_hdl.join()
    }
}
