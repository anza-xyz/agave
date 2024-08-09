use {core::fmt, solana_decode_error::DecodeError};

/// Precompile errors
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrecompileError {
    InvalidPublicKey,
    InvalidRecoveryId,
    InvalidSignature,
    InvalidDataOffsets,
    InvalidInstructionDataSize,
}

impl std::error::Error for PrecompileError {}

impl fmt::Display for PrecompileError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PrecompileError::InvalidPublicKey => f.write_str("public key is not valid"),
            PrecompileError::InvalidRecoveryId => f.write_str("id is not valid"),
            PrecompileError::InvalidSignature => f.write_str("signature is not valid"),
            PrecompileError::InvalidDataOffsets => f.write_str("offset not valid"),
            PrecompileError::InvalidInstructionDataSize => {
                f.write_str("instruction is incorrect size")
            }
        }
    }
}

impl<T> DecodeError<T> for PrecompileError {
    fn type_of() -> &'static str {
        "PrecompileError"
    }
}
