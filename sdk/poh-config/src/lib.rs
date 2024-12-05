//! Definitions of Solana's proof of history.
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]

use std::time::Duration;

// inlined to avoid solana-clock dep
const DEFAULT_TICKS_PER_SECOND: u64 = 160;
#[cfg(test)]
static_assertions::const_assert_eq!(
    DEFAULT_TICKS_PER_SECOND,
    solana_clock::DEFAULT_TICKS_PER_SECOND
);

#[cfg_attr(feature = "frozen-abi", derive(solana_frozen_abi_macro::AbiExample))]
#[cfg_attr(
    feature = "serde",
    derive(serde_derive::Deserialize, serde_derive::Serialize)
)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PohConfig {
    /// The target tick rate of the cluster.
    pub target_tick_duration: Duration,

    /// The target total tick count to be produced; used for testing only
    pub target_tick_count: Option<u64>,

    /// How many hashes to roll before emitting the next tick entry.
    /// None enables "Low power mode", which makes the validator sleep
    /// for `target_tick_duration` instead of hashing
    pub hashes_per_tick: Option<u64>,
}

impl PohConfig {
    pub fn new_sleep(target_tick_duration: Duration) -> Self {
        Self {
            target_tick_duration,
            hashes_per_tick: None,
            target_tick_count: None,
        }
    }
}

// the !=0 check was previously done by the unchecked_div_by_const macro
#[cfg(test)]
static_assertions::const_assert!(DEFAULT_TICKS_PER_SECOND != 0);
const DEFAULT_SLEEP_MICROS: u64 = (1000 * 1000) / DEFAULT_TICKS_PER_SECOND;

impl Default for PohConfig {
    fn default() -> Self {
        Self::new_sleep(Duration::from_micros(DEFAULT_SLEEP_MICROS))
    }
}
