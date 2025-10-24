//! Error types for fixture conversions.

#![cfg(feature = "fuzz")]

use thiserror::Error;

#[derive(Debug, Error, PartialEq)]
pub enum FixtureError {
    #[error("Invalid fixture input")]
    InvalidFixtureInput,

    #[error("Invalid public key bytes")]
    InvalidPubkeyBytes(Vec<u8>),

    #[error("An account is missing for instruction account index {0}")]
    AccountMissingForInstrAccount(usize),
}
