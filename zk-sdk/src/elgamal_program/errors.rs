#[cfg(not(target_os = "solana"))]
use crate::range_proof::errors::RangeProofGenerationError;
use {
    crate::{
        errors::ElGamalError, range_proof::errors::RangeProofVerificationError,
        sigma_proofs::errors::*,
    },
    thiserror::Error,
};

#[cfg(not(target_os = "solana"))]
#[derive(Error, Clone, Debug, Eq, PartialEq)]
pub enum ProofGenerationError {
    #[error("illegal number of commitments")]
    IllegalCommitmentLength,
    #[error("illegal amount bit length")]
    IllegalAmountBitLength,
    #[error("invalid commitment")]
    InvalidCommitment,
    #[error("range proof generation failed")]
    RangeProof(#[from] RangeProofGenerationError),
    #[error("unexpected proof length")]
    ProofLength,
}

#[derive(Error, Clone, Debug, Eq, PartialEq)]
pub enum ProofVerificationError {
    #[error("range proof verification failed")]
    RangeProof(#[from] RangeProofVerificationError),
    #[error("sigma proof verification failed")]
    SigmaProof(SigmaProofType, SigmaProofVerificationError),
    #[error("ElGamal ciphertext or public key error")]
    ElGamal(#[from] ElGamalError),
    #[error("Invalid proof context")]
    ProofContext,
    #[error("illegal commitment length")]
    IllegalCommitmentLength,
    #[error("illegal amount bit length")]
    IllegalAmountBitLength,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SigmaProofType {
    ZeroCiphertext,
    CiphertextCiphertextEquality,
}

impl From<ZeroCiphertextProofVerificationError> for ProofVerificationError {
    fn from(err: ZeroCiphertextProofVerificationError) -> Self {
        Self::SigmaProof(SigmaProofType::ZeroCiphertext, err.0)
    }
}

impl From<EqualityProofVerificationError> for ProofVerificationError {
    fn from(err: EqualityProofVerificationError) -> Self {
        Self::SigmaProof(SigmaProofType::CiphertextCiphertextEquality, err.0)
    }
}
