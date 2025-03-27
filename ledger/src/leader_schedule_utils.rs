use {
    crate::leader_schedule::LeaderSchedule,
    solana_runtime::bank::Bank,
    solana_sdk::{
        clock::{Epoch, Slot, NUM_CONSECUTIVE_LEADER_SLOTS},
        pubkey::Pubkey,
    },
    std::collections::HashMap,
};

/// Return the leader schedule for the given epoch.
pub fn leader_schedule(epoch: Epoch, bank: &Bank) -> Option<LeaderSchedule> {
    bank.epoch_staked_nodes(epoch).map(|stakes| {
        LeaderSchedule::new_keyed_by_validator_identity(
            &stakes,
            epoch,
            bank.get_slots_in_epoch(epoch),
            NUM_CONSECUTIVE_LEADER_SLOTS,
        )
    })
}

/// Map of leader base58 identity pubkeys to the slot indices relative to the first epoch slot
pub type LeaderScheduleByIdentity = HashMap<String, Vec<usize>>;

pub fn leader_schedule_by_identity<'a>(
    upcoming_leaders: impl Iterator<Item = (usize, &'a Pubkey)>,
) -> LeaderScheduleByIdentity {
    let mut leader_schedule_by_identity = HashMap::new();

    for (slot_index, identity_pubkey) in upcoming_leaders {
        leader_schedule_by_identity
            .entry(identity_pubkey)
            .or_insert_with(Vec::new)
            .push(slot_index);
    }

    leader_schedule_by_identity
        .into_iter()
        .map(|(identity_pubkey, slot_indices)| (identity_pubkey.to_string(), slot_indices))
        .collect()
}

/// Return the leader for the given slot.
pub fn slot_leader_at(slot: Slot, bank: &Bank) -> Option<Pubkey> {
    let (epoch, slot_index) = bank.get_epoch_and_slot_index(slot);

    leader_schedule(epoch, bank).map(|leader_schedule| leader_schedule[slot_index])
}

// Returns the number of ticks remaining from the specified tick_height to the end of the
// slot implied by the tick_height
pub fn num_ticks_left_in_slot(bank: &Bank, tick_height: u64) -> u64 {
    bank.ticks_per_slot() - tick_height % bank.ticks_per_slot()
}

pub fn first_of_consecutive_leader_slots(slot: Slot) -> Slot {
    (slot / NUM_CONSECUTIVE_LEADER_SLOTS) * NUM_CONSECUTIVE_LEADER_SLOTS
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_runtime::genesis_utils::{
            bootstrap_validator_stake_lamports, create_genesis_config_with_leader,
        },
    };

    #[test]
    fn test_leader_schedule_via_bank() {
        let pubkey = solana_pubkey::new_rand();
        let genesis_config =
            create_genesis_config_with_leader(0, &pubkey, bootstrap_validator_stake_lamports())
                .genesis_config;
        let bank = Bank::new_for_tests(&genesis_config);

        let pubkeys_and_stakes: HashMap<_, _> = bank
            .current_epoch_staked_nodes()
            .iter()
            .map(|(pubkey, stake)| (*pubkey, *stake))
            .collect();
        let leader_schedule = LeaderSchedule::new_keyed_by_validator_identity(
            &pubkeys_and_stakes,
            0,
            genesis_config.epoch_schedule.slots_per_epoch,
            NUM_CONSECUTIVE_LEADER_SLOTS,
        );

        assert_eq!(leader_schedule[0], pubkey);
        assert_eq!(leader_schedule[1], pubkey);
        assert_eq!(leader_schedule[2], pubkey);
    }

    #[test]
    fn test_leader_scheduler1_basic() {
        let pubkey = solana_pubkey::new_rand();
        let genesis_config =
            create_genesis_config_with_leader(42, &pubkey, bootstrap_validator_stake_lamports())
                .genesis_config;
        let bank = Bank::new_for_tests(&genesis_config);
        assert_eq!(slot_leader_at(bank.slot(), &bank).unwrap(), pubkey);
    }
}
