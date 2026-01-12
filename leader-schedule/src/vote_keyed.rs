use {
    super::{SlotLeader, stake_weighted_slot_leaders},
    itertools::Itertools,
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_vote::vote_account::VoteAccountsHashMap,
    std::{collections::HashMap, iter, num::NonZeroUsize, ops::Index},
};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct LeaderSchedule {
    slot_leaders: Vec<SlotLeader>,
    // Inverted index from leader id to indices where they are the leader.
    leader_slots_map: HashMap<Pubkey, Vec<usize>>,
    repeat: NonZeroUsize,
}

impl Default for LeaderSchedule {
    fn default() -> Self {
        Self {
            slot_leaders: Vec::default(),
            leader_slots_map: HashMap::default(),
            repeat: NonZeroUsize::new(1).unwrap(),
        }
    }
}

impl LeaderSchedule {
    // Note: passing in zero vote accounts will cause a panic.
    pub fn new(
        vote_accounts_map: &VoteAccountsHashMap,
        epoch: Epoch,
        len: usize,
        repeat: NonZeroUsize,
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
        Self::new_from_schedule(slot_leaders, repeat)
    }

    pub fn new_from_schedule(slot_leaders: Vec<SlotLeader>, repeat: NonZeroUsize) -> Self {
        let leader_slots_map = Self::invert_slot_leaders(&slot_leaders);
        Self {
            slot_leaders,
            leader_slots_map,
            repeat,
        }
    }

    fn invert_slot_leaders(slot_leaders: &[SlotLeader]) -> HashMap<Pubkey, Vec<usize>> {
        slot_leaders
            .iter()
            .enumerate()
            .map(|(i, leader)| (leader.id, i))
            .into_group_map()
    }

    pub fn get_slot_leaders(&self) -> impl Iterator<Item = &SlotLeader> {
        self.slot_leaders
            .iter()
            .flat_map(|leader| iter::repeat_n(leader, self.repeat()))
    }

    fn repeat(&self) -> usize {
        self.repeat.get()
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
                let offset_in_epoch = offset % num_slots;
                let repeat = self.repeat();
                let offset_chunk = offset_in_epoch / repeat;
                // We don't store repetitions in the schedule, so we need to find the
                // first element representing the latest chunk of `repeat` slots.
                // Also, find out how many slots from the starting chunk we still have
                // to yield.
                let (start_index, offset_in_chunk) = match index.binary_search(&offset_chunk) {
                    Ok(index) => (index, offset_in_epoch % repeat),
                    Err(index) => (index, 0),
                };
                let start_offset = start_index + offset / num_slots * size;
                // The modular arithmetic here and above replicate Index implementation
                // for LeaderSchedule, where the schedule keeps repeating endlessly.
                // The '%' returns where in a cycle we are and the '/' returns how many
                // times the schedule is repeated.
                Box::new(iter::chain(
                    // First yield the remaining slots from the starting chunk.
                    (offset_in_chunk..repeat).map(move |k| {
                        index[start_offset % size] * repeat + start_offset / size * num_slots + k
                    }),
                    // Then start visiting next chunks (with the same `repeat`).
                    ((start_offset + 1)..).flat_map(move |k| {
                        (0..repeat)
                            .map(move |j| index[k % size] * repeat + k / size * num_slots + j)
                    }),
                ))
            }
            _ => {
                // Empty iterator for pubkeys not in schedule
                Box::new(iter::empty())
            }
        }
    }

    pub fn num_slots(&self) -> usize {
        self.slot_leaders.len().saturating_mul(self.repeat())
    }

    pub fn get_slot_leader_at_index(&self, index: usize) -> SlotLeader {
        self.slot_leaders[index % self.num_slots() / self.repeat()]
    }

    #[cfg(test)]
    pub fn get_vote_key_at_slot_index(&self, index: usize) -> &Pubkey {
        &self.slot_leaders[index % self.num_slots() / self.repeat()].vote_address
    }
}

impl Index<u64> for LeaderSchedule {
    type Output = SlotLeader;
    fn index(&self, index: u64) -> &SlotLeader {
        &self.slot_leaders[index as usize % self.num_slots() / self.repeat()]
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_vote::vote_account::VoteAccount};

    const NZ_1: NonZeroUsize = NonZeroUsize::new(1).unwrap();
    const NZ_2: NonZeroUsize = NonZeroUsize::new(2).unwrap();
    const NZ_4: NonZeroUsize = NonZeroUsize::new(4).unwrap();
    const NZ_8: NonZeroUsize = NonZeroUsize::new(8).unwrap();

    #[test]
    fn test_index() {
        let slot_leaders = vec![SlotLeader::new_unique(), SlotLeader::new_unique()];
        let leader_schedule = LeaderSchedule::new_from_schedule(slot_leaders.clone(), NZ_1);
        assert_eq!(leader_schedule[0], slot_leaders[0]);
        assert_eq!(leader_schedule[1], slot_leaders[1]);
        assert_eq!(leader_schedule[2], slot_leaders[0]);
    }

    #[test]
    fn test_get_vote_key_at_slot_index() {
        let slot_leaders = vec![SlotLeader::new_unique(), SlotLeader::new_unique()];
        let leader_schedule = LeaderSchedule::new_from_schedule(slot_leaders.clone(), NZ_1);
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
            .map(|i| {
                (
                    solana_pubkey::new_rand(),
                    (i as u64, VoteAccount::new_random()),
                )
            })
            .collect();

        let epoch: Epoch = rand::random();
        let len = num_keys * 10;
        let repeat = NZ_1;
        let leader_schedule = LeaderSchedule::new(&vote_accounts_map, epoch, len, repeat);
        let leader_schedule2 = LeaderSchedule::new(&vote_accounts_map, epoch, len, repeat);
        assert_eq!(leader_schedule.num_slots(), len);
        // Check that the same schedule is reproducibly generated
        assert_eq!(leader_schedule, leader_schedule2);
    }

    #[test]
    fn test_repeated_leader_schedule() {
        let num_keys = 10;
        let vote_accounts_map: HashMap<_, _> = (0..num_keys)
            .map(|i| {
                (
                    solana_pubkey::new_rand(),
                    (i as u64, VoteAccount::new_random()),
                )
            })
            .collect();

        let epoch = rand::random::<Epoch>();
        let repeat = NZ_8;
        let len = num_keys * repeat.get();
        let leader_schedule = LeaderSchedule::new(&vote_accounts_map, epoch, len, repeat);
        assert_eq!(leader_schedule.num_slots(), len);
        let mut leader_node = SlotLeader::default();
        for (i, node) in leader_schedule.get_slot_leaders().enumerate() {
            if i % repeat.get() == 0 {
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
        let leader_schedule1 = LeaderSchedule::new(&vote_accounts_map, epoch, len, NZ_1);
        let leaders1: Vec<_> = leader_schedule1.get_slot_leaders().collect();

        // What the schedule looks like with repeats
        let leader_schedule2 = LeaderSchedule::new(&vote_accounts_map, epoch, len, NZ_2);
        let leaders2: Vec<_> = leader_schedule2.get_slot_leaders().collect();
        assert_eq!(leaders1.len(), leaders2.len());

        let leaders1_expected = vec![
            &leader_alice,
            &leader_alice,
            &leader_alice,
            &leader_bob,
            &leader_alice,
            &leader_alice,
            &leader_alice,
            &leader_alice,
        ];
        let leaders2_expected = vec![
            &leader_alice,
            &leader_alice,
            &leader_alice,
            &leader_alice,
            &leader_alice,
            &leader_alice,
            &leader_bob,
            &leader_bob,
        ];

        assert_eq!(leaders1, leaders1_expected);
        assert_eq!(leaders2, leaders2_expected);
    }

    #[test]
    fn test_get_vote_key_at_slot_index_with_repeat() {
        let slot_leaders = vec![SlotLeader::new_unique(), SlotLeader::new_unique()];
        let leader_schedule = LeaderSchedule::new_from_schedule(slot_leaders.clone(), NZ_4);

        for i in 0..4 {
            assert_eq!(
                leader_schedule.get_vote_key_at_slot_index(i),
                &slot_leaders[0].vote_address
            );
        }
        for i in 4..8 {
            assert_eq!(
                leader_schedule.get_vote_key_at_slot_index(i),
                &slot_leaders[1].vote_address
            );
        }
        assert_eq!(
            leader_schedule.get_vote_key_at_slot_index(8),
            &slot_leaders[0].vote_address
        );
    }

    #[test]
    fn test_get_leader_upcoming_slots_with_repeat() {
        let leader_a = SlotLeader::new_unique();
        let leader_b = SlotLeader::new_unique();
        let leader_schedule =
            LeaderSchedule::new_from_schedule(vec![leader_a, leader_b, leader_a], NZ_4);

        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 0)
                .take(16)
                .eq([0, 1, 2, 3, 8, 9, 10, 11, 12, 13, 14, 15, 20, 21, 22, 23])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 1)
                .take(15)
                .eq([1, 2, 3, 8, 9, 10, 11, 12, 13, 14, 15, 20, 21, 22, 23])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 2)
                .take(14)
                .eq([2, 3, 8, 9, 10, 11, 12, 13, 14, 15, 20, 21, 22, 23])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 3)
                .take(13)
                .eq([3, 8, 9, 10, 11, 12, 13, 14, 15, 20, 21, 22, 23])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 4)
                .take(10)
                .eq([8, 9, 10, 11, 12, 13, 14, 15, 20, 21])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 5)
                .take(10)
                .eq([8, 9, 10, 11, 12, 13, 14, 15, 20, 21])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 6)
                .take(10)
                .eq([8, 9, 10, 11, 12, 13, 14, 15, 20, 21])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 7)
                .take(10)
                .eq([8, 9, 10, 11, 12, 13, 14, 15, 20, 21])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 8)
                .take(10)
                .eq([8, 9, 10, 11, 12, 13, 14, 15, 20, 21])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 10)
                .take(10)
                .eq([10, 11, 12, 13, 14, 15, 20, 21, 22, 23])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 11)
                .take(10)
                .eq([11, 12, 13, 14, 15, 20, 21, 22, 23, 24])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 12)
                .take(10)
                .eq([12, 13, 14, 15, 20, 21, 22, 23, 24, 25])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 15)
                .take(10)
                .eq([15, 20, 21, 22, 23, 24, 25, 26, 27, 32])
        );

        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 11)
                .take(10)
                .all(|slot| slot >= 11 && leader_schedule[slot as u64].id == leader_a.id)
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_a.id, 15)
                .take(10)
                .all(|slot| slot >= 15 && leader_schedule[slot as u64].id == leader_a.id)
        );

        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_b.id, 0)
                .take(8)
                .eq([4, 5, 6, 7, 16, 17, 18, 19])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_b.id, 4)
                .take(8)
                .eq([4, 5, 6, 7, 16, 17, 18, 19])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_b.id, 5)
                .take(8)
                .eq([5, 6, 7, 16, 17, 18, 19, 28])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_b.id, 7)
                .take(8)
                .eq([7, 16, 17, 18, 19, 28, 29, 30])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_b.id, 8)
                .take(8)
                .eq([16, 17, 18, 19, 28, 29, 30, 31])
        );
        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&leader_b.id, 5)
                .take(8)
                .all(|slot| slot >= 5 && leader_schedule[slot as u64].id == leader_b.id)
        );

        assert!(
            leader_schedule
                .get_leader_upcoming_slots(&Pubkey::new_unique(), 0)
                .next()
                .is_none()
        );
    }
}
