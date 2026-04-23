use {
    crate::bank::Bank,
    solana_clock::{Epoch, Slot},
    solana_leader_schedule::LeaderSchedule,
    solana_pubkey::Pubkey,
    solana_vote::vote_account::VoteAccountsHashMap,
    std::{collections::HashMap, num::NonZeroUsize},
};

/// Return the leader schedule for the given epoch.
pub fn leader_schedule(epoch: Epoch, bank: &Bank) -> Option<LeaderSchedule> {
    leader_schedule_from_vote_accounts(
        epoch,
        bank.get_slots_in_epoch(epoch),
        bank.num_consecutive_leader_slots_for_epoch(epoch),
        bank.epoch_vote_accounts(epoch)?,
    )
}

/// Return the leader schedule for the given epoch using vote accounts directly.
/// This is useful for computing the leader schedule during snapshot restoration
/// before a Bank is fully constructed.
pub fn leader_schedule_from_vote_accounts(
    epoch: Epoch,
    slots_in_epoch: u64,
    num_consecutive_leader_slots: u64,
    epoch_vote_accounts: &VoteAccountsHashMap,
) -> Option<LeaderSchedule> {
    Some(LeaderSchedule::new(
        epoch_vote_accounts,
        epoch,
        slots_in_epoch
            .try_into()
            .expect("number of slots in epoch must fit in usize"),
        NonZeroUsize::new(
            num_consecutive_leader_slots
                .try_into()
                .expect("leader window size must fit in usize"),
        )
        .expect("leader window size must be non-zero"),
    ))
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

    leader_schedule(epoch, bank).map(|leader_schedule| leader_schedule[slot_index].id)
}

// Returns the number of ticks remaining from the specified tick_height to the end of the
// slot implied by the tick_height
pub fn num_ticks_left_in_slot(bank: &Bank, tick_height: u64) -> u64 {
    bank.ticks_per_slot() - tick_height % bank.ticks_per_slot()
}

pub fn first_of_consecutive_leader_slots(slot: Slot, bank: &Bank) -> Slot {
    let (epoch, slot_index) = bank.get_epoch_and_slot_index(slot);
    let first_slot_in_epoch = bank.get_first_slot_in_epoch(epoch);
    let num_consecutive_leader_slots = bank.num_consecutive_leader_slots_for_epoch(epoch);
    first_slot_in_epoch
        + slot_index
            .saturating_div(num_consecutive_leader_slots)
            .saturating_mul(num_consecutive_leader_slots)
}

/// Returns the last slot in the leader window that contains `slot`
#[inline]
pub fn last_of_consecutive_leader_slots(slot: Slot, bank: &Bank) -> Slot {
    first_of_consecutive_leader_slots(slot, bank) + bank.num_consecutive_leader_slots_at_slot(slot)
        - 1
}

/// Returns the index within the leader slot range that contains `slot`
#[inline]
pub fn leader_slot_index(slot: Slot, bank: &Bank) -> usize {
    let (_epoch, slot_index) = bank.get_epoch_and_slot_index(slot);
    slot_index as usize % bank.num_consecutive_leader_slots_at_slot(slot) as usize
}

/// Returns the number of slots left after `slot` in the leader window
/// that contains `slot`
#[inline]
pub fn remaining_slots_in_window(slot: Slot, bank: &Bank) -> usize {
    (bank.num_consecutive_leader_slots_at_slot(slot) as usize)
        .checked_sub(leader_slot_index(slot, bank))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::genesis_utils::{
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
        let leader_schedule = leader_schedule(0, &bank).unwrap();

        assert_eq!(leader_schedule[0].id, pubkey);
        assert_eq!(leader_schedule[1].id, pubkey);
        assert_eq!(leader_schedule[2].id, pubkey);
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

    #[test]
    fn test_leader_span_math() {
        let genesis_config = create_genesis_config_with_leader(
            0,
            &Pubkey::new_unique(),
            bootstrap_validator_stake_lamports(),
        )
        .genesis_config;
        let bank = Bank::new_for_tests(&genesis_config);
        let leader_span = bank.num_consecutive_leader_slots();
        assert!(leader_span >= 2);

        for slot in 0..leader_span {
            assert_eq!(first_of_consecutive_leader_slots(slot, &bank), 0);
            assert_eq!(
                last_of_consecutive_leader_slots(slot, &bank),
                leader_span - 1
            );
            assert_eq!(leader_slot_index(slot, &bank), slot as usize);
            assert_eq!(
                remaining_slots_in_window(slot, &bank),
                (leader_span - slot) as usize
            );
        }

        assert_eq!(
            first_of_consecutive_leader_slots(leader_span, &bank),
            leader_span
        );
        assert_eq!(
            last_of_consecutive_leader_slots(leader_span, &bank),
            leader_span * 2 - 1
        );
        assert_eq!(leader_slot_index(leader_span, &bank), 0);
        assert_eq!(
            remaining_slots_in_window(leader_span, &bank),
            leader_span as usize
        );
    }
}
