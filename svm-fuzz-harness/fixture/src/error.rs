//! Proto <--> Fixture errors.

use {prost::DecodeError, thiserror::Error};

#[derive(Debug, Error, PartialEq)]
pub enum FixtureError {
    #[error("Decode error")]
    DecodeError(#[from] DecodeError),
    #[error("Invalid public key bytes")]
    InvalidPubkeyBytes(Vec<u8>),
    #[error("An account is missing for instruction account index {0}")]
    AccountMissingForInstrAccount(usize),
}
