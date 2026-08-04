use {
    solana_pubkey::{Pubkey, PubkeyHasherBuilder},
    std::{collections::HashMap, sync::Arc, time::Duration},
};

pub type StakedNodesHashMap<S = PubkeyHasherBuilder> = HashMap<Pubkey, u64, S>;

pub trait EpochSpecs: Send + Sync {
    fn current_epoch_staked_nodes(&mut self) -> Arc<StakedNodesHashMap>;
    fn epoch_duration(&mut self) -> Duration;
    fn epoch_slots(&mut self) -> u64;
    fn clone_box(&self) -> Box<dyn EpochSpecs>;
}

#[cfg(feature = "dev-context-only-utils")]
#[derive(Clone)]
pub struct TestEpochSpecs {
    pub staked_nodes: Arc<StakedNodesHashMap>,
    pub slots_in_epoch: u64,
    pub epoch_duration: Duration,
}

#[cfg(feature = "dev-context-only-utils")]
impl EpochSpecs for TestEpochSpecs {
    fn current_epoch_staked_nodes(&mut self) -> Arc<StakedNodesHashMap> {
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
