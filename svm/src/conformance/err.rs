//! Error-code mapping for VM execution results.

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

pub(crate) fn elf_error_code(error: &ElfError) -> u32 {
    (error.discriminant() as u32).saturating_add(1)
}

fn ebpf_error_code(error: &EbpfError) -> i64 {
    (error.discriminant() as i64).saturating_add(1)
}

fn syscall_error_code(error: &SyscallError) -> i64 {
    (error.discriminant() as i64).saturating_add(1)
}

pub fn serialized_error_code<T: serde::Serialize>(error: &T) -> u32 {
    let serialized = bincode::serialize(error).unwrap();
    u32::from_le_bytes(serialized[0..4].try_into().unwrap()).saturating_add(1)
}

/// A VM `program_result` mapped into the fields a conformance fixture compares.
pub(crate) struct UnpackedResult {
    /// Error number, or `0` on success.
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

    fn err(error: i64, error_kind: i32) -> Self {
        Self {
            error,
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
                    Self::err(serialized_error_code(e) as i64, Self::ERR_KIND_INSTRUCTION)
                } else if let Some(e) = boxed.downcast_ref::<SyscallError>() {
                    Self::err(syscall_error_code(e), Self::ERR_KIND_SYSCALL)
                } else if let Some(e) = boxed.downcast_ref::<MemoryTranslationError>() {
                    Self::err(
                        syscall_error_code(&e.clone().into()),
                        Self::ERR_KIND_SYSCALL,
                    )
                } else if let Some(e) = boxed.downcast_ref::<CpiError>() {
                    Self::err(
                        syscall_error_code(&e.clone().into()),
                        Self::ERR_KIND_SYSCALL,
                    )
                } else if let Some(e) = boxed.downcast_ref::<EbpfError>() {
                    Self::err(ebpf_error_code(e), Self::ERR_KIND_EBPF)
                } else if boxed.downcast_ref::<PoseidonSyscallError>().is_some() {
                    Self::err(-1, Self::ERR_KIND_SYSCALL)
                } else {
                    Self::err(-1, Self::ERR_KIND_UNSPECIFIED)
                }
            }
            _ => Self::err(ebpf_error_code(&ebpf_err), Self::ERR_KIND_EBPF),
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
