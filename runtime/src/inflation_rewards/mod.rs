//! Information about stake and voter rewards based on stake state.

use {
    self::points::{
        calculate_stake_points_and_credits, CalculatedStakePoints, InflationPointCalculationEvent,
        PointValue, SkippedReason,
    },
    solana_clock::Epoch,
    solana_instruction::error::InstructionError,
    solana_stake_interface::{error::StakeError, stake_history::StakeHistory},
    solana_stake_program::stake_state::{Stake, StakeStateV2},
    solana_vote::vote_state_view::VoteStateView,
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
pub fn redeem_rewards(
    rewarded_epoch: Epoch,
    stake_state: &StakeStateV2,
    vote_state: &VoteStateView,
    point_value: &PointValue,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
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
            inflation_point_calc_tracer(&InflationPointCalculationEvent::Commission(
                vote_state.commission(),
            ));
        }

        let mut stake = *stake;
        if let Some((stakers_reward, voters_reward)) = redeem_stake_rewards(
            rewarded_epoch,
            &mut stake,
            point_value,
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
    vote_state: &VoteStateView,
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
    vote_state: &VoteStateView,
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
    let (voter_rewards, staker_rewards, is_split) =
        commission_split(vote_state.commission(), rewards);
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
fn commission_split(commission: u8, on: u64) -> (u64, u64, bool) {
    match commission.min(100) {
        0 => (0, on, false),
        100 => (on, 0, false),
        split => {
            let on = u128::from(on);
            // Calculate mine and theirs independently and symmetrically instead of
            // using the remainder of the other to treat them strictly equally.
            // This is also to cancel the rewarding if either of the parties
            // should receive only fractional lamports, resulting in not being rewarded at all.
            // Thus, note that we intentionally discard any residual fractional lamports.
            let mine = on
                .checked_mul(u128::from(split))
                .expect("multiplication of a u64 and u8 should not overflow")
                / 100u128;
            let theirs = on
                .checked_mul(u128::from(
                    100u8
                        .checked_sub(split)
                        .expect("commission cannot be greater than 100"),
                ))
                .expect("multiplication of a u64 and u8 should not overflow")
                / 100u128;

            (mine as u64, theirs as u64, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        self::points::null_tracer, super::*, solana_native_token::LAMPORTS_PER_SOL,
        solana_pubkey::Pubkey, solana_stake_interface::state::Delegation,
        solana_vote_program::vote_state::VoteStateV3, test_case::test_case,
    };

    fn new_stake(
        stake: u64,
        voter_pubkey: &Pubkey,
        vote_state: &VoteStateV3,
        activation_epoch: Epoch,
    ) -> Stake {
        Stake {
            delegation: Delegation::new(voter_pubkey, stake, activation_epoch),
            credits_observed: vote_state.credits(),
        }
    }

    #[test]
    fn test_stake_state_redeem_rewards() {
        let mut vote_state = VoteStateV3::default();
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
                &VoteStateView::from(vote_state.clone()),
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
                &VoteStateView::from(vote_state),
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
        let mut vote_state = VoteStateV3::default();
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
                &VoteStateView::from(vote_state.clone()),
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
                &VoteStateView::from(vote_state.clone()),
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
                &VoteStateView::from(vote_state.clone()),
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
                &VoteStateView::from(vote_state.clone()),
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
                &VoteStateView::from(vote_state.clone()),
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
                &VoteStateView::from(vote_state.clone()),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );

        // same as above, but is a really small commission out of 32 bits,
        //  verify that None comes back on small redemptions where no one gets paid
        vote_state.commission = 1;
        assert_eq!(
            None, // would be Some((0, 2 * 1 + 1 * 2, 4)),
            calculate_stake_rewards(
                2,
                &stake,
                &PointValue {
                    rewards: 4,
                    points: 4
                },
                &VoteStateView::from(vote_state.clone()),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );
        vote_state.commission = 99;
        assert_eq!(
            None, // would be Some((0, 2 * 1 + 1 * 2, 4)),
            calculate_stake_rewards(
                2,
                &stake,
                &PointValue {
                    rewards: 4,
                    points: 4
                },
                &VoteStateView::from(vote_state.clone()),
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
                &VoteStateView::from(vote_state.clone()),
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
                &VoteStateView::from(vote_state.clone()),
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
                &VoteStateView::from(vote_state.clone()),
                &StakeHistory::default(),
                null_tracer(),
                None
            )
        );

        // credits_observed is auto-rewinded when vote_state credits are assumed to have been
        // recreated
        stake.credits_observed = 1000;
        // this is new behavior 1; return the post-recreation rewinded credits from the vote account
        assert_eq!(
            CalculatedStakePoints {
                points: 0,
                new_credits_observed: 4,
                force_credits_update_with_skipped_reward: true,
            },
            calculate_stake_points_and_credits(
                &stake,
                &VoteStateView::from(vote_state.clone()),
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
                &VoteStateView::from(vote_state.clone()),
                &StakeHistory::default(),
                null_tracer(),
                None
            )
        );

        // get rewards and credits observed when not the activation epoch
        vote_state.commission = 0;
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
                &VoteStateView::from(vote_state.clone()),
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
                &VoteStateView::from(vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );
    }

    #[test_case(u64::MAX, 1_000, u64::MAX => panics "Rewards intermediate calculation should fit within u128")]
    #[test_case(1, u64::MAX, u64::MAX => panics "Rewards should fit within u64")]
    fn calculate_rewards_tests(stake: u64, rewards: u64, credits: u64) {
        let mut vote_state = VoteStateV3::default();

        let stake = new_stake(stake, &Pubkey::default(), &vote_state, u64::MAX);

        vote_state.increment_credits(0, credits);

        calculate_stake_rewards(
            0,
            &stake,
            &PointValue { rewards, points: 1 },
            &VoteStateView::from(vote_state.clone()),
            &StakeHistory::default(),
            null_tracer(),
            None,
        );
    }

    #[test]
    fn test_stake_state_calculate_points_with_typical_values() {
        let vote_state = VoteStateV3::default();

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
                &VoteStateView::from(vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None,
            )
        );
    }

    #[test]
    fn test_commission_split() {
        let mut commission = 0;
        assert_eq!(commission_split(commission, 1), (0, 1, false));

        commission = u8::MAX;
        assert_eq!(commission_split(commission, 1), (1, 0, false));

        commission = 99;
        assert_eq!(commission_split(commission, 10), (9, 0, true));

        commission = 1;
        assert_eq!(commission_split(commission, 10), (0, 9, true));

        commission = 50;
        let (voter_portion, staker_portion, was_split) = commission_split(commission, 10);

        assert_eq!((voter_portion, staker_portion, was_split), (5, 5, true));
    }
}
