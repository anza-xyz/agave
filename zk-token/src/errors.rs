use thiserror::Error;

#[cfg(not(target_os = "solana"))]
#[derive(Error, Clone, Debug, Eq, PartialEq)]
pub enum ProofDataError {
    #[error("not enough funds in account")]
    NotEnoughFunds,
    #[error("transfer fee calculation error")]
    FeeCalculation,
    #[error("illegal number of commitments")]
    IllegalCommitmentLength,
    #[error("illegal amount bit length")]
    IllegalAmountBitLength,
    #[error("invalid commitment")]
    InvalidCommitment,
    #[error("unexpected proof length")]
    ProofLength,
    #[error("decryption failed")]
    Decryption,
    #[error("proof generation")]
    ProofGeneration,
    #[error("proof verification")]
    ProofVerification,
}
