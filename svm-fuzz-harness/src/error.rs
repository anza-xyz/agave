//! Errors that can be thrown by the harness.

use thiserror::Error;

/// Errors that can be thrown by the harness.
#[derive(Debug, Error, PartialEq)]
pub enum HarnessError {
    /// Invalid protobuf.
    #[error("Invalid protobuf")]
    InvalidProtobuf(#[from] prost::DecodeError),

    /// Integer out of range.
    #[error("Integer out of range")]
    IntegerOutOfRange,

    /// Invalid hash bytes.
    #[error("Invalid hash bytes")]
    InvalidHashBytes,

    /// Invalid public key bytes.
    #[error("Invalid public key bytes")]
    InvalidPubkeyBytes,

    /// Account missing.
    #[error("Account missing")]
    AccountMissing,

    /// Invalid fixture input.
    #[error("Invalid fixture input")]
    InvalidFixtureInput,

    /// Invalid fixture output.
    #[error("Invalid fixture output")]
    InvalidFixtureOutput,
}
