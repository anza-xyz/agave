//! Information about points calculation based on stake state.
//! Used by `solana-runtime`.

use {solana_pubkey::Pubkey, solana_stake_interface::state::Delegation};

/// captures a rewards round as lamports to be awarded
///  and the total points over which those lamports
///  are to be distributed
//  basically read as rewards/points, but in integers instead of as an f64
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PointValue {
    pub rewards: u64, // lamports to split
    pub points: u128, // over these points
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
    ZeroCreditsAndReturnRewinded,
}

impl From<SkippedReason> for InflationPointCalculationEvent {
    fn from(reason: SkippedReason) -> Self {
        InflationPointCalculationEvent::Skipped(reason)
    }
}
