//! Information about points calculation based on stake state.

use {
    solana_clock::Epoch,
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    solana_stake_interface::{
        stake_history::StakeHistory,
        state::{Delegation, Stake, StakeStateV2},
    },
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
    pub(crate) points: u128,
    pub(crate) new_credits_observed: u64,
    pub(crate) force_credits_update_with_skipped_reward: bool,
}

#[derive(Debug)]
pub enum InflationPointCalculationEvent {
    CalculatedPoints(u64, u128, u128, u128),
    SplitRewards(u64, u64, u64, PointValue),
    EffectiveStakeAtRewardedEpoch(u64),
    RentExemptReserve(u64),
    Delegation(Delegation, Pubkey),
    Commission(u8),
    CreditsObserved(u64, Option<u64>),
    Skipped(SkippedReason),
}

pub(crate) fn null_tracer() -> Option<impl Fn(&InflationPointCalculationEvent)> {
    None::<fn(&_)>
}

#[derive(Debug)]
pub enum SkippedReason {
    DisabledInflation,
    JustActivated,
    TooEarlyUnfairSplit,
    ZeroPoints,
    ZeroPointValue,
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

pub fn calculate_points(
    stake_state: &StakeStateV2,
    vote_state: &VoteStateView,
    stake_history: &StakeHistory,
    new_rate_activation_epoch: Option<Epoch>,
) -> Result<u128, InstructionError> {
    if let StakeStateV2::Stake(_meta, stake, _stake_flags) = stake_state {
        Ok(calculate_stake_points(
            stake,
            vote_state,
            stake_history,
            null_tracer(),
            new_rate_activation_epoch,
        ))
    } else {
        Err(InstructionError::InvalidAccountData)
    }
}

fn calculate_stake_points(
    stake: &Stake,
    vote_state: &VoteStateView,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
) -> u128 {
    calculate_stake_points_and_credits(
        stake,
        vote_state,
        stake_history,
        inflation_point_calc_tracer,
        new_rate_activation_epoch,
    )
    .points
}

/// for a given stake and vote_state, calculate how many
///   points were earned (credits * stake) and new value
///   for credits_observed were the points paid
pub(crate) fn calculate_stake_points_and_credits(
    stake: &Stake,
    new_vote_state: &VoteStateView,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
) -> CalculatedStakePoints {
    let credits_in_stake = stake.credits_observed;
    let credits_in_vote = new_vote_state.credits();
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
                points: 0,
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
                points: 0,
                new_credits_observed: credits_in_stake,
                force_credits_update_with_skipped_reward: false,
            };
        }
        Ordering::Greater => {}
    }

    let mut points = 0;
    let mut new_credits_observed = credits_in_stake;

    for epoch_credits_item in new_vote_state.epoch_credits_iter() {
        let (epoch, final_epoch_credits, initial_epoch_credits) = epoch_credits_item.into();
        let stake_amount = u128::from(stake.delegation.stake(
            epoch,
            stake_history,
            new_rate_activation_epoch,
        ));

        // figure out how much this stake has seen that
        //   for which the vote account has a record
        let earned_credits = if credits_in_stake < initial_epoch_credits {
            // the staker observed the entire epoch
            final_epoch_credits - initial_epoch_credits
        } else if credits_in_stake < final_epoch_credits {
            // the staker registered sometime during the epoch, partial credit
            final_epoch_credits - new_credits_observed
        } else {
            // the staker has already observed or been redeemed this epoch
            //  or was activated after this epoch
            0
        };
        let earned_credits = u128::from(earned_credits);

        // don't want to assume anything about order of the iterator...
        new_credits_observed = new_credits_observed.max(final_epoch_credits);

        // finally calculate points for this epoch
        let earned_points = stake_amount * earned_credits;
        points += earned_points;

        if let Some(inflation_point_calc_tracer) = inflation_point_calc_tracer.as_ref() {
            inflation_point_calc_tracer(&InflationPointCalculationEvent::CalculatedPoints(
                epoch,
                stake_amount,
                earned_credits,
                earned_points,
            ));
        }
    }

    CalculatedStakePoints {
        points,
        new_credits_observed,
        force_credits_update_with_skipped_reward: false,
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_native_token::LAMPORTS_PER_SOL,
        solana_vote_program::vote_state::{handler::VoteStateHandle, VoteStateV4},
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
    fn test_stake_state_calculate_points_with_typical_values() {
        let mut vote_state = VoteStateV4::default();

        // bootstrap means fully-vested stake at epoch 0 with
        //  10_000_000 SOL is a big but not unreasonable stake
        let stake = new_stake(
            10_000_000 * LAMPORTS_PER_SOL,
            &Pubkey::default(),
            &vote_state,
            u64::MAX,
        );

        let epoch_slots: u128 = 14 * 24 * 3600 * 160;
        // put 193,536,000 credits in at epoch 0, typical for a 14-day epoch
        //  this loop takes a few seconds...
        for _ in 0..epoch_slots {
            vote_state.increment_credits(0, 1);
        }

        // no overflow on points
        assert_eq!(
            u128::from(stake.delegation.stake) * epoch_slots,
            calculate_stake_points(
                &stake,
                &VoteStateView::from(vote_state),
                &StakeHistory::default(),
                null_tracer(),
                None
            )
        );
    }
}
