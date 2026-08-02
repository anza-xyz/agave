use {
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc, time::Duration},
};

pub trait EpochSpecs: Send + Sync {
    fn current_epoch_staked_nodes(&mut self) -> Arc<HashMap<Pubkey, /*stake:*/ u64>>;
    fn epoch_duration(&mut self) -> Duration;
    fn epoch_slots(&mut self) -> u64;
    fn clone_box(&self) -> Box<dyn EpochSpecs>;
}

#[cfg(feature = "dev-context-only-utils")]
#[derive(Clone)]
pub struct TestEpochSpecs {
    pub staked_nodes: Arc<HashMap<Pubkey, u64>>,
    pub slots_in_epoch: u64,
    pub epoch_duration: Duration,
}

#[cfg(feature = "dev-context-only-utils")]
impl EpochSpecs for TestEpochSpecs {
    fn current_epoch_staked_nodes(&mut self) -> Arc<HashMap<Pubkey, u64>> {
        Arc::clone(&self.staked_nodes)
    }
    fn epoch_duration(&mut self) -> Duration {
        self.epoch_duration
    }
    fn epoch_slots(&mut self) -> u64 {
        self.slots_in_epoch
    }
    fn clone_box(&self) -> Box<dyn EpochSpecs> {
        Box::new(self.clone())
    }
}
