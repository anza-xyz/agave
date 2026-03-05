//! Information about stake and voter rewards based on stake state.

use {
    self::points::{
        CalculatedStakePoints, DelegatedVoteState, InflationPointCalculationEvent, PointValue,
        SkippedReason, calculate_stake_points_and_credits,
    },
    solana_clock::Epoch,
    solana_instruction::error::InstructionError,
    solana_stake_interface::{
        error::StakeError,
        stake_history::StakeHistory,
        state::{Stake, StakeStateV2},
    },
};

pub mod points;

#[derive(Debug, PartialEq, Eq)]
struct CalculatedStakeRewards {
    staker_rewards: u64,
    voter_rewards: u64,
    new_credits_observed: u64,
}

/// Redeems rewards for the given epoch, stake state and vote state.
/// Returns a tuple of:
/// * Stakers reward
/// * Voters reward
/// * Updated stake information
pub(crate) fn redeem_rewards(
    rewarded_epoch: Epoch,
    stake_state: &StakeStateV2,
    voter_commission_bps: u16,
    vote_state: DelegatedVoteState,
    point_value: &PointValue,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
    commission_rate_in_basis_points: bool,
) -> Result<(u64, u64, Stake), InstructionError> {
    if let StakeStateV2::Stake(meta, stake, _stake_flags) = stake_state {
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(
                &InflationPointCalculationEvent::EffectiveStakeAtRewardedEpoch(stake.stake(
                    rewarded_epoch,
                    stake_history,
                    new_rate_activation_epoch,
                )),
            );
            inflation_point_calc_tracer(&InflationPointCalculationEvent::RentExemptReserve(
                meta.rent_exempt_reserve,
            ));
            // Choose which trace to emit based on the `commission_rate_in_basis_points` feature.
            if commission_rate_in_basis_points {
                inflation_point_calc_tracer(&InflationPointCalculationEvent::CommissionBps(
                    voter_commission_bps,
                ));
            } else {
                inflation_point_calc_tracer(&InflationPointCalculationEvent::Commission(
                    (voter_commission_bps / 100) as u8,
                ));
            }
        }

        let mut stake = *stake;
        if let Some((stakers_reward, voters_reward)) = redeem_stake_rewards(
            rewarded_epoch,
            &mut stake,
            point_value,
            voter_commission_bps,
            vote_state,
            stake_history,
            inflation_point_calc_tracer,
            new_rate_activation_epoch,
        ) {
            Ok((stakers_reward, voters_reward, stake))
        } else {
            Err(StakeError::NoCreditsToRedeem.into())
        }
    } else {
        Err(InstructionError::InvalidAccountData)
    }
}

fn redeem_stake_rewards(
    rewarded_epoch: Epoch,
    stake: &mut Stake,
    point_value: &PointValue,
    voter_commission_bps: u16,
    vote_state: DelegatedVoteState,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
) -> Option<(u64, u64)> {
    if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
        inflation_point_calc_tracer(&InflationPointCalculationEvent::CreditsObserved(
            stake.credits_observed,
            None,
        ));
    }
    calculate_stake_rewards(
        rewarded_epoch,
        stake,
        point_value,
        voter_commission_bps,
        vote_state,
        stake_history,
        inflation_point_calc_tracer.as_ref(),
        new_rate_activation_epoch,
    )
    .map(|calculated_stake_rewards| {
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer {
            inflation_point_calc_tracer(&InflationPointCalculationEvent::CreditsObserved(
                stake.credits_observed,
                Some(calculated_stake_rewards.new_credits_observed),
            ));
        }
        stake.credits_observed = calculated_stake_rewards.new_credits_observed;
        stake.delegation.stake += calculated_stake_rewards.staker_rewards;
        (
            calculated_stake_rewards.staker_rewards,
            calculated_stake_rewards.voter_rewards,
        )
    })
}

/// for a given stake and vote_state, calculate what distributions and what updates should be made
/// returns a tuple in the case of a payout of:
///   * staker_rewards to be distributed
///   * voter_rewards to be distributed
///   * new value for credits_observed in the stake
///
/// returns None if there's no payout or if any deserved payout is < 1 lamport
fn calculate_stake_rewards(
    rewarded_epoch: Epoch,
    stake: &Stake,
    point_value: &PointValue,
    voter_commission_bps: u16,
    vote_state: DelegatedVoteState,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
) -> Option<CalculatedStakeRewards> {
    // ensure to run to trigger (optional) inflation_point_calc_tracer
    let CalculatedStakePoints {
        points,
        new_credits_observed,
        mut force_credits_update_with_skipped_reward,
    } = calculate_stake_points_and_credits(
        stake,
        vote_state,
        stake_history,
        inflation_point_calc_tracer.as_ref(),
        new_rate_activation_epoch,
    );

    // Drive credits_observed forward unconditionally when rewards are disabled
    // or when this is the stake's activation epoch
    if point_value.rewards == 0 {
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(&SkippedReason::DisabledInflation.into());
        }
        force_credits_update_with_skipped_reward = true;
    } else if stake.delegation.activation_epoch == rewarded_epoch {
        // not assert!()-ed; but points should be zero
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(&SkippedReason::JustActivated.into());
        }
        force_credits_update_with_skipped_reward = true;
    }

    if force_credits_update_with_skipped_reward {
        return Some(CalculatedStakeRewards {
            staker_rewards: 0,
            voter_rewards: 0,
            new_credits_observed,
        });
    }

    if points == 0 {
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(&SkippedReason::ZeroPoints.into());
        }
        return None;
    }
    if point_value.points == 0 {
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(&SkippedReason::ZeroPointValue.into());
        }
        return None;
    }

    // The final unwrap is safe, as points_value.points is guaranteed to be non zero above.
    let rewards = points
        .checked_mul(u128::from(point_value.rewards))
        .expect("Rewards intermediate calculation should fit within u128")
        .checked_div(point_value.points)
        .unwrap();

    let rewards = u64::try_from(rewards).expect("Rewards should fit within u64");

    // don't bother trying to split if fractional lamports got truncated
    if rewards == 0 {
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(&SkippedReason::ZeroReward.into());
        }
        return None;
    }
    let (voter_rewards, staker_rewards, is_split) = commission_split(voter_commission_bps, rewards);
    if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
        inflation_point_calc_tracer(&InflationPointCalculationEvent::SplitRewards(
            rewards,
            voter_rewards,
            staker_rewards,
            (*point_value).clone(),
        ));
    }

    if (voter_rewards == 0 || staker_rewards == 0) && is_split {
        // don't collect if we lose a whole lamport somewhere
        //  is_split means there should be tokens on both sides,
        //  uncool to move credits_observed if one side didn't get paid
        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(&SkippedReason::TooEarlyUnfairSplit.into());
        }
        return None;
    }

    Some(CalculatedStakeRewards {
        staker_rewards,
        voter_rewards,
        new_credits_observed,
    })
}

/// returns commission split as (voter_portion, staker_portion, was_split) tuple
///
///  if commission calculation is 100% one way or other,
///   indicate with false for was_split
///
/// DEVELOPER NOTE:  This function used to be a method on VoteState, but was moved here
fn commission_split(commission_bps: u16, on: u64) -> (u64, u64, bool) {
    const MAX_BPS: u16 = 10_000;
    const MAX_BPS_U128: u128 = MAX_BPS as u128;
    match commission_bps.min(MAX_BPS) {
        0 => (0, on, false),
        MAX_BPS => (on, 0, false),
        split => {
            let on = u128::from(on);
            // Calculate mine and theirs independently and symmetrically instead of
            // using the remainder of the other to treat them strictly equally.
            // This is also to cancel the rewarding if either of the parties
            // should receive only fractional lamports, resulting in not being rewarded at all.
            // Thus, note that we intentionally discard any residual fractional lamports.
            let mine = on
                .checked_mul(u128::from(split))
                .expect("multiplication of a u64 and u16 should not overflow")
                / MAX_BPS_U128;
            let theirs = on
                .checked_mul(u128::from(
                    MAX_BPS
                        .checked_sub(split)
                        .expect("commission cannot be greater than MAX_BPS"),
                ))
                .expect("multiplication of a u64 and u16 should not overflow")
                / MAX_BPS_U128;

            (mine as u64, theirs as u64, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        self::points::null_tracer,
        super::*,
        proptest::prelude::*,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_pubkey::Pubkey,
        solana_stake_interface::state::Delegation,
        solana_vote_program::vote_state::{VoteStateV4, handler::VoteStateHandle},
        test_case::test_case,
    };

    fn new_stake(
        stake: u64,
        voter_pubkey: &Pubkey,
        vote_state: &VoteStateV4,
        activation_epoch: Epoch,
    ) -> Stake {
        Stake {
            delegation: Delegation::new(voter_pubkey, stake, activation_epoch),
            credits_observed: vote_state.credits(),
        }
    }

    #[test]
    fn test_stake_state_redeem_rewards() {
        let mut vote_state = VoteStateV4::default();
        // assume stake.stake() is right
        // bootstrap means fully-vested stake at epoch 0
        let stake_lamports = 1;
        let mut stake = new_stake(stake_lamports, &Pubkey::default(), &vote_state, u64::MAX);

        // this one can't collect now, credits_observed == vote_state.credits()
        assert_eq!(
            None,
            redeem_stake_rewards(
                0,
                &mut stake,
                &PointValue {
                    rewards: 1_000_000_000,
                    points: 1
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );

        // put 2 credits in at epoch 0
        vote_state.increment_credits(0, 1);
        vote_state.increment_credits(0, 1);

        // this one should be able to collect exactly 2
        assert_eq!(
            Some((stake_lamports * 2, 0)),
            redeem_stake_rewards(
                0,
                &mut stake,
                &PointValue {
                    rewards: 1,
                    points: 1
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );

        assert_eq!(
            stake.delegation.stake,
            stake_lamports + (stake_lamports * 2)
        );
        assert_eq!(stake.credits_observed, 2);
    }

    #[test]
    fn test_stake_state_calculate_rewards() {
        let mut vote_state = VoteStateV4::default();
        // assume stake.stake() is right
        // bootstrap means fully-vested stake at epoch 0
        let mut stake = new_stake(1, &Pubkey::default(), &vote_state, u64::MAX);

        // this one can't collect now, credits_observed == vote_state.credits()
        assert_eq!(
            None,
            calculate_stake_rewards(
                0,
                &stake,
                &PointValue {
                    rewards: 1_000_000_000,
                    points: 1
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );

        // put 2 credits in at epoch 0
        vote_state.increment_credits(0, 1);
        vote_state.increment_credits(0, 1);

        // this one should be able to collect exactly 2
        assert_eq!(
            Some(CalculatedStakeRewards {
                staker_rewards: stake.delegation.stake * 2,
                voter_rewards: 0,
                new_credits_observed: 2,
            }),
            calculate_stake_rewards(
                0,
                &stake,
                &PointValue {
                    rewards: 2,
                    points: 2 // all his
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );

        stake.credits_observed = 1;
        // this one should be able to collect exactly 1 (already observed one)
        assert_eq!(
            Some(CalculatedStakeRewards {
                staker_rewards: stake.delegation.stake,
                voter_rewards: 0,
                new_credits_observed: 2,
            }),
            calculate_stake_rewards(
                0,
                &stake,
                &PointValue {
                    rewards: 1,
                    points: 1
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );

        // put 1 credit in epoch 1
        vote_state.increment_credits(1, 1);

        stake.credits_observed = 2;
        // this one should be able to collect the one just added
        assert_eq!(
            Some(CalculatedStakeRewards {
                staker_rewards: stake.delegation.stake,
                voter_rewards: 0,
                new_credits_observed: 3,
            }),
            calculate_stake_rewards(
                1,
                &stake,
                &PointValue {
                    rewards: 2,
                    points: 2
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );

        // put 1 credit in epoch 2
        vote_state.increment_credits(2, 1);
        // this one should be able to collect 2 now
        assert_eq!(
            Some(CalculatedStakeRewards {
                staker_rewards: stake.delegation.stake * 2,
                voter_rewards: 0,
                new_credits_observed: 4,
            }),
            calculate_stake_rewards(
                2,
                &stake,
                &PointValue {
                    rewards: 2,
                    points: 2
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );

        stake.credits_observed = 0;
        // this one should be able to collect everything from t=0 a warmed up stake of 2
        // (2 credits at stake of 1) + (1 credit at a stake of 2)
        assert_eq!(
            Some(CalculatedStakeRewards {
                staker_rewards: stake.delegation.stake * 2 // epoch 0
                    + stake.delegation.stake // epoch 1
                    + stake.delegation.stake, // epoch 2
                voter_rewards: 0,
                new_credits_observed: 4,
            }),
            calculate_stake_rewards(
                2,
                &stake,
                &PointValue {
                    rewards: 4,
                    points: 4
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );

        // same as above, but is a really small commission out of 32 bits,
        //  verify that None comes back on small redemptions where no one gets paid
        vote_state.inflation_rewards_commission_bps = 100;
        assert_eq!(
            None, // would be Some((0, 2 * 1 + 1 * 2, 4)),
            calculate_stake_rewards(
                2,
                &stake,
                &PointValue {
                    rewards: 4,
                    points: 4
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );
        vote_state.inflation_rewards_commission_bps = 9900;
        assert_eq!(
            None, // would be Some((0, 2 * 1 + 1 * 2, 4)),
            calculate_stake_rewards(
                2,
                &stake,
                &PointValue {
                    rewards: 4,
                    points: 4
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );

        // now one with inflation disabled. no one gets paid, but we still need
        // to advance the stake state's credits_observed field to prevent back-
        // paying rewards when inflation is turned on.
        assert_eq!(
            Some(CalculatedStakeRewards {
                staker_rewards: 0,
                voter_rewards: 0,
                new_credits_observed: 4,
            }),
            calculate_stake_rewards(
                2,
                &stake,
                &PointValue {
                    rewards: 0,
                    points: 4
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );

        // credits_observed remains at previous level when vote_state credits are
        // not advancing and inflation is disabled
        stake.credits_observed = 4;
        assert_eq!(
            Some(CalculatedStakeRewards {
                staker_rewards: 0,
                voter_rewards: 0,
                new_credits_observed: 4,
            }),
            calculate_stake_rewards(
                2,
                &stake,
                &PointValue {
                    rewards: 0,
                    points: 4
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );

        assert_eq!(
            CalculatedStakePoints {
                points: 0,
                new_credits_observed: 4,
                force_credits_update_with_skipped_reward: false,
            },
            calculate_stake_points_and_credits(
                &stake,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None
            )
        );

        // credits_observed is auto-rewound when vote_state credits are assumed to have been
        // recreated
        stake.credits_observed = 1000;
        // this is new behavior 1; return the post-recreation rewound credits from the vote account
        assert_eq!(
            CalculatedStakePoints {
                points: 0,
                new_credits_observed: 4,
                force_credits_update_with_skipped_reward: true,
            },
            calculate_stake_points_and_credits(
                &stake,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None
            )
        );
        // this is new behavior 2; don't hint when credits both from stake and vote are identical
        stake.credits_observed = 4;
        assert_eq!(
            CalculatedStakePoints {
                points: 0,
                new_credits_observed: 4,
                force_credits_update_with_skipped_reward: false,
            },
            calculate_stake_points_and_credits(
                &stake,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None
            )
        );

        // get rewards and credits observed when not the activation epoch
        vote_state.inflation_rewards_commission_bps = 0;
        stake.credits_observed = 3;
        stake.delegation.activation_epoch = 1;
        assert_eq!(
            Some(CalculatedStakeRewards {
                staker_rewards: stake.delegation.stake, // epoch 2
                voter_rewards: 0,
                new_credits_observed: 4,
            }),
            calculate_stake_rewards(
                2,
                &stake,
                &PointValue {
                    rewards: 1,
                    points: 1
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );

        // credits_observed is moved forward for the stake's activation epoch,
        // and no rewards are perceived
        stake.delegation.activation_epoch = 2;
        stake.credits_observed = 3;
        assert_eq!(
            Some(CalculatedStakeRewards {
                staker_rewards: 0,
                voter_rewards: 0,
                new_credits_observed: 4,
            }),
            calculate_stake_rewards(
                2,
                &stake,
                &PointValue {
                    rewards: 1,
                    points: 1
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );
    }

    #[test_case(u64::MAX, 1_000, u64::MAX => panics "Rewards intermediate calculation should fit within u128")]
    #[test_case(1, u64::MAX, u64::MAX => panics "Rewards should fit within u64")]
    fn calculate_rewards_tests(stake: u64, rewards: u64, credits: u64) {
        let mut vote_state = VoteStateV4::default();

        let stake = new_stake(stake, &Pubkey::default(), &vote_state, u64::MAX);

        vote_state.increment_credits(0, credits);

        calculate_stake_rewards(
            0,
            &stake,
            &PointValue { rewards, points: 1 },
            vote_state.inflation_rewards_commission_bps,
            DelegatedVoteState::from(&vote_state),
            &StakeHistory::default(),
            null_tracer(),
            None,
        );
    }

    #[test]
    fn test_stake_state_calculate_points_with_typical_values() {
        let vote_state = VoteStateV4::default();

        // bootstrap means fully-vested stake at epoch 0 with
        //  10_000_000 SOL is a big but not unreasaonable stake
        let stake = new_stake(
            10_000_000 * LAMPORTS_PER_SOL,
            &Pubkey::default(),
            &vote_state,
            u64::MAX,
        );

        // this one can't collect now, credits_observed == vote_state.credits()
        assert_eq!(
            None,
            calculate_stake_rewards(
                0,
                &stake,
                &PointValue {
                    rewards: 1_000_000_000,
                    points: 1
                },
                vote_state.inflation_rewards_commission_bps,
                DelegatedVoteState::from(&vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );
    }

    #[test]
    fn test_commission_split_bps() {
        // 0% commission
        assert_eq!(commission_split(0, 1), (0, 1, false));
        assert_eq!(commission_split(0, 10), (0, 10, false));
        assert_eq!(commission_split(0, 100), (0, 100, false));
        assert_eq!(commission_split(0, 1_000), (0, 1_000, false));
        assert_eq!(commission_split(0, u64::MAX), (0, u64::MAX, false));

        // 100% commission (10,000 bps)
        assert_eq!(commission_split(10_000, 1), (1, 0, false));
        assert_eq!(commission_split(10_000, 10), (10, 0, false));
        assert_eq!(commission_split(10_000, 100), (100, 0, false));
        assert_eq!(commission_split(10_000, 1_000), (1_000, 0, false));
        assert_eq!(commission_split(10_000, u64::MAX), (u64::MAX, 0, false));

        // Values > 10,000 bps are capped at 100%
        assert_eq!(commission_split(u16::MAX, 1), (1, 0, false));
        assert_eq!(commission_split(u16::MAX, 10), (10, 0, false));
        assert_eq!(commission_split(u16::MAX, 100), (100, 0, false));
        assert_eq!(commission_split(u16::MAX, 1_000), (1_000, 0, false));
        assert_eq!(commission_split(u16::MAX, u64::MAX), (u64::MAX, 0, false));

        // 99% commission (9,900 bps)
        assert_eq!(commission_split(9_900, 1), (0, 0, true)); // 1-lamport truncation
        assert_eq!(commission_split(9_900, 10), (9, 0, true)); // 1-lamport truncation
        assert_eq!(commission_split(9_900, 100), (99, 1, true));
        assert_eq!(commission_split(9_900, 1_000), (990, 10, true));
        assert_eq!(
            commission_split(9_900, u64::MAX),
            (
                (u64::MAX as u128 * 9_900 / 10_000) as u64,
                (u64::MAX as u128 * 100 / 10_000) as u64,
                true
            )
        ); // 1-lamport truncation

        // 99.99% commission (9,999 bps)
        assert_eq!(commission_split(9_999, 1), (0, 0, true)); // 1-lamport truncation
        assert_eq!(commission_split(9_999, 10), (9, 0, true)); // 1-lamport truncation
        assert_eq!(commission_split(9_999, 100), (99, 0, true)); // 1-lamport truncation
        assert_eq!(commission_split(9_999, 1_000), (999, 0, true)); // 1-lamport truncation
        assert_eq!(
            commission_split(9_999, u64::MAX),
            (
                (u64::MAX as u128 * 9_999 / 10_000) as u64,
                (u64::MAX as u128 / 10_000) as u64,
                true
            )
        ); // 1-lamport truncation

        // 1% commission (100 bps)
        assert_eq!(commission_split(100, 1), (0, 0, true)); // 1-lamport truncation
        assert_eq!(commission_split(100, 10), (0, 9, true)); // 1-lamport truncation
        assert_eq!(commission_split(100, 100), (1, 99, true));
        assert_eq!(commission_split(100, 1_000), (10, 990, true));
        assert_eq!(
            commission_split(100, u64::MAX),
            (
                (u64::MAX as u128 * 100 / 10_000) as u64,
                (u64::MAX as u128 * 9_900 / 10_000) as u64,
                true
            )
        ); // 1-lamport truncation

        // 50% commission (5,000 bps)
        assert_eq!(commission_split(5_000, 1), (0, 0, true)); // 1-lamport truncation
        assert_eq!(commission_split(5_000, 10), (5, 5, true));
        assert_eq!(commission_split(5_000, 100), (50, 50, true));
        assert_eq!(commission_split(5_000, 1_000), (500, 500, true));
        assert_eq!(
            commission_split(5_000, u64::MAX),
            (
                (u64::MAX as u128 * 5_000 / 10_000) as u64,
                (u64::MAX as u128 * 5_000 / 10_000) as u64,
                true
            )
        ); // 1-lamport truncation

        // 12.34% commission (1,234 bps)
        assert_eq!(commission_split(1_234, 1), (0, 0, true)); // 1-lamport truncation
        assert_eq!(commission_split(1_234, 10), (1, 8, true)); // 1-lamport truncation
        assert_eq!(commission_split(1_234, 1_000), (123, 876, true)); // 1-lamport truncation
        assert_eq!(commission_split(1_234, 10_000), (1_234, 8_766, true));
        assert_eq!(
            commission_split(1_234, u64::MAX),
            (
                (u64::MAX as u128 * 1_234 / 10_000) as u64,
                (u64::MAX as u128 * 8_766 / 10_000) as u64,
                true
            )
        ); // 1-lamport truncation

        // 33.33% commission (3,333 bps)
        assert_eq!(commission_split(3_333, 1), (0, 0, true)); // 1-lamport truncation
        assert_eq!(commission_split(3_333, 10), (3, 6, true)); // 1-lamport truncation
        assert_eq!(commission_split(3_333, 1_000), (333, 666, true)); // 1-lamport truncation
        assert_eq!(commission_split(3_333, 10_000), (3_333, 6_667, true));
        assert_eq!(
            commission_split(3_333, u64::MAX),
            (
                (u64::MAX as u128 * 3_333 / 10_000) as u64,
                (u64::MAX as u128 * 6_667 / 10_000) as u64,
                true
            )
        ); // 1-lamport truncation
    }

    proptest! {
        #[test]
        fn test_commission_split_properties(
            commission_bps in 0..=u16::MAX,
            rewards in 0..=u64::MAX,
        ) {
            let (voter, staker, was_split) = commission_split(commission_bps, rewards);

            // Invariant 1: No overflow — voter + staker never exceeds rewards.
            prop_assert!(voter + staker <= rewards);

            // Invariant 2: At most 1 lamport lost to truncation.
            prop_assert!(rewards - voter - staker <= 1);

            // Invariant 3: was_split is false only at the 0% and 100% boundaries.
            let effective_bps = commission_bps.min(10_000);
            if effective_bps == 0 || effective_bps == 10_000 {
                prop_assert!(!was_split);
            } else {
                prop_assert!(was_split);
            }

            // Invariant 4: Boundary — 0% commission gives everything to staker.
            if effective_bps == 0 {
                prop_assert_eq!(voter, 0);
                prop_assert_eq!(staker, rewards);
            }

            // Invariant 5: Boundary — 100% commission gives everything to voter.
            if effective_bps == 10_000 {
                prop_assert_eq!(voter, rewards);
                prop_assert_eq!(staker, 0);
            }

            // Invariant 6: Clamping — values above 10,000 bps behave as 10,000.
            if commission_bps > 10_000 {
                let (clamped_voter, clamped_staker, clamped_ws) =
                    commission_split(10_000, rewards);
                prop_assert_eq!(voter, clamped_voter);
                prop_assert_eq!(staker, clamped_staker);
                prop_assert_eq!(was_split, clamped_ws);
            }

            // Invariant 7: Monotonicity — higher commission means voter >= what
            // they'd get with a lower commission (for the same rewards).
            if commission_bps > 0 {
                let lower_bps = commission_bps - 1;
                let (lower_voter, _, _) = commission_split(lower_bps, rewards);
                prop_assert!(voter >= lower_voter);
            }

            // Invariant 8: Exact split when bps divides evenly.
            // voter == rewards * effective_bps / 10_000 (using u128 math).
            let expected_voter =
                (u128::from(rewards) * u128::from(effective_bps) / 10_000) as u64;
            let expected_staker =
                (u128::from(rewards) * u128::from(10_000 - effective_bps) / 10_000) as u64;
            prop_assert_eq!(voter, expected_voter);
            prop_assert_eq!(staker, expected_staker);
        }
    }
}
