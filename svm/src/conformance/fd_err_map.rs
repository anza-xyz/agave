//! Error-code mapping for VM execution results.
//!
//! Kept aligned with Firedancer so cross-implementation conformance fixtures
//! compare equal. The error number is generally the Agave enum variant index
//! `+ 1` (Agave uses the positive value; Firedancer uses its negation). Error
//! strings in Agave may carry parameters that Firedancer truncates; where they
//! diverge it is made explicit, otherwise `error.to_string()` is the expected
//! value.

use {
    solana_instruction::error::InstructionError, solana_program_runtime::solana_sbpf::elf::ElfError,
};

pub(crate) trait FiredancerErrorCode {
    fn error_code(&self) -> i32;
}

impl FiredancerErrorCode for ElfError {
    fn error_code(&self) -> i32 {
        let index: i32 = match self {
            ElfError::FailedToParse(_) => 0,
            ElfError::EntrypointOutOfBounds => 1,
            ElfError::InvalidEntrypoint => 2,
            ElfError::FailedToGetSection(_) => 3,
            ElfError::UnresolvedSymbol(_, _, _) => 4,
            ElfError::SectionNotFound(_) => 5,
            ElfError::RelativeJumpOutOfBounds(_) => 6,
            ElfError::SymbolHashCollision(_) => 7,
            ElfError::WrongEndianess => 8,
            ElfError::WrongAbi => 9,
            ElfError::WrongMachine => 10,
            ElfError::WrongClass => 11,
            ElfError::NotOneTextSection => 12,
            ElfError::WritableSectionNotSupported(_) => 13,
            ElfError::AddressOutsideLoadableSection(_) => 14,
            ElfError::InvalidVirtualAddress(_) => 15,
            ElfError::UnknownRelocation(_) => 16,
            ElfError::FailedToReadRelocationInfo => 17,
            ElfError::WrongType => 18,
            ElfError::UnknownSymbol(_) => 19,
            ElfError::ValueOutOfBounds => 20,
            ElfError::UnsupportedSBPFVersion => 21,
            ElfError::InvalidProgramHeader => 22,
        };
        index.saturating_add(1)
    }
}

impl FiredancerErrorCode for InstructionError {
    fn error_code(&self) -> i32 {
        let serialized = bincode::serialize(self).unwrap();
        i32::from_le_bytes(serialized[0..4].try_into().unwrap()).saturating_add(1)
    }
}
