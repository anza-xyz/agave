//! Memory translation utilities.

/// Error types for memory translation operations.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MemoryTranslationError {
    #[error("Unaligned pointer")]
    UnalignedPointer,
    #[error("Invalid length")]
    InvalidLength,
}
