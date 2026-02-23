use {
    super::{SlotLeader, stake_weighted_slot_leaders},
    itertools::Itertools,
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_vote::vote_account::VoteAccountsHashMap,
    std::{collections::HashMap, iter, ops::Index},
};

#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct LeaderSchedule {
    slot_leaders: Vec<SlotLeader>,
    // Inverted index from leader id to indices where they are the leader.
    leader_slots_map: HashMap<Pubkey, Vec<usize>>,
}

impl LeaderSchedule {
    // Note: passing in zero vote accounts will cause a panic.
    pub fn new(
        vote_accounts_map: &VoteAccountsHashMap,
        epoch: Epoch,
        len: u64,
        repeat: u64,
    ) -> Self {
        let slot_leader_stakes: Vec<_> = vote_accounts_map
            .iter()
            .filter(|(_pubkey, (stake, _account))| *stake > 0)
            .map(|(&vote_address, (stake, vote_account))| {
                (
                    SlotLeader {
                        vote_address,
                        id: *vote_account.node_pubkey(),
                    },
                    *stake,
                )
            })
            .collect();
        let slot_leaders = stake_weighted_slot_leaders(slot_leader_stakes, epoch, len, repeat);
        Self::new_from_schedule(slot_leaders)
    }

    pub fn new_from_schedule(slot_leaders: Vec<SlotLeader>) -> Self {
        let leader_slots_map = Self::invert_slot_leaders(&slot_leaders);
        Self {
            slot_leaders,
            leader_slots_map,
        }
    }

    fn invert_slot_leaders(slot_leaders: &[SlotLeader]) -> HashMap<Pubkey, Vec<usize>> {
        slot_leaders
            .iter()
            .enumerate()
            .map(|(i, leader)| (leader.id, i))
            .into_group_map()
    }

    pub fn get_slot_leaders(&self) -> &[SlotLeader] {
        &self.slot_leaders
    }

    pub fn get_leader_upcoming_slots(
        &self,
        leader_id: &Pubkey,
        offset: usize, // Starting index.
    ) -> Box<dyn Iterator<Item = usize> + '_> {
        let index = self.leader_slots_map.get(leader_id);
        let num_slots = self.num_slots();

        match index {
            Some(index) if !index.is_empty() => {
                let size = index.len();
                let start_offset = index
                    .binary_search(&(offset % num_slots))
                    .unwrap_or_else(std::convert::identity)
                    + offset / num_slots * size;
                // The modular arithmetic here and above replicate Index implementation
                // for LeaderSchedule, where the schedule keeps repeating endlessly.
                // The '%' returns where in a cycle we are and the '/' returns how many
                // times the schedule is repeated.
                Box::new(
                    (start_offset..=usize::MAX)
                        .map(move |k| index[k % size] + k / size * num_slots),
                )
            }
            _ => {
                // Empty iterator for pubkeys not in schedule
                Box::new(iter::empty())
            }
        }
    }

    pub fn num_slots(&self) -> usize {
        self.slot_leaders.len()
    }

    #[cfg(test)]
    pub fn get_vote_key_at_slot_index(&self, index: usize) -> &Pubkey {
        &self.slot_leaders[index % self.num_slots()].vote_address
    }
}

impl Index<u64> for LeaderSchedule {
    type Output = SlotLeader;
    fn index(&self, index: u64) -> &SlotLeader {
        &self.slot_leaders[index as usize % self.num_slots()]
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_vote::vote_account::VoteAccount};

    #[test]
    fn test_index() {
        let slot_leaders = vec![SlotLeader::new_unique(), SlotLeader::new_unique()];
        let leader_schedule = LeaderSchedule::new_from_schedule(slot_leaders.clone());
        assert_eq!(leader_schedule[0], slot_leaders[0]);
        assert_eq!(leader_schedule[1], slot_leaders[1]);
        assert_eq!(leader_schedule[2], slot_leaders[0]);
    }

    #[test]
    fn test_get_vote_key_at_slot_index() {
        let slot_leaders = vec![SlotLeader::new_unique(), SlotLeader::new_unique()];
        let leader_schedule = LeaderSchedule::new_from_schedule(slot_leaders.clone());
        assert_eq!(
            leader_schedule.get_vote_key_at_slot_index(0),
            &slot_leaders[0].vote_address
        );
        assert_eq!(
            leader_schedule.get_vote_key_at_slot_index(1),
            &slot_leaders[1].vote_address
        );
        assert_eq!(
            leader_schedule.get_vote_key_at_slot_index(2),
            &slot_leaders[0].vote_address
        );
    }

    #[test]
    fn test_leader_schedule_basic() {
        let num_keys = 10;
        let vote_accounts_map: HashMap<_, _> = (0..num_keys)
            .map(|i| (solana_pubkey::new_rand(), (i, VoteAccount::new_random())))
            .collect();

        let epoch: Epoch = rand::random();
        let len = num_keys * 10;
        let leader_schedule = LeaderSchedule::new(&vote_accounts_map, epoch, len, 1);
        let leader_schedule2 = LeaderSchedule::new(&vote_accounts_map, epoch, len, 1);
        assert_eq!(leader_schedule.num_slots() as u64, len);
        // Check that the same schedule is reproducibly generated
        assert_eq!(leader_schedule, leader_schedule2);
    }

    #[test]
    fn test_repeated_leader_schedule() {
        let num_keys = 10;
        let vote_accounts_map: HashMap<_, _> = (0..num_keys)
            .map(|i| (solana_pubkey::new_rand(), (i, VoteAccount::new_random())))
            .collect();

        let epoch = rand::random::<Epoch>();
        let repeat = 8;
        let len = num_keys * repeat;
        let leader_schedule = LeaderSchedule::new(&vote_accounts_map, epoch, len, repeat);
        assert_eq!(leader_schedule.num_slots() as u64, len);
        let mut leader_node = SlotLeader::default();
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
        let vote_key0 = solana_pubkey::new_rand();
        let vote_key1 = solana_pubkey::new_rand();
        let vote_accounts_map: HashMap<_, _> = [
            (vote_key0, (2, VoteAccount::new_random())),
            (vote_key1, (1, VoteAccount::new_random())),
        ]
        .into_iter()
        .collect();
        let leader_alice = SlotLeader {
            id: *vote_accounts_map.get(&vote_key0).unwrap().1.node_pubkey(),
            vote_address: vote_key0,
        };
        let leader_bob = SlotLeader {
            id: *vote_accounts_map.get(&vote_key1).unwrap().1.node_pubkey(),
            vote_address: vote_key1,
        };

        let epoch = 0;
        let len = 8;
        // What the schedule looks like without any repeats
        let leaders1 = LeaderSchedule::new(&vote_accounts_map, epoch, len, 1)
            .get_slot_leaders()
            .to_vec();

        // What the schedule looks like with repeats
        let leaders2 = LeaderSchedule::new(&vote_accounts_map, epoch, len, 2)
            .get_slot_leaders()
            .to_vec();
        assert_eq!(leaders1.len(), leaders2.len());

        let leaders1_expected = vec![
            leader_alice,
            leader_alice,
            leader_alice,
            leader_bob,
            leader_alice,
            leader_alice,
            leader_alice,
            leader_alice,
        ];
        let leaders2_expected = vec![
            leader_alice,
            leader_alice,
            leader_alice,
            leader_alice,
            leader_alice,
            leader_alice,
            leader_bob,
            leader_bob,
        ];

        assert_eq!(leaders1, leaders1_expected);
        assert_eq!(leaders2, leaders2_expected);
    }
}
