//! Errors related to proving and verifying proofs.
use thiserror::Error;

#[derive(Error, Clone, Debug, Eq, PartialEq)]
pub enum AuthenticatedEncryptionError {
    #[error("key derivation method not supported")]
    DerivationMethodNotSupported,
    #[error("seed length too short for derivation")]
    SeedLengthTooShort,
    #[error("seed length too long for derivation")]
    SeedLengthTooLong,
    #[error("failed to deserialize")]
    Deserialization,
}

#[derive(Error, Clone, Debug, Eq, PartialEq)]
pub enum ElGamalError {
    #[error("key derivation method not supported")]
    DerivationMethodNotSupported,
    #[error("seed length too short for derivation")]
    SeedLengthTooShort,
    #[error("seed length too long for derivation")]
    SeedLengthTooLong,
    #[error("failed to deserialize ciphertext")]
    CiphertextDeserialization,
    #[error("failed to deserialize public key")]
    PubkeyDeserialization,
    #[error("failed to deserialize keypair")]
    KeypairDeserialization,
    #[error("failed to deserialize secret key")]
    SecretKeyDeserialization,
}

#[derive(Error, Clone, Debug, Eq, PartialEq)]
pub enum TranscriptError {
    #[error("Invalid point: point is the identity")]
    ValidationError,
}

#[derive(Error, Debug, Clone, Eq, PartialEq)]
pub enum ParseError {
    #[error("Input string has wrong size")]
    WrongSize,
    #[error("Invalid Base64 string")]
    Invalid,
}
