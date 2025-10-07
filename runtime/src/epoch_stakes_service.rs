use {
    crate::{
        bank::Bank,
        epoch_stakes::{BLSPubkeyToRankMap, VersionedEpochStakes},
    },
    crossbeam_channel::Receiver,
    log::warn,
    parking_lot::RwLock as PlRwLock,
    solana_clock::{Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    std::{collections::HashMap, sync::Arc, thread},
};

struct State {
    stakes: HashMap<Epoch, VersionedEpochStakes>,
    epoch_schedule: EpochSchedule,
}

impl State {
    fn new(bank: Arc<Bank>) -> Self {
        Self {
            stakes: bank.epoch_stakes_map().clone(),
            epoch_schedule: bank.epoch_schedule().clone(),
        }
    }
}

/// A service that regularly updates the epoch stakes state from `Bank`s
/// and exposes various methods to access the state.
pub struct EpochStakesService {
    state: Arc<PlRwLock<State>>,
}

impl EpochStakesService {
    pub fn new(bank: Arc<Bank>, epoch: Epoch, new_bank_receiver: Receiver<Arc<Bank>>) -> Self {
        let mut prev_epoch = epoch;
        let state = Arc::new(PlRwLock::new(State::new(bank)));
        {
            let state = state.clone();
            thread::spawn(move || loop {
                let bank = match new_bank_receiver.recv() {
                    Ok(b) => b,
                    Err(e) => {
                        warn!("recv() returned {e:?}.  Exiting.");
                        break;
                    }
                };
                let new_epoch = bank.epoch();
                if new_epoch > prev_epoch {
                    prev_epoch = new_epoch;
                    *state.write() = State::new(bank)
                }
            });
        }
        Self { state }
    }

    pub fn get_key_to_rank_map(&self, slot: Slot) -> Option<Arc<BLSPubkeyToRankMap>> {
        let guard = self.state.read();
        let epoch = guard.epoch_schedule.get_epoch(slot);
        guard
            .stakes
            .get(&epoch)
            .map(|stake| Arc::clone(stake.bls_pubkey_to_rank_map()))
    }
}
