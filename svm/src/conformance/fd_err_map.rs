//! Error-code mapping for VM execution results.
//!
//! Kept aligned with Firedancer so cross-implementation conformance fixtures
//! compare equal. The error number is generally the Agave enum variant index
//! `+ 1` (Agave uses the positive value; Firedancer uses its negation). Error
//! strings in Agave may carry parameters that Firedancer truncates; where they
//! diverge it is made explicit, otherwise `error.to_string()` is the expected
//! value.

use {
    solana_instruction::error::InstructionError,
    solana_poseidon::PoseidonSyscallError,
    solana_program_runtime::{
        cpi::CpiError,
        memory::MemoryTranslationError,
        solana_sbpf::{
            elf::ElfError,
            error::{EbpfError, StableResult},
        },
    },
    solana_syscalls::SyscallError,
};

pub(crate) trait FiredancerErrorCode {
    fn error_code(&self) -> i32;
}

impl FiredancerErrorCode for EbpfError {
    fn error_code(&self) -> i32 {
        let index: i32 = match self {
            EbpfError::ElfError(_) => 0,
            EbpfError::FunctionAlreadyRegistered(_) => 1,
            EbpfError::CallDepthExceeded => 2,
            EbpfError::ExitRootCallFrame => 3,
            EbpfError::DivideByZero => 4,
            EbpfError::DivideOverflow => 5,
            EbpfError::ExecutionOverrun => 6,
            EbpfError::CallOutsideTextSegment => 7,
            EbpfError::ExceededMaxInstructions => 8,
            EbpfError::JitNotCompiled => 9,
            EbpfError::InvalidMemoryRegion(_) => 11,
            EbpfError::AccessViolation(_, _, _, _) => 12,
            EbpfError::StackAccessViolation(_, _, _, _) => 13,
            EbpfError::InvalidInstruction => 14,
            EbpfError::UnsupportedInstruction => 15,
            EbpfError::ExhaustedTextSegment(_) => 16,
            EbpfError::LibcInvocationFailed(_, _, _) => 17,
            EbpfError::VerifierError(_) => 18,
            // `SyscallError` is unpacked separately via downcasting.
            EbpfError::SyscallError(_) => -10,
        };
        index.saturating_add(1)
    }
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

impl FiredancerErrorCode for SyscallError {
    fn error_code(&self) -> i32 {
        let index: i32 = match self {
            SyscallError::InvalidString(_, _) => 0,
            SyscallError::Abort => 1,
            SyscallError::Panic(_, _, _) => 2,
            SyscallError::InvokeContextBorrowFailed => 3,
            SyscallError::MalformedSignerSeed(_, _) => 4,
            SyscallError::BadSeeds(_) => 5,
            SyscallError::ProgramNotSupported(_) => 6,
            SyscallError::UnalignedPointer => 7,
            SyscallError::TooManySigners => 8,
            SyscallError::InstructionTooLarge(_, _) => 9,
            SyscallError::TooManyAccounts => 10,
            SyscallError::CopyOverlapping => 11,
            SyscallError::ReturnDataTooLarge(_, _) => 12,
            SyscallError::TooManySlices => 13,
            SyscallError::InvalidLength => 14,
            SyscallError::MaxInstructionDataLenExceeded { .. } => 15,
            SyscallError::MaxInstructionAccountsExceeded { .. } => 16,
            SyscallError::MaxInstructionAccountInfosExceeded { .. } => 17,
            SyscallError::InvalidAttribute => 18,
            SyscallError::InvalidPointer => 19,
            SyscallError::ArithmeticOverflow => 20,
        };
        index.saturating_add(1)
    }
}

impl FiredancerErrorCode for i32 {
    fn error_code(&self) -> i32 {
        *self
    }
}

/// A VM `program_result` mapped into the fields a conformance fixture compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnpackedResult {
    /// Firedancer error number (negated Agave enum index), or `0` on success.
    pub error: i64,
    /// Which error taxonomy `error` belongs to (`ERR_KIND_*`).
    pub error_kind: i32,
    /// The program return value (`r0`), only meaningful on success.
    pub r0: u64,
}

impl UnpackedResult {
    const ERR_KIND_UNSPECIFIED: i32 = 0;
    const ERR_KIND_EBPF: i32 = 1;
    const ERR_KIND_SYSCALL: i32 = 2;
    const ERR_KIND_INSTRUCTION: i32 = 3;

    fn ok(r0: u64) -> Self {
        Self {
            error: 0,
            error_kind: Self::ERR_KIND_UNSPECIFIED,
            r0,
        }
    }

    fn err<E: FiredancerErrorCode>(error: &E, error_kind: i32) -> Self {
        Self {
            error: error.error_code() as i64,
            error_kind,
            r0: 0,
        }
    }

    fn from_ebpf_err(ebpf_err: EbpfError) -> Self {
        // Agave wraps syscall-side failures in `EbpfError::SyscallError`; recover
        // the concrete error by downcasting so we report the matching code and
        // kind. Anything else is a plain VM error.
        match &ebpf_err {
            EbpfError::SyscallError(boxed) => {
                if let Some(e) = boxed.downcast_ref::<InstructionError>() {
                    Self::err(e, Self::ERR_KIND_INSTRUCTION)
                } else if let Some(e) = boxed.downcast_ref::<SyscallError>() {
                    Self::err(e, Self::ERR_KIND_SYSCALL)
                } else if let Some(e) = boxed.downcast_ref::<MemoryTranslationError>() {
                    let e: SyscallError = e.clone().into();
                    Self::err(&e, Self::ERR_KIND_SYSCALL)
                } else if let Some(e) = boxed.downcast_ref::<CpiError>() {
                    let e: SyscallError = e.clone().into();
                    Self::err(&e, Self::ERR_KIND_SYSCALL)
                } else if let Some(e) = boxed.downcast_ref::<EbpfError>() {
                    Self::err(e, Self::ERR_KIND_EBPF)
                } else if boxed.downcast_ref::<PoseidonSyscallError>().is_some() {
                    Self::err(&(-1), Self::ERR_KIND_SYSCALL)
                } else {
                    Self::err(&(-1), Self::ERR_KIND_UNSPECIFIED)
                }
            }
            _ => Self::err(&ebpf_err, Self::ERR_KIND_EBPF),
        }
    }
}

/// Map a VM `program_result` to its [`UnpackedResult`].
pub(crate) fn unpack_stable_result(program_result: StableResult<u64, EbpfError>) -> UnpackedResult {
    match program_result {
        StableResult::Ok(r0) => UnpackedResult::ok(r0),
        StableResult::Err(ebpf_err) => UnpackedResult::from_ebpf_err(ebpf_err),
    }
}
