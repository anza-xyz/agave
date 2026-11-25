use {
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc},
};

pub trait EpochSpecs: Send + Sync {
    fn current_epoch_staked_nodes(&mut self) -> Arc<HashMap<Pubkey, /*stake:*/ u64>>;
    fn clone_box(&self) -> Box<dyn EpochSpecs>;
}
