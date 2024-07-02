#![allow(clippy::arithmetic_side_effects)]
// Copyright 2017 Rich Lane <lanerl@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

extern crate solana_rbpf;

use {
    solana_rbpf::{
        assembler::assemble,
        ebpf,
        program::BuiltinProgram,
        test_utils::{TCP_SACK_ASM, TCP_SACK_BIN},
        vm::TestContextObject,
    },
    std::sync::Arc,
};

fn asm(src: &str) -> Result<Vec<ebpf::Insn>, String> {
    let executable = assemble::<TestContextObject>(src, Arc::new(BuiltinProgram::new_mock()))?;
    let (_program_vm_addr, program) = executable.get_text_bytes();
    Ok((0..program.len() / ebpf::INSN_SIZE)
        .map(|insn_ptr| ebpf::get_insn(program, insn_ptr))
        .collect())
}

fn insn(ptr: usize, opc: u8, dst: u8, src: u8, off: i16, imm: i64) -> ebpf::Insn {
    ebpf::Insn {
        ptr,
        opc,
        dst,
        src,
        off,
        imm,
    }
}

#[test]
fn test_empty() {
    assert_eq!(asm(""), Ok(vec![]));
}

#[test]
fn test_fill() {
    assert_eq!(
        asm(".fill 2, 0x210F"),
        Ok(vec![
            insn(0, ebpf::ADD64_REG, 1, 2, 0, 0),
            insn(1, ebpf::ADD64_REG, 1, 2, 0, 0)
        ])
    );
}

// Example for InstructionType::NoOperand.
#[test]
fn test_exit() {
    assert_eq!(asm("exit"), Ok(vec![insn(0, ebpf::EXIT, 0, 0, 0, 0)]));
}

// Example for InstructionType::AluBinary.
#[test]
fn test_add64() {
    assert_eq!(
        asm("add64 r1, r3"),
        Ok(vec![insn(0, ebpf::ADD64_REG, 1, 3, 0, 0)])
    );
    assert_eq!(
        asm("add64 r1, 5"),
        Ok(vec![insn(0, ebpf::ADD64_IMM, 1, 0, 0, 5)])
    );
}

// Example for InstructionType::AluUnary.
#[test]
fn test_neg64() {
    assert_eq!(asm("neg64 r1"), Ok(vec![insn(0, ebpf::NEG64, 1, 0, 0, 0)]));
}

// Example for InstructionType::LoadReg.
#[test]
fn test_ldxw() {
    assert_eq!(
        asm("ldxw r1, [r2+5]"),
        Ok(vec![insn(0, ebpf::LD_W_REG, 1, 2, 5, 0)])
    );
}

// Example for InstructionType::StoreImm.
#[test]
fn test_stw() {
    assert_eq!(
        asm("stw [r2+5], 7"),
        Ok(vec![insn(0, ebpf::ST_W_IMM, 2, 0, 5, 7)])
    );
}

// Example for InstructionType::StoreReg.
#[test]
fn test_stxw() {
    assert_eq!(
        asm("stxw [r2+5], r8"),
        Ok(vec![insn(0, ebpf::ST_W_REG, 2, 8, 5, 0)])
    );
}

// Example for InstructionType::JumpUnconditional.
#[test]
fn test_ja() {
    assert_eq!(asm("ja +8"), Ok(vec![insn(0, ebpf::JA, 0, 0, 8, 0)]));
    assert_eq!(asm("ja -3"), Ok(vec![insn(0, ebpf::JA, 0, 0, -3, 0)]));
}

// Example for InstructionType::JumpConditional.
#[test]
fn test_jeq() {
    assert_eq!(
        asm("jeq r1, 4, +8"),
        Ok(vec![insn(0, ebpf::JEQ_IMM, 1, 0, 8, 4)])
    );
    assert_eq!(
        asm("jeq r1, r3, +8"),
        Ok(vec![insn(0, ebpf::JEQ_REG, 1, 3, 8, 0)])
    );
}

#[test]
fn test_call_reg() {
    assert_eq!(
        asm("callx r3"),
        Ok(vec![insn(0, ebpf::CALL_REG, 0, 3, 0, 0)])
    );
}

// Example for InstructionType::Call.
#[test]
fn test_call_imm() {
    assert_eq!(
        asm("call 299"),
        Ok(vec![insn(0, ebpf::CALL_IMM, 0, 1, 0, 300)])
    );
}

// Example for InstructionType::Endian.
#[test]
fn test_be32() {
    assert_eq!(asm("be32 r1"), Ok(vec![insn(0, ebpf::BE, 1, 0, 0, 32)]));
}

// Example for InstructionType::LoadImm.
#[test]
fn test_lddw() {
    assert_eq!(
        asm("lddw r1, 0x1234abcd5678eeff"),
        Ok(vec![
            insn(0, ebpf::LD_DW_IMM, 1, 0, 0, 0x5678eeff),
            insn(1, 0, 0, 0, 0, 0x1234abcd)
        ])
    );
    assert_eq!(
        asm("lddw r1, 0xff11ee22dd33cc44"),
        Ok(vec![
            insn(0, ebpf::LD_DW_IMM, 1, 0, 0, 0xffffffffdd33cc44u64 as i64),
            insn(1, 0, 0, 0, 0, 0xffffffffff11ee22u64 as i64)
        ])
    );
}

// Example for InstructionType::LoadReg.
#[test]
fn test_ldxdw() {
    assert_eq!(
        asm("ldxdw r1, [r2+3]"),
        Ok(vec![insn(0, ebpf::LD_DW_REG, 1, 2, 3, 0)])
    );
}

// Example for InstructionType::StoreImm.
#[test]
fn test_sth() {
    assert_eq!(
        asm("sth [r1+2], 3"),
        Ok(vec![insn(0, ebpf::ST_H_IMM, 1, 0, 2, 3)])
    );
}

// Example for InstructionType::StoreReg.
#[test]
fn test_stxh() {
    assert_eq!(
        asm("stxh [r1+2], r3"),
        Ok(vec![insn(0, ebpf::ST_H_REG, 1, 3, 2, 0)])
    );
}

// Test all supported AluBinary mnemonics.
#[test]
fn test_alu_binary() {
    assert_eq!(
        asm("add r1, r2
             sub r1, r2
             mul r1, r2
             div r1, r2
             or r1, r2
             and r1, r2
             lsh r1, r2
             rsh r1, r2
             mod r1, r2
             xor r1, r2
             mov r1, r2
             arsh r1, r2"),
        Ok(vec![
            insn(0, ebpf::ADD64_REG, 1, 2, 0, 0),
            insn(1, ebpf::SUB64_REG, 1, 2, 0, 0),
            insn(2, ebpf::MUL64_REG, 1, 2, 0, 0),
            insn(3, ebpf::DIV64_REG, 1, 2, 0, 0),
            insn(4, ebpf::OR64_REG, 1, 2, 0, 0),
            insn(5, ebpf::AND64_REG, 1, 2, 0, 0),
            insn(6, ebpf::LSH64_REG, 1, 2, 0, 0),
            insn(7, ebpf::RSH64_REG, 1, 2, 0, 0),
            insn(8, ebpf::MOD64_REG, 1, 2, 0, 0),
            insn(9, ebpf::XOR64_REG, 1, 2, 0, 0),
            insn(10, ebpf::MOV64_REG, 1, 2, 0, 0),
            insn(11, ebpf::ARSH64_REG, 1, 2, 0, 0)
        ])
    );

    assert_eq!(
        asm("add r1, 2
             sub r1, 2
             mul r1, 2
             div r1, 2
             or r1, 2
             and r1, 2
             lsh r1, 2
             rsh r1, 2
             mod r1, 2
             xor r1, 2
             mov r1, 2
             arsh r1, 2"),
        Ok(vec![
            insn(0, ebpf::ADD64_IMM, 1, 0, 0, 2),
            insn(1, ebpf::SUB64_IMM, 1, 0, 0, 2),
            insn(2, ebpf::MUL64_IMM, 1, 0, 0, 2),
            insn(3, ebpf::DIV64_IMM, 1, 0, 0, 2),
            insn(4, ebpf::OR64_IMM, 1, 0, 0, 2),
            insn(5, ebpf::AND64_IMM, 1, 0, 0, 2),
            insn(6, ebpf::LSH64_IMM, 1, 0, 0, 2),
            insn(7, ebpf::RSH64_IMM, 1, 0, 0, 2),
            insn(8, ebpf::MOD64_IMM, 1, 0, 0, 2),
            insn(9, ebpf::XOR64_IMM, 1, 0, 0, 2),
            insn(10, ebpf::MOV64_IMM, 1, 0, 0, 2),
            insn(11, ebpf::ARSH64_IMM, 1, 0, 0, 2)
        ])
    );

    assert_eq!(
        asm("add64 r1, r2
             sub64 r1, r2
             mul64 r1, r2
             div64 r1, r2
             or64 r1, r2
             and64 r1, r2
             lsh64 r1, r2
             rsh64 r1, r2
             mod64 r1, r2
             xor64 r1, r2
             mov64 r1, r2
             arsh64 r1, r2"),
        Ok(vec![
            insn(0, ebpf::ADD64_REG, 1, 2, 0, 0),
            insn(1, ebpf::SUB64_REG, 1, 2, 0, 0),
            insn(2, ebpf::MUL64_REG, 1, 2, 0, 0),
            insn(3, ebpf::DIV64_REG, 1, 2, 0, 0),
            insn(4, ebpf::OR64_REG, 1, 2, 0, 0),
            insn(5, ebpf::AND64_REG, 1, 2, 0, 0),
            insn(6, ebpf::LSH64_REG, 1, 2, 0, 0),
            insn(7, ebpf::RSH64_REG, 1, 2, 0, 0),
            insn(8, ebpf::MOD64_REG, 1, 2, 0, 0),
            insn(9, ebpf::XOR64_REG, 1, 2, 0, 0),
            insn(10, ebpf::MOV64_REG, 1, 2, 0, 0),
            insn(11, ebpf::ARSH64_REG, 1, 2, 0, 0)
        ])
    );

    assert_eq!(
        asm("add64 r1, 2
             sub64 r1, 2
             mul64 r1, 2
             div64 r1, 2
             or64 r1, 2
             and64 r1, 2
             lsh64 r1, 2
             rsh64 r1, 2
             mod64 r1, 2
             xor64 r1, 2
             mov64 r1, 2
             arsh64 r1, 2"),
        Ok(vec![
            insn(0, ebpf::ADD64_IMM, 1, 0, 0, 2),
            insn(1, ebpf::SUB64_IMM, 1, 0, 0, 2),
            insn(2, ebpf::MUL64_IMM, 1, 0, 0, 2),
            insn(3, ebpf::DIV64_IMM, 1, 0, 0, 2),
            insn(4, ebpf::OR64_IMM, 1, 0, 0, 2),
            insn(5, ebpf::AND64_IMM, 1, 0, 0, 2),
            insn(6, ebpf::LSH64_IMM, 1, 0, 0, 2),
            insn(7, ebpf::RSH64_IMM, 1, 0, 0, 2),
            insn(8, ebpf::MOD64_IMM, 1, 0, 0, 2),
            insn(9, ebpf::XOR64_IMM, 1, 0, 0, 2),
            insn(10, ebpf::MOV64_IMM, 1, 0, 0, 2),
            insn(11, ebpf::ARSH64_IMM, 1, 0, 0, 2)
        ])
    );

    assert_eq!(
        asm("add32 r1, r2
             sub32 r1, r2
             mul32 r1, r2
             div32 r1, r2
             or32 r1, r2
             and32 r1, r2
             lsh32 r1, r2
             rsh32 r1, r2
             mod32 r1, r2
             xor32 r1, r2
             mov32 r1, r2
             arsh32 r1, r2"),
        Ok(vec![
            insn(0, ebpf::ADD32_REG, 1, 2, 0, 0),
            insn(1, ebpf::SUB32_REG, 1, 2, 0, 0),
            insn(2, ebpf::MUL32_REG, 1, 2, 0, 0),
            insn(3, ebpf::DIV32_REG, 1, 2, 0, 0),
            insn(4, ebpf::OR32_REG, 1, 2, 0, 0),
            insn(5, ebpf::AND32_REG, 1, 2, 0, 0),
            insn(6, ebpf::LSH32_REG, 1, 2, 0, 0),
            insn(7, ebpf::RSH32_REG, 1, 2, 0, 0),
            insn(8, ebpf::MOD32_REG, 1, 2, 0, 0),
            insn(9, ebpf::XOR32_REG, 1, 2, 0, 0),
            insn(10, ebpf::MOV32_REG, 1, 2, 0, 0),
            insn(11, ebpf::ARSH32_REG, 1, 2, 0, 0)
        ])
    );

    assert_eq!(
        asm("add32 r1, 2
             sub32 r1, 2
             mul32 r1, 2
             div32 r1, 2
             or32 r1, 2
             and32 r1, 2
             lsh32 r1, 2
             rsh32 r1, 2
             mod32 r1, 2
             xor32 r1, 2
             mov32 r1, 2
             arsh32 r1, 2"),
        Ok(vec![
            insn(0, ebpf::ADD32_IMM, 1, 0, 0, 2),
            insn(1, ebpf::SUB32_IMM, 1, 0, 0, 2),
            insn(2, ebpf::MUL32_IMM, 1, 0, 0, 2),
            insn(3, ebpf::DIV32_IMM, 1, 0, 0, 2),
            insn(4, ebpf::OR32_IMM, 1, 0, 0, 2),
            insn(5, ebpf::AND32_IMM, 1, 0, 0, 2),
            insn(6, ebpf::LSH32_IMM, 1, 0, 0, 2),
            insn(7, ebpf::RSH32_IMM, 1, 0, 0, 2),
            insn(8, ebpf::MOD32_IMM, 1, 0, 0, 2),
            insn(9, ebpf::XOR32_IMM, 1, 0, 0, 2),
            insn(10, ebpf::MOV32_IMM, 1, 0, 0, 2),
            insn(11, ebpf::ARSH32_IMM, 1, 0, 0, 2)
        ])
    );
}

// Test all supported AluUnary mnemonics.
#[test]
fn test_alu_unary() {
    assert_eq!(
        asm("neg r1
             neg64 r1
             neg32 r1"),
        Ok(vec![
            insn(0, ebpf::NEG64, 1, 0, 0, 0),
            insn(1, ebpf::NEG64, 1, 0, 0, 0),
            insn(2, ebpf::NEG32, 1, 0, 0, 0)
        ])
    );
}

// Test all supported LoadReg mnemonics.
#[test]
fn test_load_reg() {
    assert_eq!(
        asm("ldxw r1, [r2+3]
             ldxh r1, [r2+3]
             ldxb r1, [r2+3]
             ldxdw r1, [r2+3]"),
        Ok(vec![
            insn(0, ebpf::LD_W_REG, 1, 2, 3, 0),
            insn(1, ebpf::LD_H_REG, 1, 2, 3, 0),
            insn(2, ebpf::LD_B_REG, 1, 2, 3, 0),
            insn(3, ebpf::LD_DW_REG, 1, 2, 3, 0)
        ])
    );
}

// Test all supported StoreImm mnemonics.
#[test]
fn test_store_imm() {
    assert_eq!(
        asm("stw [r1+2], 3
             sth [r1+2], 3
             stb [r1+2], 3
             stdw [r1+2], 3"),
        Ok(vec![
            insn(0, ebpf::ST_W_IMM, 1, 0, 2, 3),
            insn(1, ebpf::ST_H_IMM, 1, 0, 2, 3),
            insn(2, ebpf::ST_B_IMM, 1, 0, 2, 3),
            insn(3, ebpf::ST_DW_IMM, 1, 0, 2, 3)
        ])
    );
}

// Test all supported StoreReg mnemonics.
#[test]
fn test_store_reg() {
    assert_eq!(
        asm("stxw [r1+2], r3
             stxh [r1+2], r3
             stxb [r1+2], r3
             stxdw [r1+2], r3"),
        Ok(vec![
            insn(0, ebpf::ST_W_REG, 1, 3, 2, 0),
            insn(1, ebpf::ST_H_REG, 1, 3, 2, 0),
            insn(2, ebpf::ST_B_REG, 1, 3, 2, 0),
            insn(3, ebpf::ST_DW_REG, 1, 3, 2, 0)
        ])
    );
}

// Test all supported JumpConditional mnemonics.
#[test]
fn test_jump_conditional() {
    assert_eq!(
        asm("jeq r1, r2, +3
             jgt r1, r2, +3
             jge r1, r2, +3
             jlt r1, r2, +3
             jle r1, r2, +3
             jset r1, r2, +3
             jne r1, r2, +3
             jsgt r1, r2, +3
             jsge r1, r2, +3
             jslt r1, r2, +3
             jsle r1, r2, +3"),
        Ok(vec![
            insn(0, ebpf::JEQ_REG, 1, 2, 3, 0),
            insn(1, ebpf::JGT_REG, 1, 2, 3, 0),
            insn(2, ebpf::JGE_REG, 1, 2, 3, 0),
            insn(3, ebpf::JLT_REG, 1, 2, 3, 0),
            insn(4, ebpf::JLE_REG, 1, 2, 3, 0),
            insn(5, ebpf::JSET_REG, 1, 2, 3, 0),
            insn(6, ebpf::JNE_REG, 1, 2, 3, 0),
            insn(7, ebpf::JSGT_REG, 1, 2, 3, 0),
            insn(8, ebpf::JSGE_REG, 1, 2, 3, 0),
            insn(9, ebpf::JSLT_REG, 1, 2, 3, 0),
            insn(10, ebpf::JSLE_REG, 1, 2, 3, 0)
        ])
    );

    assert_eq!(
        asm("jeq r1, 2, +3
             jgt r1, 2, +3
             jge r1, 2, +3
             jlt r1, 2, +3
             jle r1, 2, +3
             jset r1, 2, +3
             jne r1, 2, +3
             jsgt r1, 2, +3
             jsge r1, 2, +3
             jslt r1, 2, +3
             jsle r1, 2, +3"),
        Ok(vec![
            insn(0, ebpf::JEQ_IMM, 1, 0, 3, 2),
            insn(1, ebpf::JGT_IMM, 1, 0, 3, 2),
            insn(2, ebpf::JGE_IMM, 1, 0, 3, 2),
            insn(3, ebpf::JLT_IMM, 1, 0, 3, 2),
            insn(4, ebpf::JLE_IMM, 1, 0, 3, 2),
            insn(5, ebpf::JSET_IMM, 1, 0, 3, 2),
            insn(6, ebpf::JNE_IMM, 1, 0, 3, 2),
            insn(7, ebpf::JSGT_IMM, 1, 0, 3, 2),
            insn(8, ebpf::JSGE_IMM, 1, 0, 3, 2),
            insn(9, ebpf::JSLT_IMM, 1, 0, 3, 2),
            insn(10, ebpf::JSLE_IMM, 1, 0, 3, 2)
        ])
    );
}

// Test all supported Endian mnemonics.
#[test]
fn test_endian() {
    assert_eq!(
        asm("be16 r1
             be32 r1
             be64 r1
             le16 r1
             le32 r1
             le64 r1"),
        Ok(vec![
            insn(0, ebpf::BE, 1, 0, 0, 16),
            insn(1, ebpf::BE, 1, 0, 0, 32),
            insn(2, ebpf::BE, 1, 0, 0, 64),
            insn(3, ebpf::LE, 1, 0, 0, 16),
            insn(4, ebpf::LE, 1, 0, 0, 32),
            insn(5, ebpf::LE, 1, 0, 0, 64)
        ])
    );
}

#[test]
fn test_large_immediate() {
    assert_eq!(
        asm("add64 r1, 2147483647"),
        Ok(vec![insn(0, ebpf::ADD64_IMM, 1, 0, 0, 2147483647)])
    );
    assert_eq!(
        asm("add64 r1, -2147483648"),
        Ok(vec![insn(0, ebpf::ADD64_IMM, 1, 0, 0, -2147483648)])
    );
}

#[test]
fn test_tcp_sack() {
    let executable =
        assemble::<TestContextObject>(TCP_SACK_ASM, Arc::new(BuiltinProgram::new_mock())).unwrap();
    let (_program_vm_addr, program) = executable.get_text_bytes();
    assert_eq!(program, TCP_SACK_BIN.to_vec());
}

#[test]
fn test_error_invalid_instruction() {
    assert_eq!(asm("abcd"), Err("Invalid instruction \"abcd\"".to_string()));
}

#[test]
fn test_error_unexpected_operands() {
    assert_eq!(
        asm("add 1, 2"),
        Err("Unexpected operands: [Integer(1), Integer(2)]".to_string())
    );
}

#[test]
fn test_error_operands_out_of_range() {
    assert_eq!(
        asm("add r16, r2"),
        Err("Invalid destination register 16".to_string())
    );
    assert_eq!(
        asm("add r1, r16"),
        Err("Invalid source register 16".to_string())
    );
    assert_eq!(asm("ja -32769"), Err("Invalid offset -32769".to_string()));
    assert_eq!(asm("ja 32768"), Err("Invalid offset 32768".to_string()));
    assert_eq!(
        asm("add r1, 4294967296"),
        Err("Invalid immediate 4294967296".to_string())
    );
    assert_eq!(
        asm("add r1, 2147483648"),
        Err("Invalid immediate 2147483648".to_string())
    );
    assert_eq!(
        asm("add r1, -2147483649"),
        Err("Invalid immediate -2147483649".to_string())
    );
}
