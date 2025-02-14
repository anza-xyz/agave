use {
    itertools::Itertools,
    solana_pubkey::Pubkey,
    solana_sdk::clock::Epoch,
    std::{collections::HashMap, sync::Arc},
};

#[derive(Default, Debug, PartialEq, Eq, Clone)]
pub(super) struct LeaderSchedule {
    pub(super) slot_leaders: Vec<Pubkey>,
    // Inverted index from pubkeys to indices where they are the leader.
    pub(super) index: HashMap<Pubkey, Arc<Vec<usize>>>,
}

// Note: passing in zero stakers will cause a panic.
impl LeaderSchedule {
    pub(super) fn new(
        epoch_staked_nodes: &HashMap<Pubkey, u64>,
        epoch: Epoch,
        len: u64,
        repeat: u64,
    ) -> Self {
        let keyed_stakes: Vec<_> = epoch_staked_nodes
            .iter()
            .map(|(pubkey, stake)| (pubkey, *stake))
            .collect();
        let slot_leaders =
            super::LeaderSchedule::stake_weighted_slot_leaders(keyed_stakes, epoch, len, repeat);
        Self::new_from_schedule(slot_leaders)
    }

    pub(super) fn new_from_schedule(slot_leaders: Vec<Pubkey>) -> Self {
        Self {
            index: Self::index_from_slot_leaders(&slot_leaders),
            slot_leaders,
        }
    }

    fn index_from_slot_leaders(slot_leaders: &[Pubkey]) -> HashMap<Pubkey, Arc<Vec<usize>>> {
        slot_leaders
            .iter()
            .enumerate()
            .map(|(i, pk)| (*pk, i))
            .into_group_map()
            .into_iter()
            .map(|(k, v)| (k, Arc::new(v)))
            .collect()
    }
}
