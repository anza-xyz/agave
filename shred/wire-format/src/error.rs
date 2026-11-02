use thiserror::Error;

/// What can go wrong while parsing raw bytes as a shred.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("shred is {len} bytes, expected at least {expected}")]
    TooShort { len: usize, expected: usize },
    #[error(transparent)]
    Read(#[from] wincode::ReadError),
}
