use {
    solana_runtime::{
        bank::Bank,
        bank_forks::{BankForks, ReadOnlyAtomicSlot},
    },
    solana_sdk::{
        clock::{Epoch, DEFAULT_MS_PER_SLOT},
        epoch_schedule::EpochSchedule,
        pubkey::Pubkey,
    },
    std::{
        collections::HashMap,
        sync::{Arc, RwLock},
        time::Duration,
    },
};

// Caches epoch specific information which stay fixed throughout the epoch.
// Refreshes only if the root bank has moved to a new epoch.
pub(crate) struct EpochSpecs {
    epoch: Epoch, // when fields were last updated.
    epoch_schedule: EpochSchedule,
    root: ReadOnlyAtomicSlot, // updated by bank-forks.
    bank_forks: Arc<RwLock<BankForks>>,
    current_epoch_staked_nodes: Arc<HashMap<Pubkey, /*stake:*/ u64>>,
    epoch_duration: Duration,
}

impl EpochSpecs {
    #[inline]
    pub(crate) fn current_epoch_staked_nodes(&mut self) -> &Arc<HashMap<Pubkey, /*stake:*/ u64>> {
        self.maybe_refresh();
        &self.current_epoch_staked_nodes
    }

    #[inline]
    pub(crate) fn epoch_duration(&mut self) -> Duration {
        self.maybe_refresh();
        self.epoch_duration
    }

    // Updates fields if root bank has moved to a new epoch.
    fn maybe_refresh(&mut self) {
        if self.epoch_schedule.get_epoch(self.root.get()) == self.epoch {
            return; // still same epoch. nothing to update.
        }
        let root_bank = self.bank_forks.read().unwrap().root_bank();
        debug_assert_eq!(
            self.epoch_schedule.get_epoch(root_bank.slot()),
            root_bank.epoch()
        );
        self.epoch = root_bank.epoch();
        self.epoch_schedule = root_bank.epoch_schedule().clone();
        self.current_epoch_staked_nodes = root_bank.current_epoch_staked_nodes();
        self.epoch_duration = get_epoch_duration(&root_bank);
    }
}

impl From<Arc<RwLock<BankForks>>> for EpochSpecs {
    fn from(bank_forks: Arc<RwLock<BankForks>>) -> Self {
        let (root, root_bank) = {
            let bank_forks = bank_forks.read().unwrap();
            (bank_forks.get_atomic_root(), bank_forks.root_bank())
        };
        Self {
            epoch: root_bank.epoch(),
            epoch_schedule: root_bank.epoch_schedule().clone(),
            root,
            bank_forks,
            current_epoch_staked_nodes: root_bank.current_epoch_staked_nodes(),
            epoch_duration: get_epoch_duration(&root_bank),
        }
    }
}

fn get_epoch_duration(bank: &Bank) -> Duration {
    let num_slots = bank.get_slots_in_epoch(bank.epoch());
    Duration::from_millis(num_slots * DEFAULT_MS_PER_SLOT)
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_runtime::{
            accounts_background_service::AbsRequestSender,
            genesis_utils::{create_genesis_config, GenesisConfigInfo},
        },
        solana_sdk::clock::Slot,
    };

    #[test]
    fn test_get_epoch_duration() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let mut bank = Bank::new_for_tests(&genesis_config);
        let epoch = 0;
        let num_slots = 32;
        assert_eq!(bank.epoch(), epoch);
        assert_eq!(bank.get_slots_in_epoch(epoch), num_slots);
        assert_eq!(
            get_epoch_duration(&bank),
            Duration::from_millis(num_slots * 400)
        );
        for slot in 1..32 {
            bank = Bank::new_from_parent(Arc::new(bank), &Pubkey::new_unique(), slot);
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
            bank = Bank::new_from_parent(Arc::new(bank), &Pubkey::new_unique(), slot);
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
            bank = Bank::new_from_parent(Arc::new(bank), &Pubkey::new_unique(), slot);
            assert_eq!(bank.epoch(), epoch);
            assert_eq!(bank.get_slots_in_epoch(epoch), num_slots);
            assert_eq!(
                get_epoch_duration(&bank),
                Duration::from_millis(num_slots * 400)
            );
        }
    }

    fn verify_epoch_specs(
        epoch_specs: &mut EpochSpecs,
        epoch: Epoch,
        slot: Slot,
        root_bank: &Bank,
    ) {
        assert_eq!(
            epoch_specs.current_epoch_staked_nodes(),
            &root_bank.current_epoch_staked_nodes()
        );
        assert_eq!(epoch_specs.epoch_duration(), get_epoch_duration(root_bank));
        assert_eq!(root_bank.slot(), slot);
        assert_eq!(root_bank.epoch(), epoch);
        assert_eq!(epoch_specs.epoch, epoch);
        assert_eq!(&epoch_specs.epoch_schedule, root_bank.epoch_schedule());
        assert_eq!(epoch_specs.root.get(), slot);
    }

    #[test]
    fn test_epoch_specs_refresh() {
        let GenesisConfigInfo { genesis_config, .. } = create_genesis_config(10_000);
        let bank = Bank::new_for_tests(&genesis_config);
        let bank_forks = BankForks::new_rw_arc(bank);
        let mut epoch_specs = EpochSpecs::from(bank_forks.clone());
        for slot in 1..100 {
            let bank = bank_forks.read().unwrap().get(slot - 1).unwrap();
            let bank = Bank::new_from_parent(bank, &Pubkey::new_unique(), slot);
            bank_forks.write().unwrap().insert(bank);
        }
        let abs_request_sender = AbsRequestSender::default();
        // root is still 0, epoch 0.
        let root_bank = bank_forks.read().unwrap().get(0).unwrap();
        verify_epoch_specs(
            &mut epoch_specs,
            0, // epoch
            0, // slot
            &root_bank,
        );
        // root is updated but epoch still the same.
        bank_forks
            .write()
            .unwrap()
            .set_root(17, &abs_request_sender, None)
            .unwrap();
        let root_bank = bank_forks.read().unwrap().get(17).unwrap();
        verify_epoch_specs(
            &mut epoch_specs,
            0,  // epoch
            17, // slot
            &root_bank,
        );
        // root is updated but epoch still the same.
        bank_forks
            .write()
            .unwrap()
            .set_root(19, &abs_request_sender, None)
            .unwrap();
        let root_bank = bank_forks.read().unwrap().get(19).unwrap();
        verify_epoch_specs(
            &mut epoch_specs,
            0,  // epoch
            19, // slot
            &root_bank,
        );
        // root is updated to a new epoch.
        bank_forks
            .write()
            .unwrap()
            .set_root(37, &abs_request_sender, None)
            .unwrap();
        let root_bank = bank_forks.read().unwrap().get(37).unwrap();
        verify_epoch_specs(
            &mut epoch_specs,
            1,  // epoch
            37, // slot
            &root_bank,
        );
        // root is updated but epoch still the same.
        bank_forks
            .write()
            .unwrap()
            .set_root(59, &abs_request_sender, None)
            .unwrap();
        let root_bank = bank_forks.read().unwrap().get(59).unwrap();
        verify_epoch_specs(
            &mut epoch_specs,
            1,  // epoch
            59, // slot
            &root_bank,
        );
        // root is updated to a new epoch.
        bank_forks
            .write()
            .unwrap()
            .set_root(97, &abs_request_sender, None)
            .unwrap();
        let root_bank = bank_forks.read().unwrap().get(97).unwrap();
        verify_epoch_specs(
            &mut epoch_specs,
            2,  // epoch
            97, // slot
            &root_bank,
        );
    }
}
