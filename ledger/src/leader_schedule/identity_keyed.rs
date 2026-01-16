use {
    super::{stake_weighted_slot_leaders, LeaderScheduleVariant},
    itertools::Itertools,
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, ops::Index},
};

#[derive(Default, Debug, PartialEq, Eq, Clone)]
pub struct LeaderSchedule {
    slot_leaders: Vec<Pubkey>,
    // Inverted index from pubkeys to indices where they are the leader.
    leader_slots_map: HashMap<Pubkey, Vec<usize>>,
}

impl LeaderSchedule {
    // Note: passing in zero stakers will cause a panic.
    pub fn new(
        epoch_staked_nodes: &HashMap<Pubkey, u64>,
        epoch: Epoch,
        len: u64,
        repeat: u64,
    ) -> Self {
        let keyed_stakes: Vec<_> = epoch_staked_nodes
            .iter()
            .map(|(pubkey, stake)| (pubkey, *stake))
            .collect();
        let slot_leaders = stake_weighted_slot_leaders(keyed_stakes, epoch, len, repeat);
        Self::new_from_schedule(slot_leaders)
    }

    pub fn new_from_schedule(slot_leaders: Vec<Pubkey>) -> Self {
        Self {
            leader_slots_map: Self::invert_slot_leaders(&slot_leaders),
            slot_leaders,
        }
    }

    fn invert_slot_leaders(slot_leaders: &[Pubkey]) -> HashMap<Pubkey, Vec<usize>> {
        slot_leaders
            .iter()
            .enumerate()
            .map(|(i, pk)| (*pk, i))
            .into_group_map()
    }

    pub fn get_slot_leaders(&self) -> &[Pubkey] {
        &self.slot_leaders
    }
}

impl LeaderScheduleVariant for LeaderSchedule {
    fn get_slot_leaders(&self) -> &[Pubkey] {
        &self.slot_leaders
    }

    fn get_leader_slots_map(&self) -> &HashMap<Pubkey, Vec<usize>> {
        &self.leader_slots_map
    }
}

impl Index<u64> for LeaderSchedule {
    type Output = Pubkey;
    fn index(&self, index: u64) -> &Pubkey {
        &self.get_slot_leaders()[index as usize % self.num_slots()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_leader_schedule_index() {
        let pubkey0 = solana_pubkey::new_rand();
        let pubkey1 = solana_pubkey::new_rand();
        let leader_schedule = LeaderSchedule::new_from_schedule(vec![pubkey0, pubkey1]);
        assert_eq!(leader_schedule[0], pubkey0);
        assert_eq!(leader_schedule[1], pubkey1);
        assert_eq!(leader_schedule[2], pubkey0);
    }

    #[test]
    fn test_leader_schedule_basic() {
        let num_keys = 10;
        let stakes: HashMap<_, _> = (0..num_keys)
            .map(|i| (solana_pubkey::new_rand(), i))
            .collect();

        let epoch: Epoch = rand::random();
        let len = num_keys * 10;
        let leader_schedule = LeaderSchedule::new(&stakes, epoch, len, 1);
        let leader_schedule2 = LeaderSchedule::new(&stakes, epoch, len, 1);
        assert_eq!(leader_schedule.num_slots() as u64, len);
        // Check that the same schedule is reproducibly generated
        assert_eq!(leader_schedule, leader_schedule2);
    }

    #[test]
    fn test_repeated_leader_schedule() {
        let num_keys = 10;
        let stakes: HashMap<_, _> = (0..num_keys)
            .map(|i| (solana_pubkey::new_rand(), i))
            .collect();

        let epoch = rand::random::<Epoch>();
        let repeat = 8;
        let len = num_keys * repeat;
        let leader_schedule = LeaderSchedule::new(&stakes, epoch, len, repeat);
        assert_eq!(leader_schedule.num_slots() as u64, len);
        let mut leader_node = Pubkey::default();
        for (i, node) in leader_schedule.get_slot_leaders().iter().enumerate() {
            if i % repeat as usize == 0 {
                leader_node = *node;
            } else {
                assert_eq!(leader_node, *node);
            }
        }
    }

    #[test]
    fn test_repeated_leader_schedule_specific() {
        let alice_pubkey = solana_pubkey::new_rand();
        let bob_pubkey = solana_pubkey::new_rand();
        let stakes: HashMap<_, _> = [(alice_pubkey, 2), (bob_pubkey, 1)].into_iter().collect();

        let epoch = 0;
        let len = 8;
        // What the schedule looks like without any repeats
        let leaders1 = LeaderSchedule::new(&stakes, epoch, len, 1)
            .get_slot_leaders()
            .to_vec();

        // What the schedule looks like with repeats
        let leaders2 = LeaderSchedule::new(&stakes, epoch, len, 2)
            .get_slot_leaders()
            .to_vec();
        assert_eq!(leaders1.len(), leaders2.len());

        let leaders1_expected = vec![
            alice_pubkey,
            alice_pubkey,
            alice_pubkey,
            bob_pubkey,
            alice_pubkey,
            alice_pubkey,
            alice_pubkey,
            alice_pubkey,
        ];
        let leaders2_expected = vec![
            alice_pubkey,
            alice_pubkey,
            alice_pubkey,
            alice_pubkey,
            alice_pubkey,
            alice_pubkey,
            bob_pubkey,
            bob_pubkey,
        ];

        assert_eq!(leaders1, leaders1_expected);
        assert_eq!(leaders2, leaders2_expected);
    }

    #[test]
    fn test_invert_slot_leaders() {
        let alice_pubkey = solana_pubkey::new_rand();
        let bob_pubkey = solana_pubkey::new_rand();
        let victor_pubkey = solana_pubkey::new_rand();
        let peggy_pubkey = solana_pubkey::new_rand();

        let leaders = &[
            alice_pubkey,
            victor_pubkey,
            alice_pubkey,
            bob_pubkey,
            peggy_pubkey,
            alice_pubkey,
            peggy_pubkey,
            victor_pubkey,
        ];

        let grouped_slot_leaders = LeaderSchedule::invert_slot_leaders(leaders);
        assert_eq!(
            grouped_slot_leaders.get(&alice_pubkey).unwrap().as_slice(),
            &[0, 2, 5],
        );
        assert_eq!(
            grouped_slot_leaders.get(&bob_pubkey).unwrap().as_slice(),
            &[3],
        );
        assert_eq!(
            grouped_slot_leaders.get(&victor_pubkey).unwrap().as_slice(),
            &[1, 7],
        );
        assert_eq!(
            grouped_slot_leaders.get(&peggy_pubkey).unwrap().as_slice(),
            &[4, 6],
        );
    }
}
