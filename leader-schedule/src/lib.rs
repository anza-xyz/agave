//! Solana leader schedule.

#![cfg(feature = "agave-unstable-api")]
#![allow(clippy::arithmetic_side_effects)]

use {
    agave_random::weighted::WeightedU64Index,
    rand_chacha::{ChaChaRng, rand_core::SeedableRng},
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    std::sync::Arc,
};

mod vote_keyed;
/// Stake-weighted leader schedule for one epoch.
pub use vote_keyed::LeaderSchedule;

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
pub struct SlotLeader {
    pub id: Pubkey,
    pub vote_address: Pubkey,
}

impl SlotLeader {
    pub fn new_unique() -> Self {
        SlotLeader {
            id: Pubkey::new_unique(),
            vote_address: Pubkey::new_unique(),
        }
    }
}

// Used for testing
#[derive(Clone, Debug)]
pub struct FixedSchedule {
    pub leader_schedule: Arc<LeaderSchedule>,
}

// Note: passing in zero keyed stakes will cause a panic.
fn stake_weighted_slot_leaders(
    mut slot_leader_stakes: Vec<(SlotLeader, u64)>,
    epoch: Epoch,
    len: u64,
    repeat: u64,
) -> Vec<SlotLeader> {
    debug_assert!(
        len.is_multiple_of(repeat),
        "expected `len` {len} to be divisible by `repeat` {repeat}"
    );
    sort_stakes(&mut slot_leader_stakes);
    let (slot_leaders, stakes): (Vec<_>, Vec<_>) = slot_leader_stakes.into_iter().unzip();
    let weighted_index = WeightedU64Index::new(stakes).unwrap();
    let mut seed = [0u8; 32];
    seed[0..8].copy_from_slice(&epoch.to_le_bytes());
    let rng = &mut ChaChaRng::from_seed(seed);
    let mut current_slot_leader = SlotLeader::default();
    (0..len)
        .map(|i| {
            if i % repeat == 0 {
                current_slot_leader = slot_leaders[weighted_index.sample(rng)];
            }
            current_slot_leader
        })
        .collect()
}

fn sort_stakes(stakes: &mut Vec<(SlotLeader, u64)>) {
    // Sort first by stake. If stakes are the same, sort by vote address pubkey
    // to ensure a deterministic result.
    // Note: Use unstable sort, because we dedup right after to remove the equal
    // elements.
    stakes.sort_unstable_by(|(l_leader, l_stake), (r_leader, r_stake)| {
        if r_stake == l_stake {
            r_leader.vote_address.cmp(&l_leader.vote_address)
        } else {
            r_stake.cmp(l_stake)
        }
    });

    // Now that it's sorted, we can do an O(n) dedup.
    stakes.dedup_by(|(l_leader, l_stake), (r_leader, r_stake)| {
        r_stake == l_stake && r_leader.vote_address == l_leader.vote_address
    });
}

#[cfg(test)]
mod tests {
    use {super::*, itertools::Itertools, rand::Rng, std::iter::repeat_with, test_case::test_case};

    #[test]
    fn test_get_leader_upcoming_slots() {
        const NUM_SLOTS: usize = 97;
        let mut rng = rand::rng();
        let unique_leaders: Vec<_> = repeat_with(SlotLeader::new_unique).take(4).collect();
        let schedule: Vec<_> = repeat_with(|| unique_leaders[rng.random_range(0..3)])
            .take(19)
            .collect();
        let schedule = LeaderSchedule::new_from_schedule(schedule);
        let leaders = (0..NUM_SLOTS)
            .map(|i| (schedule[i as u64].id, i))
            .into_group_map();
        for leader in &unique_leaders {
            let index = leaders.get(&leader.id).cloned().unwrap_or_default();
            for offset in 0..NUM_SLOTS {
                let upcoming_slots: Vec<_> = schedule
                    .get_leader_upcoming_slots(&leader.id, offset)
                    .take_while(|s| *s < NUM_SLOTS)
                    .collect();
                let expected: Vec<_> = index.iter().copied().skip_while(|s| *s < offset).collect();
                assert_eq!(upcoming_slots, expected);
            }
        }
    }

    #[test]
    fn test_sort_stakes_basic() {
        let leader0 = SlotLeader::new_rand();
        let leader1 = SlotLeader::new_rand();
        let mut stakes = vec![(leader0, 1), (leader1, 2)];
        sort_stakes(&mut stakes);
        assert_eq!(stakes, vec![(leader1, 2), (leader0, 1)]);
    }

    #[test]
    fn test_sort_stakes_with_dup() {
        let leader0 = SlotLeader::new_rand();
        let leader1 = SlotLeader::new_rand();
        let mut stakes = vec![(leader0, 1), (leader1, 2), (leader0, 1)];
        sort_stakes(&mut stakes);
        assert_eq!(stakes, vec![(leader1, 2), (leader0, 1)]);
    }

    #[test]
    fn test_sort_stakes_with_equal_stakes() {
        let leader0 = SlotLeader {
            id: solana_pubkey::new_rand(),
            vote_address: Pubkey::default(),
        };
        let leader1 = SlotLeader {
            id: solana_pubkey::new_rand(),
            vote_address: solana_pubkey::new_rand(),
        };
        let mut stakes = vec![(leader0, 1), (leader1, 1)];
        sort_stakes(&mut stakes);
        assert_eq!(stakes, vec![(leader1, 1), (leader0, 1)]);
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
    #[test_case(3466545, &[10, 20, 30], 14, 1, &[2, 2, 0, 0, 2, 1, 1, 1, 0, 0, 2, 2, 1, 2])]
    #[test_case(3466545, &[10, 20, 30], 14, 2, &[2, 2, 2, 2, 0, 0, 0, 0, 2, 2, 1, 1, 1, 1])]
    fn test_stake_leader_schedule_exact_order(
        epoch: u64,
        stakes: &[u64],
        len: u64,
        repeat: u64,
        expected_order: &[usize],
    ) {
        let slot_leaders: Vec<_> = (0..stakes.len() as u16)
            .map(|seed| SlotLeader {
                id: pubkey_from_u16(seed),
                vote_address: Pubkey::new_unique(),
            })
            .collect();
        let stakes = slot_leaders
            .iter()
            .copied()
            .zip(stakes.iter().copied())
            .collect();
        let order: Vec<_> = stake_weighted_slot_leaders(stakes, epoch, len, repeat)
            .into_iter()
            .map(|slot_leader| {
                slot_leaders
                    .iter()
                    .find_position(|item| *item == &slot_leader)
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
        fn hash_slot_leader_vote_addresses(v: &[SlotLeader]) -> String {
            use sha2::{Digest, Sha256};

            let hasher = v.iter().fold(Sha256::new(), |hasher, slot_leader| {
                hasher.chain_update(slot_leader.vote_address.to_bytes())
            });
            bs58::encode(hasher.finalize()).into_string()
        }
        let slot_leaders: Vec<_> = (0..=u16::MAX)
            .map(|seed| SlotLeader {
                vote_address: pubkey_from_u16(seed),
                id: Pubkey::new_unique(),
            })
            .collect();
        let stakes = slot_leaders
            .iter()
            .copied()
            .enumerate()
            .map(|(i, slot_leader)| (slot_leader, i.pow(stake_pow) as u64))
            .collect();
        let schedule = stake_weighted_slot_leaders(stakes, epoch, len, 1);
        assert_eq!(hash_slot_leader_vote_addresses(&schedule), expected_hash);
    }

    impl SlotLeader {
        pub fn new_rand() -> Self {
            SlotLeader {
                id: solana_pubkey::new_rand(),
                vote_address: solana_pubkey::new_rand(),
            }
        }
    }

    #[test]
    #[should_panic]
    fn test_zero_stake_panics() {
        let _ = stake_weighted_slot_leaders(
            vec![(SlotLeader::new_unique(), 0), (SlotLeader::new_unique(), 0)],
            0,
            5,
            1,
        );
    }
}
