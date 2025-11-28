use {
    agave_random::weighted::WeightedU64Index,
    rand_chacha0_3_1::{rand_core::SeedableRng, ChaChaRng},
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    std::{collections::HashMap, convert::identity, ops::Index, sync::Arc},
};

mod identity_keyed;
mod vote_keyed;
pub use {
    identity_keyed::LeaderSchedule as IdentityKeyedLeaderSchedule,
    vote_keyed::LeaderSchedule as VoteKeyedLeaderSchedule,
};

// Used for testing
#[derive(Clone, Debug)]
pub struct FixedSchedule {
    pub leader_schedule: Arc<LeaderSchedule>,
}

/// Stake-weighted leader schedule for one epoch.
pub type LeaderSchedule = Box<dyn LeaderScheduleVariant>;

pub trait LeaderScheduleVariant:
    std::fmt::Debug + Send + Sync + Index<u64, Output = Pubkey>
{
    fn get_slot_leaders(&self) -> &[Pubkey];
    fn get_leader_slots_map(&self) -> &HashMap<Pubkey, Vec<usize>>;

    /// Get the vote account address for the given epoch slot index. This is
    /// guaranteed to be Some if the leader schedule is keyed by vote account
    fn get_vote_key_at_slot_index(&self, _epoch_slot_index: usize) -> Option<&Pubkey> {
        None
    }

    fn get_leader_upcoming_slots(
        &self,
        pubkey: &Pubkey,
        offset: usize, // Starting index.
    ) -> Box<dyn Iterator<Item = usize> + '_> {
        let index = self.get_leader_slots_map().get(pubkey);
        let num_slots = self.num_slots();

        match index {
            Some(index) if !index.is_empty() => {
                let size = index.len();
                let start_offset = index
                    .binary_search(&(offset % num_slots))
                    .unwrap_or_else(identity)
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
                #[allow(clippy::reversed_empty_ranges)]
                Box::new((1..=0).map(|_| 0))
            }
        }
    }

    fn num_slots(&self) -> usize {
        self.get_slot_leaders().len()
    }
}

// Note: passing in zero keyed stakes will cause a panic.
fn stake_weighted_slot_leaders(
    mut keyed_stakes: Vec<(&Pubkey, u64)>,
    epoch: Epoch,
    len: u64,
    repeat: u64,
) -> Vec<Pubkey> {
    sort_stakes(&mut keyed_stakes);
    let (keys, stakes): (Vec<_>, Vec<_>) = keyed_stakes.into_iter().unzip();
    let weighted_index = WeightedU64Index::new(stakes).unwrap();
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
    use {super::*, itertools::Itertools, rand::Rng, std::iter::repeat_with, test_case::test_case};

    #[test]
    fn test_get_leader_upcoming_slots() {
        const NUM_SLOTS: usize = 97;
        let mut rng = rand::rng();
        let pubkeys: Vec<_> = repeat_with(Pubkey::new_unique).take(4).collect();
        let schedule: Vec<_> = repeat_with(|| pubkeys[rng.random_range(0..3)])
            .take(19)
            .collect();
        let schedule = IdentityKeyedLeaderSchedule::new_from_schedule(schedule);
        let leaders = (0..NUM_SLOTS)
            .map(|i| (schedule[i as u64], i))
            .into_group_map();
        for pubkey in &pubkeys {
            let index = leaders.get(pubkey).cloned().unwrap_or_default();
            for offset in 0..NUM_SLOTS {
                let schedule: Vec<_> = schedule
                    .get_leader_upcoming_slots(pubkey, offset)
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

    fn pubkey_from_u16(n: u16) -> Pubkey {
        let mut bytes = [0; 32];
        bytes[0..2].copy_from_slice(&n.to_le_bytes());
        Pubkey::new_from_array(bytes)
    }

    #[test_case(1, &[10, 20, 30], 12, 1, &[1, 1, 2, 1, 1, 0, 0, 1, 2, 1, 0, 1])]
    #[test_case(1, &[10, 20, 30], 12, 2, &[1, 1, 1, 1, 2, 2, 1, 1, 1, 1, 0, 0])]
    #[test_case(1, &[30, 10, 20], 12, 1, &[2, 2, 0, 2, 2, 1, 1, 2, 0, 2, 1, 2])]
    #[test_case(1, &[30, 10, 20], 12, 2, &[2, 2, 2, 2, 0, 0, 2, 2, 2, 2, 1,1])]
    #[test_case(1, &[10, 20, 25, 30], 12, 1, &[2, 2, 3, 1, 2, 0, 1, 1, 3, 2, 1, 2])]
    #[test_case(1, &[10, 20, 25, 30, 35, 40, 100], 15, 1,
                &[4, 5, 6, 3, 4, 1, 2, 3, 6, 4, 2, 4, 5, 6, 6])]
    #[test_case(1, &[10, 20, 25, 30, 35, 40, 100, 1000], 15, 1,
                &[7, 7, 7, 7, 7, 4, 6, 7, 7, 7, 6, 7, 7, 7, 7])]
    #[test_case(1, &[10, 20, 25, 30, 35, 40, 100, 1000, 10_000], 20, 1,
                &[8, 8, 8, 8, 8, 7, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 7])]
    #[test_case(1, &[10, 20, 25, 30, 35, 40, 100, 1000, 10_000], 25, 1,
                &[8, 8, 8, 8, 8, 7, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8, 7, 8, 8, 8, 8, 8])]
    #[test_case(457468, &[10, 20, 30], 12, 1, &[2, 2, 0, 1, 0, 2, 1, 2, 1, 2, 2, 2])]
    #[test_case(457468, &[10, 20, 30], 12, 2, &[2, 2, 2, 2, 0, 0, 1, 1, 0, 0, 2, 2])]
    #[test_case(457469, &[10, 20, 30], 12, 1, &[1, 2, 2, 2, 2, 2, 2, 1, 0, 2, 2, 0])]
    #[test_case(457470, &[10, 20, 30], 12, 1, &[2, 1, 1, 1, 1, 1, 1, 1, 1, 2, 0, 2])]
    #[test_case(3466545, &[10, 20, 30], 12, 1, &[2, 2, 0, 0, 2, 1, 1, 1, 0, 0, 2, 2])]
    #[test_case(3466545, &[10, 20, 30], 13, 1, &[2, 2, 0, 0, 2, 1, 1, 1, 0, 0, 2, 2, 1])]
    #[test_case(3466545, &[10, 20, 30], 13, 2, &[2, 2, 2, 2, 0, 0, 0, 0, 2, 2, 1, 1, 1])]
    #[test_case(3466545, &[10, 20, 30], 14, 1, &[2, 2, 0, 0, 2, 1, 1, 1, 0, 0, 2, 2, 1, 2])]
    #[test_case(3466545, &[10, 20, 30], 14, 2, &[2, 2, 2, 2, 0, 0, 0, 0, 2, 2, 1, 1, 1, 1])]
    fn test_stake_leader_schedule_exact_order(
        epoch: u64,
        stakes: &[u64],
        len: u64,
        repeat: u64,
        expected_order: &[usize],
    ) {
        let pubkeys: Vec<_> = (0..stakes.len() as u16).map(pubkey_from_u16).collect();
        let stakes = pubkeys.iter().zip(stakes.iter().copied()).collect();
        let order: Vec<_> = stake_weighted_slot_leaders(stakes, epoch, len, repeat)
            .into_iter()
            .map(|pubkey| {
                pubkeys
                    .iter()
                    .find_position(|item| *item == &pubkey)
                    .unwrap()
                    .0
            })
            .collect();
        assert_eq!(order, expected_order);
    }

    #[test_case(42, 1_000, 0, "4XU6LEarBUmBkAvXRsjeyLu3N8CcgrvbRFrNiJi2jECk")]
    #[test_case(42, 10_000, 0, "G2MGFXgdLATXWr1336i8PTcaUMc4GbJRMJdbxiarCttr")]
    #[test_case(42, 10_000, 1, "9xLLKyyqF5YrdwPSDbqh5oVamSF7cqPqQLxEyHTexEiP")]
    #[test_case(42, 10_000, 2, "AJ6NQi2p5SnRz9mqESqkW2PwVoT2vYy1fmKdaHxNFUAf")]
    #[test_case(42, 10_000, 3, "2oLjZggMwDTQhzdB4KN5VQisyeRw6MZbBBdjosNZK5xR")]
    #[test_case(346436, 1_000, 0, "59SnXMS4NzTSib8TNykiJgFQBeAVxUqsAvQm7JtkodPQ")]
    #[test_case(346436, 1_000, 1, "BEB2nC9MBALPbgwGKGfHu6V88QG7doScx65cAd6VjnRk")]
    #[test_case(346436, 1_000, 2, "3aLE5S6xLEU9yg5EZQH27qrC86aC2dG8KLh4NbcapXpy")]
    #[test_case(346436, 1_000, 3, "H2bw3Y2AjxJyK7smy1ZBB4LJ7MY3i9bPQM3YdAChAww2")]
    #[test_case(454357, 10_000, 0, "4BLanrC5t7vzNXx62javKtjCmCkd8yZfZpVrjT4eUpNQ")]
    #[test_case(454357, 10_000, 1, "FyvbdxpVchendERMnzH2KDceqydpXtJarrfFXoLQEXgQ")]
    #[test_case(454357, 10_000, 2, "7KwK44Y7V3GzJLN8aGZtM8EEfAYmRvaiDyKYV6jg4MQn")]
    #[test_case(454357, 10_000, 3, "E9XL5BLhCJ4Emyfs8jTUsQetfA8QZj78LcnN63dPp7jJ")]
    fn test_long_leader_schedule_hashed(
        epoch: Epoch,
        len: u64,
        stake_pow: u32,
        expected_hash: &str,
    ) {
        fn hash_pubkeys(v: &[Pubkey]) -> String {
            use sha2::{Digest, Sha256};

            let hasher = v.iter().fold(Sha256::new(), |hasher, pk| {
                hasher.chain_update(pk.to_bytes())
            });
            bs58::encode(hasher.finalize()).into_string()
        }
        let pubkeys: Vec<_> = (0..=u16::MAX).map(pubkey_from_u16).collect();
        let stakes = pubkeys
            .iter()
            .enumerate()
            .map(|(i, pk)| (pk, i.pow(stake_pow) as u64))
            .collect();
        let schedule = stake_weighted_slot_leaders(stakes, epoch, len, 1);
        assert_eq!(hash_pubkeys(&schedule), expected_hash);
    }

    #[test]
    #[should_panic]
    fn test_zero_stake_panics() {
        let _ = stake_weighted_slot_leaders(
            vec![(&pubkey_from_u16(1), 0), (&pubkey_from_u16(2), 0)],
            0,
            5,
            1,
        );
    }
}
