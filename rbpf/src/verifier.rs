#![allow(clippy::arithmetic_side_effects)]
// Derived from uBPF <https://github.com/iovisor/ubpf>
// Copyright 2015 Big Switch Networks, Inc
//      (uBPF: safety checks, originally in C)
// Copyright 2016 6WIND S.A. <quentin.monnet@6wind.com>
//      (Translation to Rust)
// Copyright 2020 Solana Maintainers <maintainers@solana.com>
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! This “verifier” performs simple checks when the eBPF program is loaded into the VM (before it is
//! interpreted or JIT-compiled). It has nothing to do with the much more elaborated verifier inside
//! Linux kernel. There is no verification regarding the program flow control (should be a Direct
//! Acyclic Graph) or the consistency for registers usage (the verifier of the kernel assigns types
//! to the registers and is much stricter).
//!
//! On the other hand, rbpf is not expected to run in kernel space.
//!
//! Improving the verifier would be nice, but this is not trivial (and Linux kernel is under GPL
//! license, so we cannot copy it).
//!
//! Contrary to the verifier of the Linux kernel, this one does not modify the bytecode at all.

use crate::{
    ebpf,
    program::{FunctionRegistry, SBPFVersion},
    vm::Config,
};
use thiserror::Error;

/// Error definitions
#[derive(Debug, Error, Eq, PartialEq)]
pub enum VerifierError {
    /// ProgramLengthNotMultiple
    #[error("program length must be a multiple of {} octets", ebpf::INSN_SIZE)]
    ProgramLengthNotMultiple,
    /// Deprecated
    #[error("Deprecated")]
    ProgramTooLarge(usize),
    /// NoProgram
    #[error("no program set, call prog_set() to load one")]
    NoProgram,
    /// Division by zero
    #[error("division by 0 (insn #{0})")]
    DivisionByZero(usize),
    /// UnsupportedLEBEArgument
    #[error("unsupported argument for LE/BE (insn #{0})")]
    UnsupportedLEBEArgument(usize),
    /// LDDWCannotBeLast
    #[error("LD_DW instruction cannot be last in program")]
    LDDWCannotBeLast,
    /// IncompleteLDDW
    #[error("incomplete LD_DW instruction (insn #{0})")]
    IncompleteLDDW(usize),
    /// InfiniteLoop
    #[error("infinite loop (insn #{0})")]
    InfiniteLoop(usize),
    /// JumpOutOfCode
    #[error("jump out of code to #{0} (insn #{1})")]
    JumpOutOfCode(usize, usize),
    /// JumpToMiddleOfLDDW
    #[error("jump to middle of LD_DW at #{0} (insn #{1})")]
    JumpToMiddleOfLDDW(usize, usize),
    /// InvalidSourceRegister
    #[error("invalid source register (insn #{0})")]
    InvalidSourceRegister(usize),
    /// CannotWriteR10
    #[error("cannot write into register r10 (insn #{0})")]
    CannotWriteR10(usize),
    /// InvalidDestinationRegister
    #[error("invalid destination register (insn #{0})")]
    InvalidDestinationRegister(usize),
    /// UnknownOpCode
    #[error("unknown eBPF opcode {0:#2x} (insn #{1:?})")]
    UnknownOpCode(u8, usize),
    /// Shift with overflow
    #[error("Shift with overflow of {0}-bit value by {1} (insn #{2:?})")]
    ShiftWithOverflow(u64, u64, usize),
    /// Invalid register specified
    #[error("Invalid register specified at instruction {0}")]
    InvalidRegister(usize),
    /// Invalid function
    #[error("Invalid function at instruction {0}")]
    InvalidFunction(usize),
}

/// eBPF Verifier
pub trait Verifier {
    /// eBPF verification function that returns an error if the program does not meet its requirements.
    ///
    /// Some examples of things the verifier may reject the program for:
    ///
    ///   - Program does not terminate.
    ///   - Unknown instructions.
    ///   - Bad formed instruction.
    ///   - Unknown eBPF syscall index.
    fn verify(
        prog: &[u8],
        config: &Config,
        sbpf_version: &SBPFVersion,
        function_registry: &FunctionRegistry<usize>,
    ) -> Result<(), VerifierError>;
}

fn check_prog_len(prog: &[u8]) -> Result<(), VerifierError> {
    if prog.len() % ebpf::INSN_SIZE != 0 {
        return Err(VerifierError::ProgramLengthNotMultiple);
    }
    if prog.is_empty() {
        return Err(VerifierError::NoProgram);
    }
    Ok(())
}

fn check_imm_nonzero(insn: &ebpf::Insn, insn_ptr: usize) -> Result<(), VerifierError> {
    if insn.imm == 0 {
        return Err(VerifierError::DivisionByZero(insn_ptr));
    }
    Ok(())
}

fn check_imm_endian(insn: &ebpf::Insn, insn_ptr: usize) -> Result<(), VerifierError> {
    match insn.imm {
        16 | 32 | 64 => Ok(()),
        _ => Err(VerifierError::UnsupportedLEBEArgument(insn_ptr)),
    }
}

fn check_load_dw(prog: &[u8], insn_ptr: usize) -> Result<(), VerifierError> {
    if (insn_ptr + 1) * ebpf::INSN_SIZE >= prog.len() {
        // Last instruction cannot be LD_DW because there would be no 2nd DW
        return Err(VerifierError::LDDWCannotBeLast);
    }
    let next_insn = ebpf::get_insn(prog, insn_ptr + 1);
    if next_insn.opc != 0 {
        return Err(VerifierError::IncompleteLDDW(insn_ptr));
    }
    Ok(())
}

fn check_jmp_offset(
    prog: &[u8],
    insn_ptr: usize,
    function_range: &std::ops::Range<usize>,
) -> Result<(), VerifierError> {
    let insn = ebpf::get_insn(prog, insn_ptr);

    let dst_insn_ptr = insn_ptr as isize + 1 + insn.off as isize;
    if dst_insn_ptr < 0 || !function_range.contains(&(dst_insn_ptr as usize)) {
        return Err(VerifierError::JumpOutOfCode(
            dst_insn_ptr as usize,
            insn_ptr,
        ));
    }
    let dst_insn = ebpf::get_insn(prog, dst_insn_ptr as usize);
    if dst_insn.opc == 0 {
        return Err(VerifierError::JumpToMiddleOfLDDW(
            dst_insn_ptr as usize,
            insn_ptr,
        ));
    }
    Ok(())
}

fn check_call_target(
    key: u32,
    function_registry: &FunctionRegistry<usize>,
) -> Result<(), VerifierError> {
    function_registry
        .lookup_by_key(key)
        .map(|_| ())
        .ok_or(VerifierError::InvalidFunction(key as usize))
}

fn check_registers(
    insn: &ebpf::Insn,
    store: bool,
    insn_ptr: usize,
    sbpf_version: &SBPFVersion,
) -> Result<(), VerifierError> {
    if insn.src > 10 {
        return Err(VerifierError::InvalidSourceRegister(insn_ptr));
    }

    match (insn.dst, store) {
        (0..=9, _) | (10, true) => Ok(()),
        (11, _) if sbpf_version.dynamic_stack_frames() && insn.opc == ebpf::ADD64_IMM => Ok(()),
        (10, false) => Err(VerifierError::CannotWriteR10(insn_ptr)),
        (_, _) => Err(VerifierError::InvalidDestinationRegister(insn_ptr)),
    }
}

/// Check that the imm is a valid shift operand
fn check_imm_shift(insn: &ebpf::Insn, insn_ptr: usize, imm_bits: u64) -> Result<(), VerifierError> {
    let shift_by = insn.imm as u64;
    if insn.imm < 0 || shift_by >= imm_bits {
        return Err(VerifierError::ShiftWithOverflow(
            shift_by, imm_bits, insn_ptr,
        ));
    }
    Ok(())
}

/// Check that callx has a valid register number
fn check_callx_register(
    insn: &ebpf::Insn,
    insn_ptr: usize,
    config: &Config,
    sbpf_version: &SBPFVersion,
) -> Result<(), VerifierError> {
    let reg = if sbpf_version.callx_uses_src_reg() {
        insn.src as i64
    } else {
        insn.imm
    };
    if !(0..=10).contains(&reg) || (reg == 10 && config.reject_callx_r10) {
        return Err(VerifierError::InvalidRegister(insn_ptr));
    }
    Ok(())
}

/// Mandatory verifier for solana programs to run on-chain
#[derive(Debug)]
pub struct RequisiteVerifier {}
impl Verifier for RequisiteVerifier {
    /// Check the program against the verifier's rules
    #[rustfmt::skip]
    fn verify(prog: &[u8], config: &Config, sbpf_version: &SBPFVersion, function_registry: &FunctionRegistry<usize>) -> Result<(), VerifierError> {
        check_prog_len(prog)?;

        let program_range = 0..prog.len() / ebpf::INSN_SIZE;
        let mut function_iter = function_registry.keys().map(|insn_ptr| insn_ptr as usize).peekable();
        let mut function_range = program_range.start..program_range.end;
        let mut insn_ptr: usize = 0;
        while (insn_ptr + 1) * ebpf::INSN_SIZE <= prog.len() {
            let insn = ebpf::get_insn(prog, insn_ptr);
            let mut store = false;

            if sbpf_version.static_syscalls() && function_iter.peek() == Some(&insn_ptr) {
                function_range.start = function_iter.next().unwrap_or(0);
                function_range.end = *function_iter.peek().unwrap_or(&program_range.end);
                let insn = ebpf::get_insn(prog, function_range.end.saturating_sub(1));
                match insn.opc {
                    ebpf::JA | ebpf::EXIT => {},
                    _ => return Err(VerifierError::InvalidFunction(
                        function_range.end.saturating_sub(1),
                    )),
                }
            }

            match insn.opc {
                ebpf::LD_DW_IMM if sbpf_version.enable_lddw() => {
                    check_load_dw(prog, insn_ptr)?;
                    insn_ptr += 1;
                },

                // BPF_LDX class
                ebpf::LD_B_REG   => {},
                ebpf::LD_H_REG   => {},
                ebpf::LD_W_REG   => {},
                ebpf::LD_DW_REG  => {},

                // BPF_ST class
                ebpf::ST_B_IMM   => store = true,
                ebpf::ST_H_IMM   => store = true,
                ebpf::ST_W_IMM   => store = true,
                ebpf::ST_DW_IMM  => store = true,

                // BPF_STX class
                ebpf::ST_B_REG   => store = true,
                ebpf::ST_H_REG   => store = true,
                ebpf::ST_W_REG   => store = true,
                ebpf::ST_DW_REG  => store = true,

                // BPF_ALU class
                ebpf::ADD32_IMM  => {},
                ebpf::ADD32_REG  => {},
                ebpf::SUB32_IMM  => {},
                ebpf::SUB32_REG  => {},
                ebpf::MUL32_IMM  if !sbpf_version.enable_pqr() => {},
                ebpf::MUL32_REG  if !sbpf_version.enable_pqr() => {},
                ebpf::DIV32_IMM  if !sbpf_version.enable_pqr() => { check_imm_nonzero(&insn, insn_ptr)?; },
                ebpf::DIV32_REG  if !sbpf_version.enable_pqr() => {},
                ebpf::OR32_IMM   => {},
                ebpf::OR32_REG   => {},
                ebpf::AND32_IMM  => {},
                ebpf::AND32_REG  => {},
                ebpf::LSH32_IMM  => { check_imm_shift(&insn, insn_ptr, 32)?; },
                ebpf::LSH32_REG  => {},
                ebpf::RSH32_IMM  => { check_imm_shift(&insn, insn_ptr, 32)?; },
                ebpf::RSH32_REG  => {},
                ebpf::NEG32      if sbpf_version.enable_neg() => {},
                ebpf::MOD32_IMM  if !sbpf_version.enable_pqr() => { check_imm_nonzero(&insn, insn_ptr)?; },
                ebpf::MOD32_REG  if !sbpf_version.enable_pqr() => {},
                ebpf::XOR32_IMM  => {},
                ebpf::XOR32_REG  => {},
                ebpf::MOV32_IMM  => {},
                ebpf::MOV32_REG  => {},
                ebpf::ARSH32_IMM => { check_imm_shift(&insn, insn_ptr, 32)?; },
                ebpf::ARSH32_REG => {},
                ebpf::LE         if sbpf_version.enable_le() => { check_imm_endian(&insn, insn_ptr)?; },
                ebpf::BE         => { check_imm_endian(&insn, insn_ptr)?; },

                // BPF_ALU64 class
                ebpf::ADD64_IMM  => {},
                ebpf::ADD64_REG  => {},
                ebpf::SUB64_IMM  => {},
                ebpf::SUB64_REG  => {},
                ebpf::MUL64_IMM  if !sbpf_version.enable_pqr() => {},
                ebpf::MUL64_REG  if !sbpf_version.enable_pqr() => {},
                ebpf::DIV64_IMM  if !sbpf_version.enable_pqr() => { check_imm_nonzero(&insn, insn_ptr)?; },
                ebpf::DIV64_REG  if !sbpf_version.enable_pqr() => {},
                ebpf::OR64_IMM   => {},
                ebpf::OR64_REG   => {},
                ebpf::AND64_IMM  => {},
                ebpf::AND64_REG  => {},
                ebpf::LSH64_IMM  => { check_imm_shift(&insn, insn_ptr, 64)?; },
                ebpf::LSH64_REG  => {},
                ebpf::RSH64_IMM  => { check_imm_shift(&insn, insn_ptr, 64)?; },
                ebpf::RSH64_REG  => {},
                ebpf::NEG64      if sbpf_version.enable_neg() => {},
                ebpf::MOD64_IMM  if !sbpf_version.enable_pqr() => { check_imm_nonzero(&insn, insn_ptr)?; },
                ebpf::MOD64_REG  if !sbpf_version.enable_pqr() => {},
                ebpf::XOR64_IMM  => {},
                ebpf::XOR64_REG  => {},
                ebpf::MOV64_IMM  => {},
                ebpf::MOV64_REG  => {},
                ebpf::ARSH64_IMM => { check_imm_shift(&insn, insn_ptr, 64)?; },
                ebpf::ARSH64_REG => {},
                ebpf::HOR64_IMM  if !sbpf_version.enable_lddw() => {},

                // BPF_PQR class
                ebpf::LMUL32_IMM if sbpf_version.enable_pqr() => {},
                ebpf::LMUL32_REG if sbpf_version.enable_pqr() => {},
                ebpf::LMUL64_IMM if sbpf_version.enable_pqr() => {},
                ebpf::LMUL64_REG if sbpf_version.enable_pqr() => {},
                ebpf::UHMUL64_IMM if sbpf_version.enable_pqr() => {},
                ebpf::UHMUL64_REG if sbpf_version.enable_pqr() => {},
                ebpf::SHMUL64_IMM if sbpf_version.enable_pqr() => {},
                ebpf::SHMUL64_REG if sbpf_version.enable_pqr() => {},
                ebpf::UDIV32_IMM if sbpf_version.enable_pqr() => { check_imm_nonzero(&insn, insn_ptr)?; },
                ebpf::UDIV32_REG if sbpf_version.enable_pqr() => {},
                ebpf::UDIV64_IMM if sbpf_version.enable_pqr() => { check_imm_nonzero(&insn, insn_ptr)?; },
                ebpf::UDIV64_REG if sbpf_version.enable_pqr() => {},
                ebpf::UREM32_IMM if sbpf_version.enable_pqr() => { check_imm_nonzero(&insn, insn_ptr)?; },
                ebpf::UREM32_REG if sbpf_version.enable_pqr() => {},
                ebpf::UREM64_IMM if sbpf_version.enable_pqr() => { check_imm_nonzero(&insn, insn_ptr)?; },
                ebpf::UREM64_REG if sbpf_version.enable_pqr() => {},
                ebpf::SDIV32_IMM if sbpf_version.enable_pqr() => { check_imm_nonzero(&insn, insn_ptr)?; },
                ebpf::SDIV32_REG if sbpf_version.enable_pqr() => {},
                ebpf::SDIV64_IMM if sbpf_version.enable_pqr() => { check_imm_nonzero(&insn, insn_ptr)?; },
                ebpf::SDIV64_REG if sbpf_version.enable_pqr() => {},
                ebpf::SREM32_IMM if sbpf_version.enable_pqr() => { check_imm_nonzero(&insn, insn_ptr)?; },
                ebpf::SREM32_REG if sbpf_version.enable_pqr() => {},
                ebpf::SREM64_IMM if sbpf_version.enable_pqr() => { check_imm_nonzero(&insn, insn_ptr)?; },
                ebpf::SREM64_REG if sbpf_version.enable_pqr() => {},

                // BPF_JMP class
                ebpf::JA         => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JEQ_IMM    => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JEQ_REG    => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JGT_IMM    => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JGT_REG    => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JGE_IMM    => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JGE_REG    => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JLT_IMM    => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JLT_REG    => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JLE_IMM    => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JLE_REG    => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JSET_IMM   => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JSET_REG   => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JNE_IMM    => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JNE_REG    => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JSGT_IMM   => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JSGT_REG   => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JSGE_IMM   => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JSGE_REG   => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JSLT_IMM   => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JSLT_REG   => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JSLE_IMM   => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::JSLE_REG   => { check_jmp_offset(prog, insn_ptr, &function_range)?; },
                ebpf::CALL_IMM   if sbpf_version.static_syscalls() && insn.src != 0 => { check_call_target(insn.imm as u32, function_registry)?; },
                ebpf::CALL_IMM   => {},
                ebpf::CALL_REG   => { check_callx_register(&insn, insn_ptr, config, sbpf_version)?; },
                ebpf::EXIT       => {},

                _                => {
                    return Err(VerifierError::UnknownOpCode(insn.opc, insn_ptr));
                }
            }

            check_registers(&insn, store, insn_ptr, sbpf_version)?;

            insn_ptr += 1;
        }

        // insn_ptr should now be equal to number of instructions.
        if insn_ptr != prog.len() / ebpf::INSN_SIZE {
            return Err(VerifierError::JumpOutOfCode(insn_ptr, insn_ptr));
        }

        Ok(())
    }
}
