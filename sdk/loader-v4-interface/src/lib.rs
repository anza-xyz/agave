//! The v4 built-in loader program.
//!
//! This is the loader of the program runtime v2.
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

pub mod instruction;
pub mod state;

/// Cooldown before a program can be un-/redeployed again
pub const DEPLOYMENT_COOLDOWN_IN_SLOTS: u64 = 1;
