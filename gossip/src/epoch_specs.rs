use {
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc, time::Duration},
};

pub trait EpochSpecs: Send + Sync {
    fn epoch_current_staked_nodes(&self) -> Arc<HashMap<Pubkey, /*stake:*/ u64>>;
    fn epoch_duration(&self) -> Duration;
    fn epoch_slots(&self) -> u64;
}
