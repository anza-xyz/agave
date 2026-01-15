use {
    super::{stake_weighted_slot_leaders, IdentityKeyedLeaderSchedule, LeaderScheduleVariant},
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_vote::vote_account::VoteAccountsHashMap,
    std::{collections::HashMap, ops::Index},
};

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct LeaderSchedule {
    vote_keyed_slot_leaders: Vec<Pubkey>,
    // cached leader schedule keyed by validator identities created by mapping
    // vote account addresses to the validator identity designated at the time
    // of leader schedule generation. This is used to avoid the need to look up
    // the validator identity address for each slot.
    identity_keyed_leader_schedule: IdentityKeyedLeaderSchedule,
}

impl LeaderSchedule {
    // Note: passing in zero vote accounts will cause a panic.
    pub fn new(
        vote_accounts_map: &VoteAccountsHashMap,
        epoch: Epoch,
        len: u64,
        repeat: u64,
    ) -> Self {
        let keyed_stakes: Vec<_> = vote_accounts_map
            .iter()
            .filter(|(_pubkey, (stake, _account))| *stake > 0)
            .map(|(vote_pubkey, (stake, _account))| (vote_pubkey, *stake))
            .collect();
        let vote_keyed_slot_leaders = stake_weighted_slot_leaders(keyed_stakes, epoch, len, repeat);
        Self::new_from_schedule(vote_keyed_slot_leaders, vote_accounts_map)
    }

    fn new_from_schedule(
        vote_keyed_slot_leaders: Vec<Pubkey>,
        vote_accounts_map: &VoteAccountsHashMap,
    ) -> Self {
        struct SlotLeaderInfo<'a> {
            vote_account_address: &'a Pubkey,
            validator_identity_address: &'a Pubkey,
        }

        let default_pubkey = Pubkey::default();
        let mut current_slot_leader_info = SlotLeaderInfo {
            vote_account_address: &default_pubkey,
            validator_identity_address: &default_pubkey,
        };

        let slot_leaders: Vec<Pubkey> = vote_keyed_slot_leaders
            .iter()
            .map(|vote_account_address| {
                if vote_account_address != current_slot_leader_info.vote_account_address {
                    let validator_identity_address = vote_accounts_map
                        .get(vote_account_address)
                        .expect("vote account must be in vote_accounts_map")
                        .1
                        .node_pubkey();
                    current_slot_leader_info = SlotLeaderInfo {
                        vote_account_address,
                        validator_identity_address,
                    };
                }
                *current_slot_leader_info.validator_identity_address
            })
            .collect();

        Self {
            vote_keyed_slot_leaders,
            identity_keyed_leader_schedule: IdentityKeyedLeaderSchedule::new_from_schedule(
                slot_leaders,
            ),
        }
    }
}

impl LeaderScheduleVariant for LeaderSchedule {
    fn get_slot_leaders(&self) -> &[Pubkey] {
        self.identity_keyed_leader_schedule.get_slot_leaders()
    }

    fn get_leader_slots_map(&self) -> &HashMap<Pubkey, Vec<usize>> {
        self.identity_keyed_leader_schedule.get_leader_slots_map()
    }

    fn get_vote_key_at_slot_index(&self, index: usize) -> Option<&Pubkey> {
        let slot_vote_addresses = &self.vote_keyed_slot_leaders;
        Some(&slot_vote_addresses[index % slot_vote_addresses.len()])
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
    use {super::*, solana_vote::vote_account::VoteAccount};

    #[test]
    fn test_index() {
        let pubkey0 = solana_pubkey::new_rand();
        let pubkey1 = solana_pubkey::new_rand();
        let vote_keyed_slot_leaders = vec![pubkey0, pubkey1];
        let vote_accounts_map: VoteAccountsHashMap = [
            (pubkey0, (0, VoteAccount::new_random())),
            (pubkey1, (0, VoteAccount::new_random())),
        ]
        .into_iter()
        .collect();

        let leader_schedule =
            LeaderSchedule::new_from_schedule(vote_keyed_slot_leaders, &vote_accounts_map);
        assert_eq!(
            &leader_schedule[0],
            vote_accounts_map.get(&pubkey0).unwrap().1.node_pubkey()
        );
        assert_eq!(
            &leader_schedule[1],
            vote_accounts_map.get(&pubkey1).unwrap().1.node_pubkey()
        );
        assert_eq!(
            &leader_schedule[2],
            vote_accounts_map.get(&pubkey0).unwrap().1.node_pubkey()
        );
    }

    #[test]
    fn test_get_vote_key_at_slot_index() {
        let pubkey0 = solana_pubkey::new_rand();
        let pubkey1 = solana_pubkey::new_rand();
        let vote_keyed_slot_leaders = vec![pubkey0, pubkey1];
        let vote_accounts_map: VoteAccountsHashMap = [
            (pubkey0, (0, VoteAccount::new_random())),
            (pubkey1, (0, VoteAccount::new_random())),
        ]
        .into_iter()
        .collect();

        let leader_schedule =
            LeaderSchedule::new_from_schedule(vote_keyed_slot_leaders, &vote_accounts_map);
        assert_eq!(
            leader_schedule.get_vote_key_at_slot_index(0),
            Some(&pubkey0)
        );
        assert_eq!(
            leader_schedule.get_vote_key_at_slot_index(1),
            Some(&pubkey1)
        );
        assert_eq!(
            leader_schedule.get_vote_key_at_slot_index(2),
            Some(&pubkey0)
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
        let vote_key0 = solana_pubkey::new_rand();
        let vote_key1 = solana_pubkey::new_rand();
        let vote_accounts_map: HashMap<_, _> = [
            (vote_key0, (2, VoteAccount::new_random())),
            (vote_key1, (1, VoteAccount::new_random())),
        ]
        .into_iter()
        .collect();
        let alice_pubkey = *vote_accounts_map.get(&vote_key0).unwrap().1.node_pubkey();
        let bob_pubkey = *vote_accounts_map.get(&vote_key1).unwrap().1.node_pubkey();

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
}
