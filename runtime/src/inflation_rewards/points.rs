//! Information about points calculation based on stake state.

use {
    crate::reward_epoch_delegated_stakes::RewardEpochDelegatedStakes,
    log::error,
    solana_clock::Epoch,
    solana_pubkey::Pubkey,
    solana_stake_history::StakeHistory,
    solana_stake_interface::state::{Delegation, Stake},
    solana_vote::vote_state_view::VoteStateView,
    std::cmp::Ordering,
};

/// captures a rewards round as lamports to be awarded
///  and the total points over which those lamports
///  are to be distributed
//  basically read as rewards/points, but in integers instead of as an f64
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointValue {
    pub rewards: u64, // lamports to split
    pub points: u128, // over these points
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CalculatedStakePoints {
    pub(crate) ag_points: u128,
    pub(crate) new_credits_observed: u64,
    pub(crate) force_credits_update_with_skipped_reward: bool,
}

/// Combination of info needed to calculate rewards
pub(crate) struct CalculationEnvironment<'a> {
    pub(crate) rewarded_epoch: Epoch,
    pub(crate) point_value: &'a PointValue,
    pub(crate) stake_history: &'a StakeHistory,
    pub(crate) new_rate_activation_epoch: Option<Epoch>,
    pub(crate) commission_rate_in_basis_points: bool,
    pub(crate) adjust_delegations_for_rent: bool,
}

#[derive(Debug)]
pub enum InflationPointCalculationEvent {
    CalculatedPoints(u64, u128, u128, u128),
    SplitRewards(u64, u64, u64, PointValue),
    EffectiveStakeAtRewardedEpoch(u64),
    PriorTotalLamports(u64),
    Delegation(Delegation, Pubkey),
    /// Commission as a percentage (0-100).
    Commission(u8),
    /// Commission in basis points (0-10,000 representing 0-100%).
    /// Used when `commission_rate_in_basis_points` feature is active.
    CommissionBps(u16),
    CreditsObserved(u64, Option<u64>),
    Skipped(SkippedReason),
}

#[cfg(test)]
pub(crate) fn null_tracer() -> Option<impl Fn(&InflationPointCalculationEvent)> {
    None::<fn(&_)>
}

#[derive(Debug)]
pub enum SkippedReason {
    DisabledInflation,
    JustActivated,
    ZeroPoints,
    ZeroReward,
    ZeroCreditsAndReturnZero,
    ZeroCreditsAndReturnCurrent,
    ZeroCreditsAndReturnRewound,
}

impl From<SkippedReason> for InflationPointCalculationEvent {
    fn from(reason: SkippedReason) -> Self {
        InflationPointCalculationEvent::Skipped(reason)
    }
}

// DEVELOPER NOTE: The commission is intentionally not included here because it
// is determined from past epoch vote state.
pub(crate) struct DelegatedVoteState<'a> {
    pub(crate) credits: u64,
    pub(crate) epoch_credits_iter: Box<dyn Iterator<Item = (Epoch, u64, u64)> + 'a>,
}

impl<'a> From<&'a VoteStateView> for DelegatedVoteState<'a> {
    fn from(vote_state: &'a VoteStateView) -> Self {
        DelegatedVoteState {
            credits: vote_state.credits(),
            epoch_credits_iter: Box::new(vote_state.epoch_credits_iter().map(Into::into)),
        }
    }
}

fn record_error(msg: String) {
    error!("{msg}");
    datapoint_error!(
        "PER-total-stake-calculation-failure",
        ("error", msg, String)
    );
}

/// Returns how many credits the stake actually earned based on what it has already observed and
/// the vote account has recorded.
///
/// Also updates the `new_credits_observed` as a side effect.
fn calc_earned_credits(
    stake: &Stake,
    final_epoch_credits: u64,
    initial_epoch_credits: u64,
    new_credits_observed: &mut u64,
) -> u128 {
    let credits_in_stake = stake.credits_observed;

    // figure out how much this stake has seen that
    //   for which the vote account has a record
    let earned_credits = if credits_in_stake < initial_epoch_credits {
        // the staker observed the entire epoch
        final_epoch_credits - initial_epoch_credits
    } else if credits_in_stake < final_epoch_credits {
        // the staker registered sometime during the epoch, partial credit
        final_epoch_credits - *new_credits_observed
    } else {
        // the staker has already observed or been redeemed this epoch
        //  or was activated after this epoch
        0
    };
    *new_credits_observed = (*new_credits_observed).max(final_epoch_credits);
    u128::from(earned_credits)
}

/// Calculate alpenglow points for `stake` based on the vote account's `reward_epoch_credits`
///
/// This value is the lamports paid to the vote account * `stake_amount` / `vote_account_stake`
/// `vote_account_stake` is fetched from the precomputed `reward_epoch_delegated_stakes` for the
/// reward epoch
///
/// Returns (alpenglow points, new_credits_observed)
fn calculate_alpenglow_points(
    stake: &Stake,
    reward_epoch_credits: Option<(Epoch, u64, u64)>,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
    reward_epoch_delegated_stakes: &RewardEpochDelegatedStakes,
) -> Result<(u128, u64), CalculatedStakePoints> {
    let Some((epoch, final_epoch_credits, initial_epoch_credits)) = reward_epoch_credits else {
        return Ok((0, stake.credits_observed));
    };
    if epoch != reward_epoch_delegated_stakes.epoch {
        // In this case, the vote account did not record any credits in this epoch
        // The latest entry is from a prior epoch - thus the delegation gets 0 rewards
        return Ok((0, stake.credits_observed));
    }

    let (earned_credits, new_credits_observed) = {
        let mut new_credits_observed = stake.credits_observed;
        let earned_credits = calc_earned_credits(
            stake,
            final_epoch_credits,
            initial_epoch_credits,
            &mut new_credits_observed,
        );
        (earned_credits, new_credits_observed)
    };

    let stake_amount = u128::from(stake.delegation.stake_v2(
        epoch,
        stake_history,
        new_rate_activation_epoch,
    ));

    let earned_points = if earned_credits == 0 || stake_amount == 0 {
        0
    } else {
        let Some(total_stake) = reward_epoch_delegated_stakes
            .delegated_stakes
            .get(&stake.delegation.voter_pubkey)
            .copied()
            .filter(|stake| *stake != 0)
        else {
            record_error(format!(
                "AG delegated stake denominator for vote_pubkey={} in epoch={} failed",
                stake.delegation.voter_pubkey, reward_epoch_delegated_stakes.epoch
            ));
            return Err(CalculatedStakePoints {
                ag_points: 0,
                new_credits_observed,
                force_credits_update_with_skipped_reward: true,
            });
        };
        earned_credits * stake_amount / total_stake as u128
    };

    if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
        inflation_point_calc_tracer(&InflationPointCalculationEvent::CalculatedPoints(
            epoch,
            stake_amount,
            earned_credits,
            earned_points,
        ));
    }
    Ok((earned_points, new_credits_observed))
}

/// for a given stake and vote_state, calculate how many
///   points were earned (credits * stake) and new value
///   for credits_observed were the points paid
pub(crate) fn calculate_stake_points_and_credits(
    stake: &Stake,
    vote_state: DelegatedVoteState,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
    reward_epoch_delegated_stakes: &RewardEpochDelegatedStakes,
) -> CalculatedStakePoints {
    let credits_in_stake = stake.credits_observed;
    let credits_in_vote = vote_state.credits;
    // if there is no newer credits since observed, return no point
    match credits_in_vote.cmp(&credits_in_stake) {
        Ordering::Less => {
            if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
                inflation_point_calc_tracer(&SkippedReason::ZeroCreditsAndReturnRewound.into());
            }
            // Don't adjust stake.activation_epoch for simplicity:
            //  - generally fast-forwarding stake.activation_epoch forcibly (for
            //    artificial re-activation with re-warm-up) skews the stake
            //    history sysvar. And properly handling all the cases
            //    regarding deactivation epoch/warm-up/cool-down without
            //    introducing incentive skew is hard.
            //  - Conceptually, it should be acceptable for the staked SOLs at
            //    the recreated vote to receive rewards again immediately after
            //    rewind even if it looks like instant activation. That's
            //    because it must have passed the required warmed-up at least
            //    once in the past already
            //  - Also such a stake account remains to be a part of overall
            //    effective stake calculation even while the vote account is
            //    missing for (indefinite) time or remains to be pre-remove
            //    credits score. It should be treated equally to staking with
            //    delinquent validator with no differentiation.

            // hint with true to indicate some exceptional credits handling is needed
            return CalculatedStakePoints {
                ag_points: 0,
                new_credits_observed: credits_in_vote,
                force_credits_update_with_skipped_reward: true,
            };
        }
        Ordering::Equal => {
            if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
                inflation_point_calc_tracer(&SkippedReason::ZeroCreditsAndReturnCurrent.into());
            }
            // don't hint caller and return current value if credits remain unchanged (= delinquent)
            return CalculatedStakePoints {
                ag_points: 0,
                new_credits_observed: credits_in_stake,
                force_credits_update_with_skipped_reward: false,
            };
        }
        Ordering::Greater => {}
    }

    let (ag_points, new_credits_observed) = match calculate_alpenglow_points(
        stake,
        vote_state.epoch_credits_iter.last(),
        stake_history,
        inflation_point_calc_tracer,
        new_rate_activation_epoch,
        reward_epoch_delegated_stakes,
    ) {
        Ok(result) => result,
        Err(e) => return e,
    };
    CalculatedStakePoints {
        ag_points,
        new_credits_observed,
        force_credits_update_with_skipped_reward: false,
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_vote_program::vote_state::{VoteStateV4, handler::VoteStateHandler},
        std::cell::Cell,
    };

    impl<'a> From<&'a VoteStateV4> for DelegatedVoteState<'a> {
        fn from(vote_state: &'a VoteStateV4) -> Self {
            DelegatedVoteState {
                credits: vote_state.credits(),
                epoch_credits_iter: Box::new(vote_state.epoch_credits.iter().copied()),
            }
        }
    }

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
    fn test_calculate_alpenglow_points() {
        let stake_lamports = 10_000_000 * LAMPORTS_PER_SOL;
        let total_stake = stake_lamports * 2;
        let credits = 1235;

        let stake = new_stake(
            stake_lamports,
            &Pubkey::default(),
            VoteStateHandler::new_v4(VoteStateV4::default()).as_ref_v4(),
            u64::MAX,
        );

        let reward_epoch_delegated_stakes = RewardEpochDelegatedStakes {
            epoch: 1,
            delegated_stakes: [(Pubkey::default(), total_stake)].into_iter().collect(),
        };

        let epoch_credits = vec![(0, credits, 0), (1, credits * 2, credits)];
        let (points, new_credits) = calculate_alpenglow_points(
            &stake,
            epoch_credits.into_iter().last(),
            &StakeHistory::default(),
            null_tracer(),
            None,
            &reward_epoch_delegated_stakes,
        )
        .unwrap();
        assert_eq!(
            points,
            credits as u128 * stake_lamports as u128 / total_stake as u128
        );
        assert_eq!(new_credits, credits * 2);

        let epoch_credits = vec![
            (0, credits, 0),
            (0, credits * 2, credits),
            (1, credits * 3, credits * 2),
        ];
        let (points, new_credits) = calculate_alpenglow_points(
            &stake,
            epoch_credits.into_iter().last(),
            &StakeHistory::default(),
            null_tracer(),
            None,
            &reward_epoch_delegated_stakes,
        )
        .unwrap();
        assert_eq!(
            points,
            credits as u128 * stake_lamports as u128 / total_stake as u128
        );
        assert_eq!(new_credits, credits * 3);

        let missing_reward_epoch_delegated_stakes = RewardEpochDelegatedStakes {
            epoch: 2,
            delegated_stakes: [(Pubkey::default(), total_stake)].into_iter().collect(),
        };
        let epoch_credits = vec![(0, credits, 0), (1, credits * 2, credits)];
        let (points, new_credits) = calculate_alpenglow_points(
            &stake,
            epoch_credits.into_iter().last(),
            &StakeHistory::default(),
            null_tracer(),
            None,
            &missing_reward_epoch_delegated_stakes,
        )
        .unwrap();
        assert_eq!(points, 0);
        assert_eq!(new_credits, 0);
    }

    #[test]
    fn test_calculate_alpenglow_points_uses_current_delegated_stake_denominator() {
        let stake_lamports = 200;
        let current_delegated_total = 200;
        let credits = 10;

        let stake = Stake {
            delegation: Delegation {
                voter_pubkey: Pubkey::default(),
                stake: stake_lamports,
                activation_epoch: u64::MAX,
                deactivation_epoch: u64::MAX,
                ..Default::default()
            },
            credits_observed: 0,
        };

        let reward_epoch_delegated_stakes = RewardEpochDelegatedStakes {
            epoch: 1,
            delegated_stakes: [(Pubkey::default(), current_delegated_total)]
                .into_iter()
                .collect(),
        };
        let epoch_credits = vec![
            (0, credits, 0),
            (0, credits * 2, credits),
            (1, credits * 3, credits * 2),
        ];

        let (points, new_credits) = calculate_alpenglow_points(
            &stake,
            epoch_credits.into_iter().last(),
            &StakeHistory::default(),
            null_tracer(),
            None,
            &reward_epoch_delegated_stakes,
        )
        .unwrap();

        assert_eq!(points, credits as u128);
        assert_eq!(new_credits, credits * 3);
    }

    #[test]
    fn test_alpenglow_uses_rewarded_epoch_credits_only() {
        let stake_lamports = 10_000_000 * LAMPORTS_PER_SOL;
        let total_stake = stake_lamports * 2;
        let credits = 1235;

        let stake = Stake {
            delegation: Delegation {
                voter_pubkey: Pubkey::default(),
                stake: stake_lamports,
                activation_epoch: u64::MAX,
                deactivation_epoch: u64::MAX,
                ..Default::default()
            },
            credits_observed: 0,
        };
        let vote_state = VoteStateV4 {
            epoch_credits: (1..=64)
                .map(|epoch| {
                    let initial_credits = (epoch - 1) * credits;
                    (epoch, initial_credits + credits, initial_credits)
                })
                .collect(),
            ..VoteStateV4::default()
        };

        let reward_epoch_delegated_stakes = RewardEpochDelegatedStakes {
            epoch: 64,
            delegated_stakes: [(Pubkey::default(), total_stake)].into_iter().collect(),
        };

        let calculated_points_events = Cell::new(0);
        let tracer = |event: &InflationPointCalculationEvent| {
            if matches!(event, InflationPointCalculationEvent::CalculatedPoints(..)) {
                calculated_points_events.set(calculated_points_events.get() + 1);
            }
        };

        let calculated_stake_points = calculate_stake_points_and_credits(
            &stake,
            DelegatedVoteState::from(&vote_state),
            &StakeHistory::default(),
            Some(tracer),
            None,
            &reward_epoch_delegated_stakes,
        );

        assert_eq!(calculated_points_events.get(), 1);
        assert_eq!(
            calculated_stake_points,
            CalculatedStakePoints {
                ag_points: credits as u128 * stake_lamports as u128 / total_stake as u128,
                new_credits_observed: credits * 64,
                force_credits_update_with_skipped_reward: false,
            }
        );
    }

    #[test]
    fn test_alpenglow_rewarded_epoch_uses_delegated_stake_denominator() {
        let stake_lamports = 200;
        let current_delegated_total = 200;
        let credits = 10;

        let stake = Stake {
            delegation: Delegation {
                voter_pubkey: Pubkey::default(),
                stake: stake_lamports,
                activation_epoch: u64::MAX,
                deactivation_epoch: u64::MAX,
                ..Default::default()
            },
            credits_observed: 0,
        };
        let vote_state = VoteStateV4 {
            epoch_credits: vec![(1, credits, 0)],
            ..VoteStateV4::default()
        };

        let reward_epoch_delegated_stakes = RewardEpochDelegatedStakes {
            epoch: 1,
            delegated_stakes: [(Pubkey::default(), current_delegated_total)]
                .into_iter()
                .collect(),
        };

        let calculated_stake_points = calculate_stake_points_and_credits(
            &stake,
            DelegatedVoteState::from(&vote_state),
            &StakeHistory::default(),
            null_tracer(),
            None,
            &reward_epoch_delegated_stakes,
        );

        assert_eq!(
            calculated_stake_points,
            CalculatedStakePoints {
                ag_points: credits as u128,
                new_credits_observed: credits,
                force_credits_update_with_skipped_reward: false,
            }
        );
    }

    #[test]
    fn test_changing_total_stake() {
        let pubkey = Pubkey::new_unique();
        let staker_delegation = LAMPORTS_PER_SOL;
        let reward_epoch_validator_stake = staker_delegation * 5;
        let stake = Stake {
            delegation: Delegation {
                voter_pubkey: pubkey,
                stake: staker_delegation,
                activation_epoch: u64::MAX,
                deactivation_epoch: u64::MAX,
                ..Default::default()
            },
            credits_observed: 0,
        };

        let credits = 1235;
        let epoch_credits = vec![
            (0, credits, 0),
            (0, credits * 2, credits),
            (1, credits * 3, credits * 2),
            (2, credits * 4, credits * 3),
        ];
        let reward_epoch_delegated_stakes = RewardEpochDelegatedStakes {
            epoch: 2,
            delegated_stakes: [(pubkey, reward_epoch_validator_stake)]
                .into_iter()
                .collect(),
        };
        let (points, new_credits) = calculate_alpenglow_points(
            &stake,
            epoch_credits.into_iter().last(),
            &StakeHistory::default(),
            null_tracer(),
            None,
            &reward_epoch_delegated_stakes,
        )
        .unwrap();
        assert_eq!(new_credits, credits * 4);
        let expected_points = credits * staker_delegation / reward_epoch_validator_stake;
        assert_eq!(points, expected_points as u128);
    }

    #[test]
    fn test_stake_activating_deactivating() {
        let stake_lamports = 10_000_000 * LAMPORTS_PER_SOL;
        let credits = 1235;

        for (activation_epoch, deactivation_epoch) in [(0, u64::MAX), (u64::MAX, 0)] {
            let stake = Stake {
                delegation: Delegation {
                    voter_pubkey: Pubkey::default(),
                    stake: stake_lamports,
                    activation_epoch,
                    deactivation_epoch,
                    ..Default::default()
                },
                credits_observed: 0,
            };

            let total_stake = stake_lamports * 2;
            let epoch_credits = vec![
                (0, credits, 0),
                (0, credits * 2, credits),
                (1, credits * 3, credits * 2),
            ];
            let reward_epoch_delegated_stakes = RewardEpochDelegatedStakes {
                epoch: 1,
                delegated_stakes: [(Pubkey::default(), total_stake)].into_iter().collect(),
            };
            let (points, new_credits) = calculate_alpenglow_points(
                &stake,
                epoch_credits.into_iter().last(),
                &StakeHistory::default(),
                null_tracer(),
                None,
                &reward_epoch_delegated_stakes,
            )
            .unwrap();
            let expected_points = if activation_epoch == 0 {
                credits as u128 * stake_lamports as u128 / total_stake as u128
            } else {
                0
            };
            assert_eq!(points, expected_points);
            assert_eq!(new_credits, credits * 3);
        }
    }
}
