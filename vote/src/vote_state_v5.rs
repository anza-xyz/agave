//! Compatibility projection for consumers of the common vote-state fields.
//!
//! Account bytes always remain V5. This projection is used by the existing
//! vote program's common-field API and parsed RPC responses.
use {
    solana_vote_interface::{
        authorized_voters::AuthorizedVoters,
        state::{
            LandedVote, Lockout, VoteStateV4,
            vote_state_v5::{Location, VoteStateV5},
        },
    },
    std::collections::VecDeque,
};

pub fn common_fields(state: &VoteStateV5) -> VoteStateV4 {
    let mut authorized_voters = AuthorizedVoters::default();
    for voter in state.authorized_voters_ref().0 {
        authorized_voters.insert(voter.epoch, voter.pubkey);
    }
    VoteStateV4 {
        node_pubkey: state.node_pubkey,
        authorized_withdrawer: state.authorized_withdrawer,
        inflation_rewards_collector: state.inflation_rewards_collector,
        block_revenue_collector: state.block_revenue_collector,
        inflation_rewards_commission_bps: state
            .inflation_rewards_commission_ref()
            .bps
            .last()
            .unwrap()
            .get(),
        block_revenue_commission_bps: state
            .block_revenue_commission_ref()
            .bps
            .last()
            .unwrap()
            .get(),
        pending_delegator_rewards: state.pending_delegator_rewards,
        bls_pubkey_compressed: Some(state.bls_pubkey_compressed),
        votes: state
            .latest_vote()
            .map(|slot| LandedVote {
                lockout: Lockout::new(slot),
                latency: 0,
            })
            .into_iter()
            .collect::<VecDeque<_>>(),
        root_slot: state.root_slot(),
        authorized_voters,
        epoch_credits: state
            .accumulated_lamports_ref()
            .0
            .iter()
            .map(|entry| (entry.epoch, entry.lamports, entry.prev_lamports))
            .collect(),
        last_timestamp: state.last_timestamp.clone(),
    }
}

/// Squared ECEF chord distance. Location's constructor bounds each component,
/// so widening before subtraction makes every intermediate fit in 64 bits.
pub fn distance_squared(a: &Location, b: &Location) -> u64 {
    let square_difference = |a: i32, b: i32| {
        let difference = i64::from(a) - i64::from(b);
        (difference * difference) as u64
    };
    square_difference(a.x(), b.x())
        + square_difference(a.y(), b.y())
        + square_difference(a.z(), b.z())
}

#[cfg(test)]
pub(crate) mod tests {
    use {
        super::*,
        solana_clock::Clock,
        solana_pubkey::Pubkey,
        solana_vote_interface::{
            authorized_voters::AuthorizedVoters, state::vote_state_v5::VOTE_STATE_V5_SIZE,
        },
    };

    pub(crate) fn state() -> VoteStateV5 {
        VoteStateV5::from_vote_state_v4(
            VoteStateV4 {
                node_pubkey: Pubkey::new_unique(),
                authorized_voters: AuthorizedVoters::new(0, Pubkey::new_unique()),
                bls_pubkey_compressed: Some([1; 48]),
                pending_delegator_rewards: 123,
                epoch_credits: vec![(0, 20, 0)],
                ..VoteStateV4::default()
            },
            Location::try_new(4_051_542, 618_526, 4_870_645).unwrap(),
            Clock::default().epoch,
        )
        .unwrap()
    }

    pub(crate) fn data(state: &VoteStateV5) -> Vec<u8> {
        let mut data = vec![0; VOTE_STATE_V5_SIZE];
        state.write_account_data(&mut data).unwrap();
        data
    }

    #[test]
    fn coordinate_and_distance_vectors() {
        let cities = [
            (4_051_542, 618_526, 4_870_645, 116_387_798),
            (3_977_778, 0, 4_969_055, 122_847_190),
            (1_333_729, -4_654_333, 4_138_065, 87_098_983),
            (-3_955_214, 3_355_452, 3_699_409, 72_402_516),
        ];
        for (x, y, z, deviation) in cities {
            assert!(Location::try_new(x, y, z).is_some());
            let square = |v: i32| (i64::from(v) * i64::from(v)) as u64;
            let q = square(x) + square(y) + square(z) + ((442 * square(z)) >> 16);
            assert_eq!(q.abs_diff(40_680_631_590_769), deviation);
        }
        let locations = cities.map(|(x, y, z, _)| Location::try_new(x, y, z).unwrap());
        assert_eq!(
            distance_squared(&locations[0], &locations[2]),
            35_726_222_993_250
        );
        assert_eq!(
            distance_squared(&locations[0], &locations[3]),
            72_970_699_340_708
        );
        assert_eq!(distance_squared(&locations[0], &locations[0]), 0);
        assert_eq!(
            distance_squared(&locations[2], &locations[0]),
            distance_squared(&locations[0], &locations[2])
        );
        for (coords, valid) in [
            ((6_378_138, 0, 0), true),
            ((0, -6_378_138, 0), true),
            ((6_378_139, 0, 0), false),
            ((0, -6_378_139, 0), false),
            ((4_875_666, 4_113_547, 2), true),
            ((4_719_095, 4_292_264, 7), false),
            ((4_846_432, 4_144_873, 4), true),
            ((4_554_754, 4_463_417, 19), false),
            ((0, 0, 0), false),
            ((0, 0, i32::MAX), false),
            ((i32::MIN, 0, 0), false),
        ] {
            assert_eq!(
                Location::try_new(coords.0, coords.1, coords.2).is_some(),
                valid,
                "{coords:?}"
            );
        }
    }

    #[test]
    fn views_rewards_and_snapshots_preserve_location() {
        use {
            crate::{
                vote_account::{VoteAccount, VoteAccounts},
                vote_state_view::VoteStateView,
                vote_state_view_mut::VoteStateViewMut,
            },
            solana_account::{AccountSharedData, ReadableAccount},
            std::{collections::HashMap, sync::Arc},
        };
        let state = state();
        let mut bytes = data(&state);
        let snapshot = VoteStateView::try_new(Arc::new(bytes.clone())).unwrap();
        assert_eq!(snapshot.node_pubkey(), &state.node_pubkey);
        assert_eq!(snapshot.pending_delegator_rewards(), 123);
        assert_eq!(
            snapshot.bls_pubkey_compressed(),
            Some(state.bls_pubkey_compressed)
        );
        assert_eq!(
            snapshot.get_authorized_voter(0),
            Some(&state.authorized_voters_ref().0[0].pubkey)
        );
        assert_eq!(
            snapshot
                .epoch_credits_iter()
                .map(<(u64, u64, u64)>::from)
                .collect::<Vec<_>>(),
            vec![(0, 20, 0)]
        );
        let mut mutable = VoteStateViewMut::new_v4_or_v5(&mut bytes).unwrap();
        mutable
            .increment_pending_delegator_rewards_checked(50)
            .unwrap();
        assert_eq!(mutable.reset_pending_delegator_rewards(), 173);
        mutable
            .increment_pending_delegator_rewards_checked(123)
            .unwrap();
        assert_eq!(
            VoteStateV5::from_account_data_unaligned(&bytes).unwrap(),
            state
        );
        let account = VoteAccount::try_from(AccountSharedData::new_with_data(
            100,
            bytes.clone(),
            &solana_sdk_ids::vote::id(),
        ))
        .unwrap();
        let accounts = VoteAccounts::from(Arc::new(HashMap::from([(
            Pubkey::new_unique(),
            (10, account),
        )])));
        let encoded = wincode::serialize(&accounts).unwrap();
        let restored: VoteAccounts = wincode::deserialize(&encoded).unwrap();
        let restored = restored.iter().next().unwrap().1;
        assert_eq!(restored.account().data(), bytes);
        assert_eq!(restored.vote_state_view().location(), Some(state.location));
        assert_eq!(snapshot.location(), Some(state.location));
        for offset in [618, 950] {
            let mut invalid = bytes.clone();
            invalid[offset] ^= 0xff;
            assert!(VoteStateView::try_new(Arc::new(invalid)).is_err());
        }
    }

    #[test]
    fn admission_filters_before_the_stake_cutoff() {
        use {
            crate::vote_account::{VoteAccount, VoteAccounts},
            solana_account::AccountSharedData,
            std::collections::HashMap,
        };
        let registered = state();
        let registered_key = Pubkey::new_unique();
        let unregistered_key = Pubkey::new_unique();
        let registered = VoteAccount::try_from(AccountSharedData::new_with_data(
            20,
            data(&registered),
            &solana_sdk_ids::vote::id(),
        ))
        .unwrap();
        let legacy = VoteAccount::try_from(AccountSharedData::new_with_data(
            100,
            wincode::serialize(&solana_vote_interface::state::VoteStateVersions::new_v4(
                common_fields(&state()),
            ))
            .unwrap(),
            &solana_sdk_ids::vote::id(),
        ))
        .unwrap();
        let accounts = VoteAccounts::from(std::sync::Arc::new(HashMap::from([
            (registered_key, (10, registered)),
            (unregistered_key, (100, legacy)),
        ])));
        let before = accounts.clone_and_filter_for_vat_with_location(1, 30, 20, false, false);
        assert!(before.get(&unregistered_key).is_some());
        let after = accounts.clone_and_filter_for_vat_with_location(1, 30, 20, false, true);
        assert_eq!(after.len(), 1);
        assert!(after.get(&registered_key).is_some());
        assert_eq!(after.get_delegated_stake(&unregistered_key), 0);
        assert!(accounts.get(&unregistered_key).is_some());
    }
}
