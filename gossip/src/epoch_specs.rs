use {
    solana_clock::Slot,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc},
};

pub trait EpochSpecs: Send + Sync {
    fn current_epoch_staked_nodes(&mut self) -> Arc<HashMap<Pubkey, /*stake:*/ u64>>;
    fn epoch_slots(&mut self) -> u64;
    /// Returns true if shreds of `slot` must carry the Merkle proof size
    /// implied by a 32:32 FEC set. Every node has to agree on this predicate:
    /// it decides whether a duplicate proof is admitted.
    fn enforce_correct_proof_size(&mut self, slot: Slot) -> bool;
    fn clone_box(&self) -> Box<dyn EpochSpecs>;
}

#[cfg(feature = "dev-context-only-utils")]
#[derive(Clone)]
pub struct TestEpochSpecs {
    pub staked_nodes: Arc<HashMap<Pubkey, u64>>,
    pub slots_in_epoch: u64,
    pub enforce_correct_proof_size: bool,
}

#[cfg(feature = "dev-context-only-utils")]
impl EpochSpecs for TestEpochSpecs {
    fn current_epoch_staked_nodes(&mut self) -> Arc<HashMap<Pubkey, u64>> {
        Arc::clone(&self.staked_nodes)
    }
    fn epoch_slots(&mut self) -> u64 {
        self.slots_in_epoch
    }
    fn enforce_correct_proof_size(&mut self, _slot: Slot) -> bool {
        self.enforce_correct_proof_size
    }
    fn clone_box(&self) -> Box<dyn EpochSpecs> {
        Box::new(self.clone())
    }
}
