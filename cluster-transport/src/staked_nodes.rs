// Total stake and nodes => stake map
use {
    solana_pubkey::Pubkey,
    std::{collections::HashMap, sync::Arc},
};

#[derive(Default)]
pub struct StakedNodes {
    stakes: Arc<HashMap<Pubkey, u64>>,
    overrides: HashMap<Pubkey, u64>,
    total_stake: u64,
}

impl StakedNodes {
    fn calculate_total_stake(
        stakes: &HashMap<Pubkey, u64>,
        overrides: &HashMap<Pubkey, u64>,
    ) -> u64 {
        stakes
            .iter()
            .filter(|(pubkey, _)| !overrides.contains_key(pubkey))
            .map(|(_, &stake)| stake)
            .chain(overrides.values().copied())
            .sum()
    }

    pub fn new(stakes: Arc<HashMap<Pubkey, u64>>, overrides: HashMap<Pubkey, u64>) -> Self {
        let total_stake = Self::calculate_total_stake(&stakes, &overrides);
        Self {
            stakes,
            overrides,
            total_stake,
        }
    }

    pub fn get_node_stake(&self, pubkey: &Pubkey) -> Option<u64> {
        self.overrides
            .get(pubkey)
            .or_else(|| self.stakes.get(pubkey))
            .filter(|&&stake| stake > 0)
            .copied()
    }

    #[inline]
    pub fn total_stake(&self) -> u64 {
        self.total_stake
    }
}
