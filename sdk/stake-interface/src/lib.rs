//! The Stake program interface.

#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

#[allow(deprecated)]
pub mod config;
pub mod error;
pub mod instruction;
pub mod stake_flags;
pub mod stake_history;
pub mod state;
pub mod tools;

pub mod program {
    solana_pubkey::declare_id!("Stake11111111111111111111111111111111111111");
}

/// The minimum number of epochs before stake account that is delegated to a delinquent vote
/// account may be unstaked with `StakeInstruction::DeactivateDelinquent`
pub const MINIMUM_DELINQUENT_EPOCHS_FOR_DEACTIVATION: usize = 5;
