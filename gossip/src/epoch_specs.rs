use {
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc},
};

pub trait EpochSpecs: Send + Sync {
    fn current_epoch_staked_nodes(&mut self) -> Arc<HashMap<Pubkey, /*stake:*/ u64>>;
    fn clone_box(&self) -> Box<dyn EpochSpecs>;
}

#[cfg(feature = "dev-context-only-utils")]
#[derive(Clone)]
pub struct TestEpochSpecs {
    pub staked_nodes: Arc<HashMap<Pubkey, u64>>,
}

#[cfg(feature = "dev-context-only-utils")]
impl EpochSpecs for TestEpochSpecs {
    fn current_epoch_staked_nodes(&mut self) -> Arc<HashMap<Pubkey, u64>> {
        Arc::clone(&self.staked_nodes)
    }
    fn clone_box(&self) -> Box<dyn EpochSpecs> {
        Box::new(self.clone())
    }
}
