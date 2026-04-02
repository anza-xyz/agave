use {
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc, time::Duration},
};

pub trait EpochSpecs: Send + Sync {
    fn epoch_current_staked_nodes(&mut self) -> Arc<HashMap<Pubkey, /*stake:*/ u64>>;
    fn epoch_duration(&mut self) -> Duration;
    fn epoch_slots(&mut self) -> u64;
    fn clone_box(&self) -> Box<dyn EpochSpecs>;
}
