use {
    solana_clock::{DEFAULT_MS_PER_SLOT, Epoch},
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
    fn epoch_current_staked_nodes(&mut self) -> Arc<HashMap<Pubkey, u64>> {
        let cache = &mut self.cache;
        Self::refresh_cache(cache, &self.sharable_banks);
        Arc::clone(&cache.current_epoch_staked_nodes)
    }

    fn epoch_duration(&mut self) -> Duration {
        let cache = &mut self.cache;
        Self::refresh_cache(cache, &self.sharable_banks);
        cache.epoch_duration
    }

    fn epoch_slots(&mut self) -> u64 {
        let cache = &mut self.cache;
        Self::refresh_cache(cache, &self.sharable_banks);
        cache.slots_in_epoch
    }

    fn clone_box(&self) -> Box<dyn EpochSpecsTrait> {
        Box::new(self.clone())
    }
}

impl EpochSpecs {
    fn refresh_cache(cache: &mut EpochSpecsCache, shareable_banks: &SharableBanks) {
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
    Duration::from_millis(num_slots * DEFAULT_MS_PER_SLOT)
}
