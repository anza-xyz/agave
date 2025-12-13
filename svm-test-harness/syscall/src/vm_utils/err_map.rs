//! Error mapping utilities for syscall harness.
//!
//! Important: The error mapping in this file should be kept aligned with Firedancer.

use {
    agave_syscalls::SyscallError,
    solana_instruction_error::InstructionError,
    solana_poseidon::PoseidonSyscallError,
    solana_program_runtime::{
        cpi::CpiError, invoke_context::InvokeContext, memory::MemoryTranslationError, stable_log,
    },
    solana_pubkey::Pubkey,
    solana_sbpf::error::{EbpfError, StableResult},
    solana_svm_test_harness_fixture::syscall_effects::ErrKind,
};

pub fn instr_err_to_num(error: &InstructionError) -> i32 {
    let serialized_err = bincode::serialize(error).unwrap();
    i32::from_le_bytes((&serialized_err[0..4]).try_into().unwrap()).saturating_add(1)
}

pub fn syscall_err_to_num(error: &SyscallError) -> i32 {
    let err: i32 = match error {
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
    err.saturating_add(1)
}

pub fn ebpf_err_to_num(error: &EbpfError) -> i32 {
    let err: i32 = match error {
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
        EbpfError::SyscallError(_) => -10,
    };
    err.saturating_add(1)
}

/// Unpack a StableResult from VM execution into (error_number, error_kind, r0).
pub fn unpack_stable_result(
    program_result: StableResult<u64, EbpfError>,
    invoke_context: &InvokeContext,
    program_id: &Pubkey,
) -> (i64, ErrKind, u64) {
    match program_result {
        StableResult::Ok(n) => (0, ErrKind::Unspecified, n),
        StableResult::Err(ref err) => {
            let logger = invoke_context.get_log_collector();
            let (err_no, err_kind) = if let EbpfError::SyscallError(syscall_error) = err {
                if let Some(instruction_err) = syscall_error.downcast_ref::<InstructionError>() {
                    stable_log::program_failure(&logger, program_id, &instruction_err.to_string());
                    (instr_err_to_num(instruction_err), ErrKind::Instruction)
                } else if let Some(syscall_error) = syscall_error.downcast_ref::<SyscallError>() {
                    stable_log::program_failure(&logger, program_id, &syscall_error.to_string());
                    (syscall_err_to_num(syscall_error), ErrKind::Syscall)
                } else if let Some(memory_error) =
                    syscall_error.downcast_ref::<MemoryTranslationError>()
                {
                    let syscall_error: SyscallError = (memory_error.clone()).into();
                    stable_log::program_failure(&logger, program_id, &syscall_error.to_string());
                    (syscall_err_to_num(&syscall_error), ErrKind::Syscall)
                } else if let Some(cpi_error) = syscall_error.downcast_ref::<CpiError>() {
                    let syscall_error: SyscallError = (cpi_error.clone()).into();
                    stable_log::program_failure(&logger, program_id, &syscall_error.to_string());
                    (syscall_err_to_num(&syscall_error), ErrKind::Syscall)
                } else if let Some(ebpf_error) = syscall_error.downcast_ref::<EbpfError>() {
                    stable_log::program_failure(&logger, program_id, &ebpf_error.to_string());
                    (ebpf_err_to_num(ebpf_error), ErrKind::Ebpf)
                } else if syscall_error
                    .downcast_ref::<PoseidonSyscallError>()
                    .is_some()
                {
                    (-1, ErrKind::Syscall)
                } else {
                    stable_log::program_failure(&logger, program_id, &err.to_string());
                    (-1, ErrKind::Unspecified)
                }
            } else {
                stable_log::program_failure(&logger, program_id, &err.to_string());
                (ebpf_err_to_num(err), ErrKind::Ebpf)
            };
            (err_no as i64, err_kind, 0)
        }
    }
}
