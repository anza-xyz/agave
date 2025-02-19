use {
    rand::distributions::{Distribution, WeightedIndex},
    rand_chacha::{rand_core::SeedableRng, ChaChaRng},
    solana_pubkey::Pubkey,
    solana_sdk::clock::Epoch,
    solana_vote::vote_account::VoteAccountsHashMap,
    std::{collections::HashMap, convert::identity, ops::Index, sync::Arc},
};

mod identity_keyed;
mod vote_keyed;

// Used for testing
#[derive(Clone, Debug)]
pub struct FixedSchedule {
    pub leader_schedule: Arc<LeaderSchedule>,
}

/// Stake-weighted leader schedule for one epoch.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct LeaderSchedule(LeaderScheduleVariants);

#[cfg(feature = "dev-context-only-utils")]
impl Default for LeaderSchedule {
    fn default() -> Self {
        Self(LeaderScheduleVariants::IdentityKeyed(
            identity_keyed::LeaderSchedule::default(),
        ))
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum LeaderScheduleVariants {
    // Latest leader schedule algorithm which designates a specific vote account
    // to each slot so that the runtime can load vote state (e.g. commission and
    // fee collector accounts) for a given slot
    VoteKeyed(vote_keyed::LeaderSchedule),
    // Old leader schedule algorithm which designates a specific validator
    // identity to each slot. Since multiple vote accounts can be associated
    // with a single validator identity, it's not possible to use this to load
    // vote state for a given slot.
    IdentityKeyed(identity_keyed::LeaderSchedule),
}

impl LeaderSchedule {
    // Note: passing in zero vote accounts will cause a panic.
    pub fn new_vote_keyed(
        vote_accounts_map: &VoteAccountsHashMap,
        epoch: Epoch,
        len: u64,
        repeat: u64,
    ) -> Self {
        Self(LeaderScheduleVariants::VoteKeyed(
            vote_keyed::LeaderSchedule::new(vote_accounts_map, epoch, len, repeat),
        ))
    }

    // Note: passing in zero stakers will cause a panic.
    pub fn new_identity_keyed(
        epoch_staked_nodes: &HashMap<Pubkey, u64>,
        epoch: Epoch,
        len: u64,
        repeat: u64,
    ) -> Self {
        Self(LeaderScheduleVariants::IdentityKeyed(
            identity_keyed::LeaderSchedule::new(epoch_staked_nodes, epoch, len, repeat),
        ))
    }

    // Note: passing in zero stakers will cause a panic.
    fn stake_weighted_slot_leaders(
        mut keyed_stakes: Vec<(&Pubkey, u64)>,
        epoch: Epoch,
        len: u64,
        repeat: u64,
    ) -> Vec<Pubkey> {
        sort_stakes(&mut keyed_stakes);
        let (keys, stakes): (Vec<_>, Vec<_>) = keyed_stakes.into_iter().unzip();
        let weighted_index = WeightedIndex::new(stakes).unwrap();
        let mut seed = [0u8; 32];
        seed[0..8].copy_from_slice(&epoch.to_le_bytes());
        let rng = &mut ChaChaRng::from_seed(seed);
        let mut current_slot_leader = Pubkey::default();
        (0..len)
            .map(|i| {
                if i % repeat == 0 {
                    current_slot_leader = keys[weighted_index.sample(rng)];
                }
                current_slot_leader
            })
            .collect()
    }

    pub fn new_identity_keyed_with_schedule(slot_leaders: Vec<Pubkey>) -> Self {
        Self(LeaderScheduleVariants::IdentityKeyed(
            identity_keyed::LeaderSchedule::new_from_schedule(slot_leaders),
        ))
    }

    pub fn is_vote_keyed(&self) -> bool {
        matches!(self.0, LeaderScheduleVariants::VoteKeyed(_))
    }

    /// Get the vote account address for the given epoch slot index. This is
    /// guaranteed to be Some if the leader schedule is keyed by vote account
    /// and the slot index is within the range of the leader schedule.
    pub fn get_vote_account_address_for_slot_index(
        &self,
        epoch_slot_index: usize,
    ) -> Option<&Pubkey> {
        match &self.0 {
            LeaderScheduleVariants::VoteKeyed(schedule) => schedule
                .slot_leader_vote_account_addresses
                .get(epoch_slot_index),
            LeaderScheduleVariants::IdentityKeyed(_) => None,
        }
    }

    pub fn get_slot_leaders(&self) -> &[Pubkey] {
        match self.0 {
            LeaderScheduleVariants::VoteKeyed(ref schedule) => {
                &schedule.identity_keyed_leader_schedule.slot_leaders
            }
            LeaderScheduleVariants::IdentityKeyed(ref schedule) => &schedule.slot_leaders,
        }
    }

    pub fn num_slots(&self) -> usize {
        self.get_slot_leaders().len()
    }

    fn index(&self) -> &HashMap<Pubkey, Arc<Vec<usize>>> {
        match &self.0 {
            LeaderScheduleVariants::VoteKeyed(schedule) => {
                &schedule.identity_keyed_leader_schedule.index
            }
            LeaderScheduleVariants::IdentityKeyed(schedule) => &schedule.index,
        }
    }

    /// 'offset' is an index into the leader schedule. The function returns an
    /// iterator of indices i >= offset where the given pubkey is the leader.
    pub(crate) fn get_indices(
        &self,
        pubkey: &Pubkey,
        offset: usize, // Starting index.
    ) -> impl Iterator<Item = usize> {
        let index = self.index().get(pubkey).cloned().unwrap_or_default();
        let num_slots = self.num_slots();
        let size = index.len();
        #[allow(clippy::reversed_empty_ranges)]
        let range = if index.is_empty() {
            1..=0 // Intentionally empty range of type RangeInclusive.
        } else {
            let offset = index
                .binary_search(&(offset % num_slots))
                .unwrap_or_else(identity)
                + offset / num_slots * size;
            offset..=usize::MAX
        };
        // The modular arithmetic here and above replicate Index implementation
        // for LeaderSchedule, where the schedule keeps repeating endlessly.
        // The '%' returns where in a cycle we are and the '/' returns how many
        // times the schedule is repeated.
        range.map(move |k| index[k % size] + k / size * num_slots)
    }
}

impl Index<u64> for LeaderSchedule {
    type Output = Pubkey;
    fn index(&self, index: u64) -> &Pubkey {
        let index = index as usize;
        &self.get_slot_leaders()[index % self.num_slots()]
    }
}

fn sort_stakes(stakes: &mut Vec<(&Pubkey, u64)>) {
    // Sort first by stake. If stakes are the same, sort by pubkey to ensure a
    // deterministic result.
    // Note: Use unstable sort, because we dedup right after to remove the equal elements.
    stakes.sort_unstable_by(|(l_pubkey, l_stake), (r_pubkey, r_stake)| {
        if r_stake == l_stake {
            r_pubkey.cmp(l_pubkey)
        } else {
            r_stake.cmp(l_stake)
        }
    });

    // Now that it's sorted, we can do an O(n) dedup.
    stakes.dedup();
}

#[cfg(test)]
mod tests {
    use {super::*, itertools::Itertools, rand::Rng, std::iter::repeat_with};

    #[test]
    fn test_leader_schedule_index() {
        let pubkey0 = solana_pubkey::new_rand();
        let pubkey1 = solana_pubkey::new_rand();
        let leader_schedule =
            LeaderSchedule::new_identity_keyed_with_schedule(vec![pubkey0, pubkey1]);
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
        let leader_schedule = LeaderSchedule::new_identity_keyed(&stakes, epoch, len, 1);
        let leader_schedule2 = LeaderSchedule::new_identity_keyed(&stakes, epoch, len, 1);
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
        let len = num_keys * 10;
        let repeat = 8;
        let leader_schedule = LeaderSchedule::new_identity_keyed(&stakes, epoch, len, repeat);
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
        let leaders1 = LeaderSchedule::new_identity_keyed(&stakes, epoch, len, 1)
            .get_slot_leaders()
            .to_vec();

        // What the schedule looks like with repeats
        let leaders2 = LeaderSchedule::new_identity_keyed(&stakes, epoch, len, 2)
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
    fn test_get_indices() {
        const NUM_SLOTS: usize = 97;
        let mut rng = rand::thread_rng();
        let pubkeys: Vec<_> = repeat_with(Pubkey::new_unique).take(4).collect();
        let schedule: Vec<_> = repeat_with(|| pubkeys[rng.gen_range(0..3)])
            .take(19)
            .collect();
        let schedule = LeaderSchedule::new_identity_keyed_with_schedule(schedule);
        let leaders = (0..NUM_SLOTS)
            .map(|i| (schedule[i as u64], i))
            .into_group_map();
        for pubkey in &pubkeys {
            let index = leaders.get(pubkey).cloned().unwrap_or_default();
            for offset in 0..NUM_SLOTS {
                let schedule: Vec<_> = schedule
                    .get_indices(pubkey, offset)
                    .take_while(|s| *s < NUM_SLOTS)
                    .collect();
                let index: Vec<_> = index.iter().copied().skip_while(|s| *s < offset).collect();
                assert_eq!(schedule, index);
            }
        }
    }

    #[test]
    fn test_sort_stakes_basic() {
        let pubkey0 = solana_pubkey::new_rand();
        let pubkey1 = solana_pubkey::new_rand();
        let mut stakes = vec![(&pubkey0, 1), (&pubkey1, 2)];
        sort_stakes(&mut stakes);
        assert_eq!(stakes, vec![(&pubkey1, 2), (&pubkey0, 1)]);
    }

    #[test]
    fn test_sort_stakes_with_dup() {
        let pubkey0 = solana_pubkey::new_rand();
        let pubkey1 = solana_pubkey::new_rand();
        let mut stakes = vec![(&pubkey0, 1), (&pubkey1, 2), (&pubkey0, 1)];
        sort_stakes(&mut stakes);
        assert_eq!(stakes, vec![(&pubkey1, 2), (&pubkey0, 1)]);
    }

    #[test]
    fn test_sort_stakes_with_equal_stakes() {
        let pubkey0 = Pubkey::default();
        let pubkey1 = solana_pubkey::new_rand();
        let mut stakes = vec![(&pubkey0, 1), (&pubkey1, 1)];
        sort_stakes(&mut stakes);
        assert_eq!(stakes, vec![(&pubkey1, 1), (&pubkey0, 1)]);
    }
}
