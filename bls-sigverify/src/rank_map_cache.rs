use {
    solana_clock::Epoch,
    solana_runtime::{bank::Bank, epoch_stakes::BLSPubkeyToRankMap},
    std::{
        collections::{HashMap, hash_map::Entry},
        sync::Arc,
    },
};

#[derive(Default)]
pub(crate) struct RankMapCache {
    map: HashMap<Epoch, Arc<BLSPubkeyToRankMap>>,
    last_checked_root_epoch: Epoch,
}

impl RankMapCache {
    pub(crate) fn get_rank_map(
        &mut self,
        root_bank: &Bank,
        epoch: Epoch,
    ) -> Option<Arc<BLSPubkeyToRankMap>> {
        match self.map.entry(epoch) {
            Entry::Occupied(entry) => Some(entry.get().clone()),
            Entry::Vacant(entry) => {
                let epoch_stakes = root_bank.epoch_stakes(epoch)?;
                let rank_map = epoch_stakes.bls_pubkey_to_rank_map().clone();
                Some(entry.insert(rank_map.clone()).clone())
            }
        }
    }

    pub(crate) fn purge(&mut self, root_epoch: Epoch) {
        if self.last_checked_root_epoch < root_epoch {
            self.last_checked_root_epoch = root_epoch;
            // Keeping previous epoch as we need to look up slots older than root_slot for rewards.
            self.map
                .retain(|epoch, _| *epoch >= root_epoch.saturating_sub(1));
        }
    }
}
