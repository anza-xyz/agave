use {
    solana_program::{clock::Epoch, epoch_schedule::EpochSchedule},
    solana_runtime::{bank::Bank, epoch_stakes::EpochStakes},
    solana_sdk::{clock::Slot, pubkey::Pubkey},
    std::{collections::HashMap, sync::Arc},
};

pub struct EpochStakesCache {
    epoch_stakes_map: HashMap<Epoch, EpochStakes>,
    epoch_schedule: EpochSchedule,
}

impl EpochStakesCache {
    // Right now the epoch_stakes for next Epoch is calculated at the beginning of the current epoch.
    // Also wen_restart only handles outages up to 9 hours, so we should have the epoch_stakes for
    // all slots involved. If some slot has no corresponding epoch_stakes, it will just be ignored
    // because it's too far in the future (any slot older than root will be ignored).
    pub(crate) fn new(root_bank: &Arc<Bank>) -> Self {
        Self {
            epoch_stakes_map: root_bank.epoch_stakes_map().clone(),
            epoch_schedule: root_bank.epoch_schedule().clone(),
        }
    }

    pub fn epoch(&self, slot: &Slot) -> Epoch {
        self.epoch_schedule.get_epoch(*slot)
    }

    pub fn node_stake(&self, slot: &Slot, id: &Pubkey) -> Option<u64> {
        self.node_stake_at_epoch(&self.epoch_schedule.get_epoch(*slot), id)
    }

    pub fn node_stake_at_epoch(&self, epoch: &Epoch, id: &Pubkey) -> Option<u64> {
        self.epoch_stakes_map.get(epoch).and_then(|epoch_stakes| {
            epoch_stakes
                .node_id_to_vote_accounts()
                .get(id)
                .map(|node| node.total_stake)
        })
    }

    pub fn total_stake(&self, slot: &Slot) -> Option<u64> {
        let epoch = self.epoch_schedule.get_epoch(*slot);
        self.epoch_stakes_map
            .get(&epoch)
            .map(|epoch_stakes| epoch_stakes.total_stake())
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_program::epoch_schedule::MINIMUM_SLOTS_PER_EPOCH,
        solana_runtime::genesis_utils::{
            create_genesis_config_with_vote_accounts, GenesisConfigInfo, ValidatorVoteKeypairs,
        },
        solana_sdk::signer::Signer,
    };

    const TOTAL_VALIDATOR_COUNT: usize = 10;

    #[test]
    fn test_epoch_stakes_cache() {
        let validator_voting_keypairs: Vec<_> = (0..TOTAL_VALIDATOR_COUNT)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect();
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config_with_vote_accounts(
            10_000,
            &validator_voting_keypairs,
            vec![100; validator_voting_keypairs.len()],
        );
        let (_, bank_forks) = Bank::new_with_bank_forks_for_tests(&genesis_config);
        let root_bank = bank_forks.read().unwrap().root_bank();

        let epoch_stakes_cache = EpochStakesCache::new(&root_bank);

        let node_pubkey = validator_voting_keypairs[0].node_keypair.pubkey();
        let random_pubkey = Pubkey::new_unique();
        assert_eq!(epoch_stakes_cache.epoch(&2), 0);
        assert_eq!(epoch_stakes_cache.node_stake(&2, &node_pubkey), Some(100));
        assert_eq!(epoch_stakes_cache.node_stake(&2, &random_pubkey), None);
        assert_eq!(
            epoch_stakes_cache.node_stake_at_epoch(&0, &node_pubkey),
            Some(100)
        );
        assert_eq!(
            epoch_stakes_cache.node_stake_at_epoch(&0, &random_pubkey),
            None
        );
        assert_eq!(epoch_stakes_cache.total_stake(&2), Some(1000));

        // The epoch stake for next epoch should exist.
        assert_eq!(epoch_stakes_cache.epoch(&MINIMUM_SLOTS_PER_EPOCH), 1);
        assert_eq!(
            epoch_stakes_cache.node_stake(&MINIMUM_SLOTS_PER_EPOCH, &node_pubkey),
            Some(100)
        );
        assert_eq!(
            epoch_stakes_cache.node_stake(&MINIMUM_SLOTS_PER_EPOCH, &random_pubkey),
            None
        );
        assert_eq!(
            epoch_stakes_cache.node_stake_at_epoch(&1, &node_pubkey),
            Some(100)
        );
        assert_eq!(
            epoch_stakes_cache.node_stake_at_epoch(&1, &random_pubkey),
            None
        );
        assert_eq!(
            epoch_stakes_cache.total_stake(&MINIMUM_SLOTS_PER_EPOCH),
            Some(1000)
        );

        // The epoch stake for epoch in distant future would not exist.
        let first_normal_slot = root_bank.epoch_schedule().first_normal_slot;
        assert_eq!(epoch_stakes_cache.epoch(&first_normal_slot), 14);
        assert_eq!(epoch_stakes_cache.total_stake(&first_normal_slot), None);
        assert_eq!(
            epoch_stakes_cache.node_stake_at_epoch(&14, &node_pubkey),
            None
        );
        assert_eq!(
            epoch_stakes_cache.node_stake(&first_normal_slot, &node_pubkey),
            None
        );
    }
}
