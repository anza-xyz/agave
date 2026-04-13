use {
    solana_clock::Epoch,
    solana_epoch_schedule::EpochSchedule,
    solana_gossip::epoch_specs::EpochSpecs as EpochSpecsTrait,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_forks::{BankForks, SharableBanks},
    },
    std::{
        collections::HashMap,
        sync::{Arc, RwLock},
        time::Duration,
    },
};

#[derive(Clone)]
struct EpochSpecsCache {
    epoch: Epoch,
    epoch_schedule: EpochSchedule,
    current_epoch_staked_nodes: Arc<HashMap<Pubkey, u64>>,
    epoch_duration: Duration,
    slots_in_epoch: u64,
}

#[derive(Clone)]
pub struct EpochSpecs {
    sharable_banks: SharableBanks,
    cache: EpochSpecsCache,
}
impl EpochSpecsTrait for EpochSpecs {
    fn current_epoch_staked_nodes(&mut self) -> Arc<HashMap<Pubkey, u64>> {
        let cache = &mut self.cache;
        Self::maybe_refresh_cache(cache, &self.sharable_banks);
        Arc::clone(&cache.current_epoch_staked_nodes)
    }

    fn epoch_duration(&mut self) -> Duration {
        let cache = &mut self.cache;
        Self::maybe_refresh_cache(cache, &self.sharable_banks);
        cache.epoch_duration
    }

    fn epoch_slots(&mut self) -> u64 {
        let cache = &mut self.cache;
        Self::maybe_refresh_cache(cache, &self.sharable_banks);
        cache.slots_in_epoch
    }

    fn clone_box(&self) -> Box<dyn EpochSpecsTrait> {
        Box::new(self.clone())
    }
}

impl EpochSpecs {
    fn maybe_refresh_cache(cache: &mut EpochSpecsCache, shareable_banks: &SharableBanks) {
        let root_bank = shareable_banks.root();
        if root_bank.epoch() == cache.epoch {
            return; // still the same epoch. nothing to update.
        }
        debug_assert_eq!(
            cache.epoch_schedule.get_epoch(root_bank.slot()),
            root_bank.epoch()
        );
        cache.epoch = root_bank.epoch();
        cache.epoch_schedule = root_bank.epoch_schedule().clone();
        cache.current_epoch_staked_nodes = root_bank.current_epoch_staked_nodes();
        cache.epoch_duration = get_epoch_duration(&root_bank);
        cache.slots_in_epoch = root_bank.get_slots_in_epoch(root_bank.epoch());
    }
}
impl From<Arc<RwLock<BankForks>>> for EpochSpecs {
    fn from(bank_forks: Arc<RwLock<BankForks>>) -> Self {
        let (sharable_banks, root_bank) = {
            let bank_forks = bank_forks.read().unwrap();
            let sharable_banks = bank_forks.sharable_banks();
            let root_bank = sharable_banks.root();
            (sharable_banks, root_bank)
        };
        Self {
            sharable_banks,
            cache: EpochSpecsCache {
                epoch: root_bank.epoch(),
                epoch_schedule: root_bank.epoch_schedule().clone(),
                current_epoch_staked_nodes: root_bank.current_epoch_staked_nodes(),
                epoch_duration: get_epoch_duration(&root_bank),
                slots_in_epoch: root_bank.get_slots_in_epoch(root_bank.epoch()),
            },
        }
    }
}

fn get_epoch_duration(bank: &Bank) -> Duration {
    let num_slots = bank.get_slots_in_epoch(bank.epoch());
    Duration::from_nanos_u128(num_slots as u128 * bank.ns_per_slot)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_runtime::{
            bank::SlotLeader,
            genesis_utils::{GenesisConfigInfo, create_genesis_config},
        },
    };

    #[test]
    fn test_get_epoch_duration() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let (mut bank, bank_forks) =
            Bank::new_for_tests(&genesis_config).wrap_with_bank_forks_for_tests();
        let epoch = 0;
        let num_slots = 32;
        assert_eq!(bank.epoch(), epoch);
        assert_eq!(bank.get_slots_in_epoch(epoch), num_slots);
        assert_eq!(
            get_epoch_duration(&bank),
            Duration::from_millis(num_slots * 400)
        );
        for slot in 1..32 {
            bank = Bank::new_from_parent_with_bank_forks(
                bank_forks.as_ref(),
                bank,
                SlotLeader::new_unique(),
                slot,
            );
            assert_eq!(bank.epoch(), epoch);
            assert_eq!(bank.get_slots_in_epoch(epoch), num_slots);
            assert_eq!(
                get_epoch_duration(&bank),
                Duration::from_millis(num_slots * 400)
            );
        }
        let epoch = 1;
        let num_slots = 64;
        for slot in 32..32 + num_slots {
            bank = Bank::new_from_parent_with_bank_forks(
                bank_forks.as_ref(),
                bank,
                SlotLeader::new_unique(),
                slot,
            );
            assert_eq!(bank.epoch(), epoch);
            assert_eq!(bank.get_slots_in_epoch(epoch), num_slots);
            assert_eq!(
                get_epoch_duration(&bank),
                Duration::from_millis(num_slots * 400)
            );
        }
        let epoch = 2;
        let num_slots = 128;
        for slot in 96..96 + num_slots {
            bank = Bank::new_from_parent_with_bank_forks(
                bank_forks.as_ref(),
                bank,
                SlotLeader::new_unique(),
                slot,
            );
            assert_eq!(bank.epoch(), epoch);
            assert_eq!(bank.get_slots_in_epoch(epoch), num_slots);
            assert_eq!(
                get_epoch_duration(&bank),
                Duration::from_millis(num_slots * 400)
            );
        }
    }

    fn verify_epoch_specs(epoch_specs: &mut EpochSpecs, root_bank: &Bank) {
        assert_eq!(
            // also triggers the cache refresh needed for the rest of the tests
            epoch_specs.current_epoch_staked_nodes(),
            root_bank.current_epoch_staked_nodes()
        );

        let cache = &epoch_specs.cache;
        assert_eq!(cache.epoch, root_bank.epoch());
        assert_eq!(cache.epoch_schedule, *root_bank.epoch_schedule());
        assert_eq!(cache.epoch_duration, get_epoch_duration(root_bank));
        assert_eq!(
            cache.slots_in_epoch,
            root_bank.get_slots_in_epoch(root_bank.epoch())
        );
    }

    #[test]
    fn test_epoch_specs_refresh() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut epoch_specs = EpochSpecs::from(bank_forks.clone());
        for slot in 1..100 {
            let bank = bank_forks.read().unwrap().get(slot - 1).unwrap();
            let bank = Bank::new_from_parent(bank, SlotLeader::new_unique(), slot);
            bank_forks.write().unwrap().insert(bank);
        }

        // root is still 0, epoch 0.
        let root_bank = bank_forks.read().unwrap().get(0).unwrap();
        assert_eq!(root_bank.epoch(), 0);
        assert_eq!(root_bank.slot(), 0);
        verify_epoch_specs(&mut epoch_specs, &root_bank);

        // root is updated but epoch still the same.
        bank_forks.write().unwrap().set_root(17, None, None);
        let root_bank = bank_forks.read().unwrap().get(17).unwrap();
        assert_eq!(root_bank.epoch(), 0);
        assert_eq!(root_bank.slot(), 17);
        verify_epoch_specs(&mut epoch_specs, &root_bank);

        // root is updated but epoch still the same.
        bank_forks.write().unwrap().set_root(19, None, None);
        let root_bank = bank_forks.read().unwrap().get(19).unwrap();
        assert_eq!(root_bank.epoch(), 0);
        assert_eq!(root_bank.slot(), 19);
        verify_epoch_specs(&mut epoch_specs, &root_bank);

        // root is updated to a new epoch.
        bank_forks.write().unwrap().set_root(37, None, None);
        let root_bank = bank_forks.read().unwrap().get(37).unwrap();
        assert_eq!(root_bank.epoch(), 1);
        assert_eq!(root_bank.slot(), 37);
        verify_epoch_specs(&mut epoch_specs, &root_bank);

        // root is updated but epoch still the same.
        bank_forks.write().unwrap().set_root(59, None, None);
        let root_bank = bank_forks.read().unwrap().get(59).unwrap();
        assert_eq!(root_bank.epoch(), 1);
        assert_eq!(root_bank.slot(), 59);
        verify_epoch_specs(&mut epoch_specs, &root_bank);

        // root is updated to a new epoch.
        bank_forks.write().unwrap().set_root(97, None, None);
        let root_bank = bank_forks.read().unwrap().get(97).unwrap();
        assert_eq!(root_bank.epoch(), 2);
        assert_eq!(root_bank.slot(), 97);
        verify_epoch_specs(&mut epoch_specs, &root_bank);
    }
}
