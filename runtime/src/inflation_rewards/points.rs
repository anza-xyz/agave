//! Information about points calculation based on stake state.

use {
    crate::{alpenglow_epoch_type::AlpenglowEpochType, epoch_stakes::VersionedEpochStakes},
    agave_votor_messages::migration::AG_MIGRATION_EPOCH_CREDIT,
    log::{error, trace},
    solana_clock::Epoch,
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    solana_stake_interface::{
        stake_history::StakeHistory,
        state::{Delegation, Stake, StakeStateV2},
    },
    solana_vote::vote_state_view::VoteStateView,
    std::{cmp::Ordering, collections::HashMap},
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
    pub(crate) tower_points: u128,
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

pub(crate) fn calculate_points_for_tower(
    stake_state: &StakeStateV2,
    vote_state: DelegatedVoteState,
    stake_history: &StakeHistory,
    new_rate_activation_epoch: Option<Epoch>,
    epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
) -> Result<u128, InstructionError> {
    if let StakeStateV2::Stake(_meta, stake, _stake_flags) = stake_state {
        Ok(calculate_stake_points_for_tower(
            stake,
            vote_state,
            stake_history,
            null_tracer(),
            new_rate_activation_epoch,
            epoch_stakes,
        ))
    } else {
        Err(InstructionError::InvalidAccountData)
    }
}

/// This function is used to calculate `PointValue::points` for tower epoch and tower portion of
/// the migration epoch.
fn calculate_stake_points_for_tower(
    stake: &Stake,
    vote_state: DelegatedVoteState,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
    epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
) -> u128 {
    calculate_stake_points_and_credits(
        stake,
        vote_state,
        stake_history,
        inflation_point_calc_tracer,
        new_rate_activation_epoch,
        &AlpenglowEpochType::Tower,
        epoch_stakes,
    )
    .tower_points
}

/// Returns the total stake delegated to `vote_pubkey` in the given `epoch`.
fn get_total_stake(
    vote_pubkey: Pubkey,
    epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
    epoch: Epoch,
) -> Result<u64, ()> {
    let Some(rank_map) = epoch_stakes.get(&epoch).map(|e| e.bls_pubkey_to_rank_map()) else {
        record_error(format!(
            "epoch_stakes.get(epoch={epoch}) for vote_pubkey={vote_pubkey} failed"
        ));
        return Err(());
    };
    let Some(rank) = rank_map.get_rank_for_vote_pubkey(&vote_pubkey) else {
        // this is benign and can happen if a staker stakes with a validator not
        // in the validator set.
        record_trace(format!(
            "get_rank_for_vote_pubkey(vote_pubkey={vote_pubkey}) in epoch={epoch} failed"
        ));
        return Err(());
    };
    let Some(entry) = rank_map.get_pubkey_stake_entry(*rank as usize) else {
        record_error(format!(
            "get_pubkey_stake_entry(rank={rank}) for vote_pubkey={vote_pubkey} in epoch={epoch} \
             failed",
        ));
        return Err(());
    };
    Ok(entry.stake)
}

fn record_error(msg: String) {
    error!("{msg}");
    datapoint_error!(
        "PER-total-stake-calculation-failure",
        ("error", msg, String)
    );
}

fn record_trace(msg: String) {
    trace!("{msg}");
    datapoint_trace!(
        "PER-total-stake-calculation-failure",
        ("trace", msg, String)
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

/// Calculates tower points.
///
/// Returns (points calculated, new_credits_observed,
/// true if AG_MIGRATION_EPOCH_CREDIT was seen else false)
fn tower_epoch_credits_iter(
    stake: &Stake,
    epoch_credits_iter: impl Iterator<Item = (Epoch, u64, u64)>,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
) -> (u128, u64, bool) {
    let mut points = 0;
    let credits_in_stake = stake.credits_observed;
    let mut new_credits_observed = credits_in_stake;
    let mut saw_marker = false;

    for entry in epoch_credits_iter {
        if entry == AG_MIGRATION_EPOCH_CREDIT {
            saw_marker = true;
            break;
        }
        let (epoch, final_epoch_credits, initial_epoch_credits) = entry;
        let earned_credits = calc_earned_credits(
            stake,
            final_epoch_credits,
            initial_epoch_credits,
            &mut new_credits_observed,
        );
        #[allow(deprecated)]
        let stake_amount = u128::from(stake.delegation.stake(
            epoch,
            stake_history,
            new_rate_activation_epoch,
        ));

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
    (points, new_credits_observed, saw_marker)
}

fn ag_epoch_credits_iter(
    migration_epoch: Epoch,
    mut saw_marker: bool,
    stake: &Stake,
    epoch_credits_iter: impl Iterator<Item = (Epoch, u64, u64)>,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
    epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
) -> Result<(u128, u64), CalculatedStakePoints> {
    let mut points = 0;
    let credits_in_stake = stake.credits_observed;
    let mut new_credits_observed = credits_in_stake;

    for entry in epoch_credits_iter {
        let (epoch, final_epoch_credits, initial_epoch_credits) = entry;
        if epoch < migration_epoch {
            continue;
        }
        if entry == AG_MIGRATION_EPOCH_CREDIT {
            debug_assert!(!saw_marker);
            saw_marker = true;
            continue;
        }
        if epoch == migration_epoch && !saw_marker {
            continue;
        }

        let earned_credits = calc_earned_credits(
            stake,
            final_epoch_credits,
            initial_epoch_credits,
            &mut new_credits_observed,
        );
        #[allow(deprecated)]
        let stake_amount = u128::from(stake.delegation.stake(
            epoch,
            stake_history,
            new_rate_activation_epoch,
        ));

        let earned_points = {
            if earned_credits == 0 {
                earned_credits
            } else {
                let Ok(total_stake) =
                    get_total_stake(stake.delegation.voter_pubkey, epoch_stakes, epoch)
                else {
                    return Err(CalculatedStakePoints {
                        tower_points: 0,
                        ag_points: 0,
                        new_credits_observed,
                        force_credits_update_with_skipped_reward: true,
                    });
                };
                earned_credits * stake_amount / total_stake as u128
            }
        };
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
    Ok((points, new_credits_observed))
}

fn migrating_epoch_credits_iter(
    migration_epoch: Epoch,
    stake: &Stake,
    mut epoch_credits_iter: impl Iterator<Item = (Epoch, u64, u64)>,
    stake_history: &StakeHistory,
    inflation_point_calc_tracer: Option<impl Fn(&InflationPointCalculationEvent)>,
    new_rate_activation_epoch: Option<Epoch>,
    epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
) -> Result<(u128, u128, u64), CalculatedStakePoints> {
    let (tower_points, tower_new_credits_observed, saw_marker) = tower_epoch_credits_iter(
        stake,
        epoch_credits_iter.by_ref(),
        stake_history,
        inflation_point_calc_tracer.as_ref(),
        new_rate_activation_epoch,
    );
    let (ag_points, ag_new_credits_observed) = ag_epoch_credits_iter(
        migration_epoch,
        saw_marker,
        stake,
        epoch_credits_iter,
        stake_history,
        inflation_point_calc_tracer,
        new_rate_activation_epoch,
        epoch_stakes,
    )?;

    let new_credits_observed = tower_new_credits_observed.max(ag_new_credits_observed);
    Ok((tower_points, ag_points, new_credits_observed))
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
    ag_epoch_type: &AlpenglowEpochType,
    epoch_stakes: &HashMap<Epoch, VersionedEpochStakes>,
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
                tower_points: 0,
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
                tower_points: 0,
                ag_points: 0,
                new_credits_observed: credits_in_stake,
                force_credits_update_with_skipped_reward: false,
            };
        }
        Ordering::Greater => {}
    }

    let (tower_points, ag_points, new_credits_observed) = match ag_epoch_type {
        AlpenglowEpochType::Tower => {
            let (points, credits, _) = tower_epoch_credits_iter(
                stake,
                vote_state.epoch_credits_iter,
                stake_history,
                inflation_point_calc_tracer,
                new_rate_activation_epoch,
            );
            (points, 0, credits)
        }
        AlpenglowEpochType::MigrationEpoch {
            migration_epoch, ..
        } => {
            match migrating_epoch_credits_iter(
                *migration_epoch,
                stake,
                vote_state.epoch_credits_iter,
                stake_history,
                inflation_point_calc_tracer,
                new_rate_activation_epoch,
                epoch_stakes,
            ) {
                Ok(r) => r,
                Err(e) => return e,
            }
        }
        AlpenglowEpochType::Alpenglow { migration_epoch } => {
            let (ag_points, credits) = match ag_epoch_credits_iter(
                *migration_epoch,
                false,
                stake,
                vote_state.epoch_credits_iter,
                stake_history,
                inflation_point_calc_tracer,
                new_rate_activation_epoch,
                epoch_stakes,
            ) {
                Ok(r) => r,
                Err(e) => return e,
            };
            (0, ag_points, credits)
        }
    };
    CalculatedStakePoints {
        tower_points,
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
        solana_vote::vote_account::VoteAccount,
        solana_vote_program::vote_state::{VoteStateV4, handler::VoteStateHandler},
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
    fn test_stake_state_calculate_points_with_typical_values() {
        let mut vote_state = VoteStateHandler::new_v4(VoteStateV4::default());

        // bootstrap means fully-vested stake at epoch 0 with
        //  10_000_000 SOL is a big but not unreasonable stake
        let stake = new_stake(
            10_000_000 * LAMPORTS_PER_SOL,
            &Pubkey::default(),
            vote_state.as_ref_v4(),
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
            calculate_stake_points_for_tower(
                &stake,
                DelegatedVoteState::from(vote_state.as_ref_v4()),
                &StakeHistory::default(),
                null_tracer(),
                None,
                &HashMap::new()
            )
        );
    }

    #[test]
    fn test_tower_epoch_credits_iter() {
        let stake_lamports = 10_000_000 * LAMPORTS_PER_SOL;
        let credits = 1235;

        let stake = new_stake(
            stake_lamports,
            &Pubkey::default(),
            VoteStateHandler::new_v4(VoteStateV4::default()).as_ref_v4(),
            u64::MAX,
        );

        let epoch_credits = vec![(0, credits, 0), (1, credits * 2, credits)];
        let mut epoch_credits_iter = epoch_credits.into_iter();
        let (points, new_credits, saw_marker) = tower_epoch_credits_iter(
            &stake,
            epoch_credits_iter.by_ref(),
            &StakeHistory::default(),
            null_tracer(),
            None,
        );
        assert_eq!(points, credits as u128 * stake_lamports as u128 * 2);
        assert_eq!(new_credits, credits * 2);
        assert_eq!(epoch_credits_iter.next(), None);
        assert!(!saw_marker);

        let epoch_credits = vec![
            (0, credits, 0),
            (1, credits * 2, credits),
            AG_MIGRATION_EPOCH_CREDIT,
        ];
        let mut epoch_credits_iter = epoch_credits.into_iter();
        let (points, new_credits, saw_marker) = tower_epoch_credits_iter(
            &stake,
            epoch_credits_iter.by_ref(),
            &StakeHistory::default(),
            null_tracer(),
            None,
        );
        assert_eq!(points, credits as u128 * stake_lamports as u128 * 2);
        assert_eq!(new_credits, credits * 2);
        assert_eq!(epoch_credits_iter.next(), None);
        assert!(saw_marker);

        let epoch_credits = vec![
            (0, credits, 0),
            (1, credits * 2, credits),
            AG_MIGRATION_EPOCH_CREDIT,
            (1, credits * 3, credits * 2),
        ];
        let mut epoch_credits_iter = epoch_credits.into_iter();
        let (points, new_credits, saw_marker) = tower_epoch_credits_iter(
            &stake,
            epoch_credits_iter.by_ref(),
            &StakeHistory::default(),
            null_tracer(),
            None,
        );
        assert_eq!(points, credits as u128 * stake_lamports as u128 * 2);
        assert_eq!(new_credits, credits * 2);
        assert_eq!(
            epoch_credits_iter.next().unwrap(),
            (1, credits * 3, credits * 2)
        );
        assert!(saw_marker);
    }

    #[test]
    fn test_ag_epoch_credits_iter() {
        let stake_lamports = 10_000_000 * LAMPORTS_PER_SOL;
        let total_stake = stake_lamports * 2;
        let credits = 1235;

        let stake = new_stake(
            stake_lamports,
            &Pubkey::default(),
            VoteStateHandler::new_v4(VoteStateV4::default()).as_ref_v4(),
            u64::MAX,
        );

        let vote_account = VoteAccount::new_random();
        let vote_account_hash_map = [(Pubkey::default(), (total_stake, vote_account))]
            .into_iter()
            .collect();
        let versioned_epoch_stakes = VersionedEpochStakes::new_for_tests(vote_account_hash_map, 0);
        let epoch_stakes = (0..10)
            .map(|epoch| (epoch, versioned_epoch_stakes.clone()))
            .collect();

        for saw_marker in [true, false] {
            let epoch_credits = vec![(0, credits, 0), (1, credits * 2, credits)];
            let (points, new_credits) = ag_epoch_credits_iter(
                2,
                saw_marker,
                &stake,
                epoch_credits.into_iter(),
                &StakeHistory::default(),
                null_tracer(),
                None,
                &epoch_stakes,
            )
            .unwrap();
            assert_eq!(points, 0);
            assert_eq!(new_credits, 0);
        }

        for saw_marker in [true, false] {
            let epoch_credits = vec![(0, credits, 0), (1, credits * 2, credits)];
            let (points, new_credits) = ag_epoch_credits_iter(
                0,
                saw_marker,
                &stake,
                epoch_credits.into_iter(),
                &StakeHistory::default(),
                null_tracer(),
                None,
                &epoch_stakes,
            )
            .unwrap();
            if saw_marker {
                assert_eq!(
                    points,
                    (credits as u128 * stake_lamports as u128 / total_stake as u128) * 2
                );
            } else {
                assert_eq!(
                    points,
                    credits as u128 * stake_lamports as u128 / total_stake as u128
                );
            }
            assert_eq!(new_credits, credits * 2);
        }

        let epoch_credits = vec![
            (0, credits, 0),
            AG_MIGRATION_EPOCH_CREDIT,
            (0, credits * 2, credits),
            (1, credits * 3, credits * 2),
        ];
        let (points, new_credits) = ag_epoch_credits_iter(
            0,
            false,
            &stake,
            epoch_credits.into_iter(),
            &StakeHistory::default(),
            null_tracer(),
            None,
            &epoch_stakes,
        )
        .unwrap();
        assert_eq!(
            points,
            (credits as u128 * stake_lamports as u128 / total_stake as u128) * 2
        );
        assert_eq!(new_credits, credits * 3);

        for saw_marker in [true, false] {
            let epoch_credits = vec![(1, credits, 0), (2, credits * 2, credits)];
            let (points, new_credits) = ag_epoch_credits_iter(
                0,
                saw_marker,
                &stake,
                epoch_credits.into_iter(),
                &StakeHistory::default(),
                null_tracer(),
                None,
                &epoch_stakes,
            )
            .unwrap();
            assert_eq!(
                points,
                (credits as u128 * stake_lamports as u128 / total_stake as u128) * 2
            );
            assert_eq!(new_credits, credits * 2);
        }
    }

    #[test]
    fn test_migrating_epoch_credits_iter() {
        let stake_lamports = 10_000_000 * LAMPORTS_PER_SOL;
        let total_stake = stake_lamports * 2;
        let credits = 1235;

        let stake = new_stake(
            stake_lamports,
            &Pubkey::default(),
            VoteStateHandler::new_v4(VoteStateV4::default()).as_ref_v4(),
            u64::MAX,
        );

        let vote_account = VoteAccount::new_random();
        let vote_account_hash_map = [(Pubkey::default(), (total_stake, vote_account))]
            .into_iter()
            .collect();
        let versioned_epoch_stakes = VersionedEpochStakes::new_for_tests(vote_account_hash_map, 0);
        let epoch_stakes = (0..10)
            .map(|epoch| (epoch, versioned_epoch_stakes.clone()))
            .collect();

        let epoch_credits = vec![(0, credits, 0), (1, credits * 2, credits)];
        let (tower_points, ag_points, new_credits) = migrating_epoch_credits_iter(
            2,
            &stake,
            epoch_credits.into_iter(),
            &StakeHistory::default(),
            null_tracer(),
            None,
            &epoch_stakes,
        )
        .unwrap();
        assert_eq!(tower_points, credits as u128 * stake_lamports as u128 * 2);
        assert_eq!(ag_points, 0);
        assert_eq!(new_credits, credits * 2);

        let epoch_credits = vec![
            (0, credits, 0),
            AG_MIGRATION_EPOCH_CREDIT,
            (0, credits * 2, credits),
        ];
        let (tower_points, ag_points, new_credits) = migrating_epoch_credits_iter(
            0,
            &stake,
            epoch_credits.into_iter(),
            &StakeHistory::default(),
            null_tracer(),
            None,
            &epoch_stakes,
        )
        .unwrap();
        assert_eq!(tower_points, credits as u128 * stake_lamports as u128);
        assert_eq!(
            ag_points,
            credits as u128 * stake_lamports as u128 / total_stake as u128
        );
        assert_eq!(new_credits, credits * 2);

        let epoch_credits = vec![AG_MIGRATION_EPOCH_CREDIT, (0, credits * 2, credits)];
        let (tower_points, ag_points, new_credits) = migrating_epoch_credits_iter(
            0,
            &stake,
            epoch_credits.into_iter(),
            &StakeHistory::default(),
            null_tracer(),
            None,
            &epoch_stakes,
        )
        .unwrap();
        assert_eq!(tower_points, 0);
        assert_eq!(
            ag_points,
            credits as u128 * stake_lamports as u128 / total_stake as u128
        );
        assert_eq!(new_credits, credits * 2);
    }

    #[test]
    fn test_changing_total_stake() {
        let pubkey = Pubkey::new_unique();
        let vote_account = VoteAccount::new_random();
        let staker_delegation = LAMPORTS_PER_SOL;
        let epoch0_validator_stake = staker_delegation * 5;
        let epoch1_validator_stake = epoch0_validator_stake * 3;
        let epoch2_validator_stake = epoch0_validator_stake / 3;

        let epoch0_vote_accounts = [(pubkey, (epoch0_validator_stake, vote_account.clone()))]
            .into_iter()
            .collect();
        let versioned_epoch_stakes0 = VersionedEpochStakes::new_for_tests(epoch0_vote_accounts, 0);

        let epoch1_vote_accounts = [(pubkey, (epoch1_validator_stake, vote_account.clone()))]
            .into_iter()
            .collect();
        let versioned_epoch_stakes1 = VersionedEpochStakes::new_for_tests(epoch1_vote_accounts, 0);

        let epoch2_vote_accounts = [(pubkey, (epoch2_validator_stake, vote_account.clone()))]
            .into_iter()
            .collect();
        let versioned_epoch_stakes2 = VersionedEpochStakes::new_for_tests(epoch2_vote_accounts, 0);

        let epoch_stakes = [
            (0, versioned_epoch_stakes0),
            (1, versioned_epoch_stakes1),
            (2, versioned_epoch_stakes2),
        ]
        .into_iter()
        .collect();
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
            AG_MIGRATION_EPOCH_CREDIT,
            (0, credits * 2, credits),
            (1, credits * 3, credits * 2),
            (2, credits * 4, credits * 3),
        ];
        let (points, new_credits) = ag_epoch_credits_iter(
            0,
            false,
            &stake,
            epoch_credits.into_iter(),
            &StakeHistory::default(),
            null_tracer(),
            None,
            &epoch_stakes,
        )
        .unwrap();
        assert_eq!(new_credits, credits * 4);
        let epoch0_expected = credits * staker_delegation / epoch0_validator_stake;
        let epoch1_expected = credits * staker_delegation / epoch1_validator_stake;
        let epoch2_expected = credits * staker_delegation / epoch2_validator_stake;
        let expected_points = epoch0_expected + epoch1_expected + epoch2_expected;
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
            let epoch_credits = vec![(0, credits, 0), (1, credits * 2, credits)];
            let mut epoch_credits_iter = epoch_credits.into_iter();
            let (points, new_credits, saw_marker) = tower_epoch_credits_iter(
                &stake,
                epoch_credits_iter.by_ref(),
                &StakeHistory::default(),
                null_tracer(),
                None,
            );
            assert_eq!(points, credits as u128 * stake_lamports as u128);
            assert_eq!(new_credits, credits * 2);
            assert_eq!(epoch_credits_iter.next(), None);
            assert!(!saw_marker);
        }

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
            let vote_account = VoteAccount::new_random();
            let vote_account_hash_map = [(Pubkey::default(), (total_stake, vote_account))]
                .into_iter()
                .collect();
            let versioned_epoch_stakes =
                VersionedEpochStakes::new_for_tests(vote_account_hash_map, 0);
            let epoch_stakes = (0..10)
                .map(|epoch| (epoch, versioned_epoch_stakes.clone()))
                .collect();

            let epoch_credits = vec![
                (0, credits, 0),
                AG_MIGRATION_EPOCH_CREDIT,
                (0, credits * 2, credits),
                (1, credits * 3, credits * 2),
            ];
            let (points, new_credits) = ag_epoch_credits_iter(
                0,
                false,
                &stake,
                epoch_credits.into_iter(),
                &StakeHistory::default(),
                null_tracer(),
                None,
                &epoch_stakes,
            )
            .unwrap();
            assert_eq!(
                points,
                credits as u128 * stake_lamports as u128 / total_stake as u128
            );
            assert_eq!(new_credits, credits * 3);
        }

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
            let vote_account = VoteAccount::new_random();
            let vote_account_hash_map = [(Pubkey::default(), (total_stake, vote_account))]
                .into_iter()
                .collect();
            let versioned_epoch_stakes =
                VersionedEpochStakes::new_for_tests(vote_account_hash_map, 0);
            let epoch_stakes = (0..10)
                .map(|epoch| (epoch, versioned_epoch_stakes.clone()))
                .collect();

            let epoch_credits = vec![
                (0, credits, 0),
                AG_MIGRATION_EPOCH_CREDIT,
                (0, credits * 2, credits),
                (1, credits * 3, credits * 2),
            ];
            let (tower_points, ag_points, new_credits) = migrating_epoch_credits_iter(
                0,
                &stake,
                epoch_credits.into_iter(),
                &StakeHistory::default(),
                null_tracer(),
                None,
                &epoch_stakes,
            )
            .unwrap();
            if activation_epoch == 0 {
                assert_eq!(tower_points, 0);
            } else {
                assert_eq!(tower_points, credits as u128 * stake_lamports as u128);
            }
            assert_eq!(
                ag_points,
                credits as u128 * stake_lamports as u128 / total_stake as u128
            );
            assert_eq!(new_credits, credits * 3);
        }
    }
}
