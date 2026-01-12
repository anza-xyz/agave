use {
    super::{stake_weighted_slot_leaders, SlotLeader},
    itertools::Itertools,
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_vote::vote_account::VoteAccountsHashMap,
    std::{collections::HashMap, iter, ops::Index},
};

/// Iterator that expands compressed leader chunks into real slot numbers.
struct LeaderSlotsIter<'a> {
    slots: &'a [usize],
    /// Number of physical slots each entry in `slots` represents.
    repeat: usize,
    num_slots: usize,
    idx: usize,
    repeat_offset: usize,
    /// How many full epochs of this validator’s slots we’ve already emitted.
    cycle: usize,
}

impl<'a> Iterator for LeaderSlotsIter<'a> {
    type Item = usize;
    fn next(&mut self) -> Option<Self::Item> {
        if self.slots.is_empty() {
            return None;
        }
        let base = self.slots[self.idx].saturating_mul(self.repeat);
        let slot = base
            .saturating_add(self.repeat_offset)
            .saturating_add(self.cycle.saturating_mul(self.num_slots));
        self.repeat_offset = self.repeat_offset.saturating_add(1);
        if self.repeat_offset == self.repeat {
            self.repeat_offset = 0;
            self.idx += 1;
            if self.idx == self.slots.len() {
                self.idx = 0;
                self.cycle = self.cycle.saturating_add(1);
            }
        }
        Some(slot)
    }
}

#[derive(Debug, Default, PartialEq, Eq, Clone)]
pub struct LeaderSchedule {
    slot_leaders: Vec<SlotLeader>,
    // Inverted index from leader id to indices where they are the leader.
    leader_slots_map: HashMap<Pubkey, Vec<usize>>,
    repeat: u64,
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
        Self::new_from_schedule(slot_leaders, repeat)
    }

    pub fn new_from_schedule(slot_leaders: Vec<SlotLeader>, repeat: u64) -> Self {
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
        self.repeat as usize
    }

    pub fn get_leader_upcoming_slots(
        &self,
        leader_id: &Pubkey,
        offset: usize, // Starting index.
    ) -> Box<dyn Iterator<Item = usize> + '_> {
        let index = self.leader_slots_map.get(leader_id);
        let num_slots = self.num_slots();
        let repeat = self.repeat();
        match index {
            Some(index) if !index.is_empty() && num_slots > 0 && repeat > 0 => {
                let size = index.len();
                let mut cycle = offset.saturating_div(num_slots);
                let offset_in_epoch = offset % num_slots;
                let offset_chunk = offset_in_epoch.saturating_div(repeat);
                let mut repeat_offset = offset_in_epoch % repeat;
                // `chunk_index` locates the first leadership chunk for this validator
                // at or after the requested offset.
                let chunk_index = match index.binary_search(&offset_chunk) {
                    Ok(pos) => pos,
                    Err(pos) => {
                        repeat_offset = 0;
                        if pos == size {
                            cycle = cycle.saturating_add(1);
                            0
                        } else {
                            pos
                        }
                    }
                };
                Box::new(LeaderSlotsIter {
                    slots: index,
                    repeat,
                    num_slots,
                    idx: chunk_index,
                    repeat_offset,
                    cycle,
                })
            }
            _ => {
                // Empty iterator for pubkeys not in schedule
                #[allow(clippy::reversed_empty_ranges)]
                Box::new((1..=0).map(|_| 0))
            }
        }
    }

    pub fn num_slots(&self) -> usize {
        self.slot_leaders.len().saturating_mul(self.repeat())
    }

    #[cfg(test)]
    pub fn get_vote_key_at_slot_index(&self, index: usize) -> &Pubkey {
        &self.slot_leaders[index % self.num_slots()].vote_address
    }
}

impl Index<u64> for LeaderSchedule {
    type Output = SlotLeader;
    fn index(&self, index: u64) -> &SlotLeader {
        &self.slot_leaders[index as usize % self.num_slots() / self.repeat as usize]
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_vote::vote_account::VoteAccount};

    #[test]
    fn test_index() {
        let slot_leaders = vec![SlotLeader::new_unique(), SlotLeader::new_unique()];
        let leader_schedule = LeaderSchedule::new_from_schedule(slot_leaders.clone(), 1);
        assert_eq!(leader_schedule[0], slot_leaders[0]);
        assert_eq!(leader_schedule[1], slot_leaders[1]);
        assert_eq!(leader_schedule[2], slot_leaders[0]);
    }

    #[test]
    fn test_get_vote_key_at_slot_index() {
        let slot_leaders = vec![SlotLeader::new_unique(), SlotLeader::new_unique()];
        let leader_schedule = LeaderSchedule::new_from_schedule(slot_leaders.clone(), 1);
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
        for (i, node) in leader_schedule.get_slot_leaders().enumerate() {
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
        let leader_schedule1 = LeaderSchedule::new(&vote_accounts_map, epoch, len, 1);
        let leaders1: Vec<_> = leader_schedule1.get_slot_leaders().collect();

        // What the schedule looks like with repeats
        let leader_schedule2 = LeaderSchedule::new(&vote_accounts_map, epoch, len, 2);
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
}
