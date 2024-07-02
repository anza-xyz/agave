// Copyright 2016 6WIND S.A. <quentin.monnet@6wind.com>
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! This module contains error and result types

use {
    crate::{elf::ElfError, memory_region::AccessType, verifier::VerifierError},
    std::error::Error,
};

/// Error definitions
#[derive(Debug, thiserror::Error)]
#[repr(u64)] // discriminant size, used in emit_exception_kind in JIT
pub enum EbpfError {
    /// ELF error
    #[error("ELF error: {0}")]
    ElfError(#[from] ElfError),
    /// Function was already registered
    #[error("function #{0} was already registered")]
    FunctionAlreadyRegistered(usize),
    /// Exceeded max BPF to BPF call depth
    #[error("exceeded max BPF to BPF call depth")]
    CallDepthExceeded,
    /// Attempt to exit from root call frame
    #[error("attempted to exit root call frame")]
    ExitRootCallFrame,
    /// Divide by zero"
    #[error("divide by zero at BPF instruction")]
    DivideByZero,
    /// Divide overflow
    #[error("division overflow at BPF instruction")]
    DivideOverflow,
    /// Exceeded max instructions allowed
    #[error("attempted to execute past the end of the text segment at BPF instruction")]
    ExecutionOverrun,
    /// Attempt to call to an address outside the text segment
    #[error("callx attempted to call outside of the text segment")]
    CallOutsideTextSegment,
    /// Exceeded max instructions allowed
    #[error("exceeded CUs meter at BPF instruction")]
    ExceededMaxInstructions,
    /// Program has not been JIT-compiled
    #[error("program has not been JIT-compiled")]
    JitNotCompiled,
    /// Invalid virtual address
    #[error("invalid virtual address {0:x?}")]
    InvalidVirtualAddress(u64),
    /// Memory region index or virtual address space is invalid
    #[error("Invalid memory region at index {0}")]
    InvalidMemoryRegion(usize),
    /// Access violation (general)
    #[error("Access violation in {3} section at address {1:#x} of size {2:?}")]
    AccessViolation(AccessType, u64, u64, &'static str),
    /// Access violation (stack specific)
    #[error("Access violation in stack frame {3} at address {1:#x} of size {2:?}")]
    StackAccessViolation(AccessType, u64, u64, i64),
    /// Invalid instruction
    #[error("invalid BPF instruction")]
    InvalidInstruction,
    /// Unsupported instruction
    #[error("unsupported BPF instruction")]
    UnsupportedInstruction,
    /// Compilation is too big to fit
    #[error("Compilation exhausted text segment at BPF instruction {0}")]
    ExhaustedTextSegment(usize),
    /// Libc function call returned an error
    #[error("Libc calling {0} {1:?} returned error code {2}")]
    LibcInvocationFailed(&'static str, Vec<String>, i32),
    /// Verifier error
    #[error("Verifier error: {0}")]
    VerifierError(#[from] VerifierError),
    /// Syscall error
    #[error("Syscall error: {0}")]
    SyscallError(Box<dyn Error>),
}

/// Same as `Result` but provides a stable memory layout
#[derive(Debug)]
#[repr(C, u64)]
pub enum StableResult<T, E> {
    /// Success
    Ok(T),
    /// Failure
    Err(E),
}

impl<T: std::fmt::Debug, E: std::fmt::Debug> StableResult<T, E> {
    /// `true` if `Ok`
    pub fn is_ok(&self) -> bool {
        match self {
            Self::Ok(_) => true,
            Self::Err(_) => false,
        }
    }

    /// `true` if `Err`
    pub fn is_err(&self) -> bool {
        match self {
            Self::Ok(_) => false,
            Self::Err(_) => true,
        }
    }

    /// Returns the inner value if `Ok`, panics otherwise
    pub fn unwrap(self) -> T {
        match self {
            Self::Ok(value) => value,
            Self::Err(error) => panic!("unwrap {:?}", error),
        }
    }

    /// Returns the inner error if `Err`, panics otherwise
    pub fn unwrap_err(self) -> E {
        match self {
            Self::Ok(value) => panic!("unwrap_err {:?}", value),
            Self::Err(error) => error,
        }
    }

    /// Maps ok values, leaving error values untouched
    pub fn map<U, O: FnOnce(T) -> U>(self, op: O) -> StableResult<U, E> {
        match self {
            Self::Ok(value) => StableResult::<U, E>::Ok(op(value)),
            Self::Err(error) => StableResult::<U, E>::Err(error),
        }
    }

    /// Maps error values, leaving ok values untouched
    pub fn map_err<F, O: FnOnce(E) -> F>(self, op: O) -> StableResult<T, F> {
        match self {
            Self::Ok(value) => StableResult::<T, F>::Ok(value),
            Self::Err(error) => StableResult::<T, F>::Err(op(error)),
        }
    }

    #[cfg_attr(
        any(
            not(feature = "jit"),
            target_os = "windows",
            not(target_arch = "x86_64")
        ),
        allow(dead_code)
    )]
    pub(crate) fn discriminant(&self) -> u64 {
        unsafe { *std::ptr::addr_of!(*self).cast::<u64>() }
    }
}

impl<T, E> From<StableResult<T, E>> for Result<T, E> {
    fn from(result: StableResult<T, E>) -> Self {
        match result {
            StableResult::Ok(value) => Ok(value),
            StableResult::Err(value) => Err(value),
        }
    }
}

impl<T, E> From<Result<T, E>> for StableResult<T, E> {
    fn from(result: Result<T, E>) -> Self {
        match result {
            Ok(value) => Self::Ok(value),
            Err(value) => Self::Err(value),
        }
    }
}

/// Return value of programs and syscalls
pub type ProgramResult = StableResult<u64, EbpfError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_program_result_is_stable() {
        let ok = ProgramResult::Ok(42);
        assert_eq!(ok.discriminant(), 0);
        let err = ProgramResult::Err(EbpfError::JitNotCompiled);
        assert_eq!(err.discriminant(), 1);
    }
}
