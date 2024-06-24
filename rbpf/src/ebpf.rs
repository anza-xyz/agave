#![allow(clippy::arithmetic_side_effects)]
// Copyright 2016 6WIND S.A. <quentin.monnet@6wind.com>
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! This module contains all the definitions related to eBPF, and some functions permitting to
//! manipulate eBPF instructions.
//!
//! The number of bytes in an instruction, the maximum number of instructions in a program, and
//! also all operation codes are defined here as constants.
//!
//! The structure for an instruction used by this crate, as well as the function to extract it from
//! a program, is also defined in the module.
//!
//! To learn more about these instructions, see the Linux kernel documentation:
//! <https://www.kernel.org/doc/Documentation/networking/filter.txt>, or for a shorter version of
//! the list of the operation codes: <https://github.com/iovisor/bpf-docs/blob/master/eBPF.md>

use byteorder::{ByteOrder, LittleEndian};
use hash32::{Hash, Hasher, Murmur3Hasher};
use std::fmt;

/// Solana BPF version flag
pub const EF_SBPF_V2: u32 = 0x20;
/// Maximum number of instructions in an eBPF program.
pub const PROG_MAX_INSNS: usize = 65_536;
/// Size of an eBPF instructions, in bytes.
pub const INSN_SIZE: usize = 8;
/// Frame pointer register
pub const FRAME_PTR_REG: usize = 10;
/// Stack pointer register
pub const STACK_PTR_REG: usize = 11;
/// First scratch register
pub const FIRST_SCRATCH_REG: usize = 6;
/// Number of scratch registers
pub const SCRATCH_REGS: usize = 4;
/// Alignment of the memory regions in host address space in bytes
pub const HOST_ALIGN: usize = 16;
/// Upper half of a pointer is the region index, lower half the virtual address inside that region.
pub const VIRTUAL_ADDRESS_BITS: usize = 32;

// Memory map regions virtual addresses need to be (1 << VIRTUAL_ADDRESS_BITS) bytes apart.
// Also the region at index 0 should be skipped to catch NULL ptr accesses.

/// Start of the program bits (text and ro segments) in the memory map
pub const MM_PROGRAM_START: u64 = 0x100000000;
/// Start of the stack in the memory map
pub const MM_STACK_START: u64 = 0x200000000;
/// Start of the heap in the memory map
pub const MM_HEAP_START: u64 = 0x300000000;
/// Start of the input buffers in the memory map
pub const MM_INPUT_START: u64 = 0x400000000;

// eBPF op codes.
// See also https://www.kernel.org/doc/Documentation/networking/filter.txt

// Three least significant bits are operation class:
/// BPF operation class: load from immediate. [DEPRECATED]
pub const BPF_LD: u8 = 0x00;
/// BPF operation class: load from register.
pub const BPF_LDX: u8 = 0x01;
/// BPF operation class: store immediate.
pub const BPF_ST: u8 = 0x02;
/// BPF operation class: store value from register.
pub const BPF_STX: u8 = 0x03;
/// BPF operation class: 32 bits arithmetic operation.
pub const BPF_ALU: u8 = 0x04;
/// BPF operation class: jump.
pub const BPF_JMP: u8 = 0x05;
/// BPF operation class: product / quotient / remainder.
pub const BPF_PQR: u8 = 0x06;
/// BPF operation class: 64 bits arithmetic operation.
pub const BPF_ALU64: u8 = 0x07;

// For load and store instructions:
// +------------+--------+------------+
// |   3 bits   | 2 bits |   3 bits   |
// |    mode    |  size  | insn class |
// +------------+--------+------------+
// (MSB)                          (LSB)

// Size modifiers:
/// BPF size modifier: word (4 bytes).
pub const BPF_W: u8 = 0x00;
/// BPF size modifier: half-word (2 bytes).
pub const BPF_H: u8 = 0x08;
/// BPF size modifier: byte (1 byte).
pub const BPF_B: u8 = 0x10;
/// BPF size modifier: double word (8 bytes).
pub const BPF_DW: u8 = 0x18;

// Mode modifiers:
/// BPF mode modifier: immediate value.
pub const BPF_IMM: u8 = 0x00;
/// BPF mode modifier: absolute load.
pub const BPF_ABS: u8 = 0x20;
/// BPF mode modifier: indirect load.
pub const BPF_IND: u8 = 0x40;
/// BPF mode modifier: load from / store to memory.
pub const BPF_MEM: u8 = 0x60;
// [ 0x80 reserved ]
// [ 0xa0 reserved ]
// [ 0xc0 reserved ]

// For arithmetic (BPF_ALU/BPF_ALU64) and jump (BPF_JMP) instructions:
// +----------------+--------+--------+
// |     4 bits     |1 b.|   3 bits   |
// | operation code | src| insn class |
// +----------------+----+------------+
// (MSB)                          (LSB)

// Source modifiers:
/// BPF source operand modifier: 32-bit immediate value.
pub const BPF_K: u8 = 0x00;
/// BPF source operand modifier: `src` register.
pub const BPF_X: u8 = 0x08;

// Operation codes -- BPF_ALU or BPF_ALU64 classes:
/// BPF ALU/ALU64 operation code: addition.
pub const BPF_ADD: u8 = 0x00;
/// BPF ALU/ALU64 operation code: subtraction.
pub const BPF_SUB: u8 = 0x10;
/// BPF ALU/ALU64 operation code: multiplication. [DEPRECATED]
pub const BPF_MUL: u8 = 0x20;
/// BPF ALU/ALU64 operation code: division. [DEPRECATED]
pub const BPF_DIV: u8 = 0x30;
/// BPF ALU/ALU64 operation code: or.
pub const BPF_OR: u8 = 0x40;
/// BPF ALU/ALU64 operation code: and.
pub const BPF_AND: u8 = 0x50;
/// BPF ALU/ALU64 operation code: left shift.
pub const BPF_LSH: u8 = 0x60;
/// BPF ALU/ALU64 operation code: right shift.
pub const BPF_RSH: u8 = 0x70;
/// BPF ALU/ALU64 operation code: negation. [DEPRECATED]
pub const BPF_NEG: u8 = 0x80;
/// BPF ALU/ALU64 operation code: modulus. [DEPRECATED]
pub const BPF_MOD: u8 = 0x90;
/// BPF ALU/ALU64 operation code: exclusive or.
pub const BPF_XOR: u8 = 0xa0;
/// BPF ALU/ALU64 operation code: move.
pub const BPF_MOV: u8 = 0xb0;
/// BPF ALU/ALU64 operation code: sign extending right shift.
pub const BPF_ARSH: u8 = 0xc0;
/// BPF ALU/ALU64 operation code: endianness conversion.
pub const BPF_END: u8 = 0xd0;
/// BPF ALU/ALU64 operation code: high or.
pub const BPF_HOR: u8 = 0xf0;

// Operation codes -- BPF_PQR class:
//    7         6               5                               4       3          2-0
// 0  Unsigned  Multiplication  Product Lower Half / Quotient   32 Bit  Immediate  PQR
// 1  Signed    Division        Product Upper Half / Remainder  64 Bit  Register   PQR
/// BPF PQR operation code: unsigned high multiplication.
pub const BPF_UHMUL: u8 = 0x20;
/// BPF PQR operation code: unsigned division quotient.
pub const BPF_UDIV: u8 = 0x40;
/// BPF PQR operation code: unsigned division remainder.
pub const BPF_UREM: u8 = 0x60;
/// BPF PQR operation code: low multiplication.
pub const BPF_LMUL: u8 = 0x80;
/// BPF PQR operation code: signed high multiplication.
pub const BPF_SHMUL: u8 = 0xA0;
/// BPF PQR operation code: signed division quotient.
pub const BPF_SDIV: u8 = 0xC0;
/// BPF PQR operation code: signed division remainder.
pub const BPF_SREM: u8 = 0xE0;

// Operation codes -- BPF_JMP class:
/// BPF JMP operation code: jump.
pub const BPF_JA: u8 = 0x00;
/// BPF JMP operation code: jump if equal.
pub const BPF_JEQ: u8 = 0x10;
/// BPF JMP operation code: jump if greater than.
pub const BPF_JGT: u8 = 0x20;
/// BPF JMP operation code: jump if greater or equal.
pub const BPF_JGE: u8 = 0x30;
/// BPF JMP operation code: jump if `src` & `reg`.
pub const BPF_JSET: u8 = 0x40;
/// BPF JMP operation code: jump if not equal.
pub const BPF_JNE: u8 = 0x50;
/// BPF JMP operation code: jump if greater than (signed).
pub const BPF_JSGT: u8 = 0x60;
/// BPF JMP operation code: jump if greater or equal (signed).
pub const BPF_JSGE: u8 = 0x70;
/// BPF JMP operation code: syscall function call.
pub const BPF_CALL: u8 = 0x80;
/// BPF JMP operation code: return from program.
pub const BPF_EXIT: u8 = 0x90;
/// BPF JMP operation code: jump if lower than.
pub const BPF_JLT: u8 = 0xa0;
/// BPF JMP operation code: jump if lower or equal.
pub const BPF_JLE: u8 = 0xb0;
/// BPF JMP operation code: jump if lower than (signed).
pub const BPF_JSLT: u8 = 0xc0;
/// BPF JMP operation code: jump if lower or equal (signed).
pub const BPF_JSLE: u8 = 0xd0;

// Op codes
// (Following operation names are not “official”, but may be proper to rbpf; Linux kernel only
// combines above flags and does not attribute a name per operation.)

/// BPF opcode: `lddw dst, imm` /// `dst = imm`. [DEPRECATED]
pub const LD_DW_IMM: u8 = BPF_LD | BPF_IMM | BPF_DW;
/// BPF opcode: `ldxb dst, [src + off]` /// `dst = (src + off) as u8`.
pub const LD_B_REG: u8 = BPF_LDX | BPF_MEM | BPF_B;
/// BPF opcode: `ldxh dst, [src + off]` /// `dst = (src + off) as u16`.
pub const LD_H_REG: u8 = BPF_LDX | BPF_MEM | BPF_H;
/// BPF opcode: `ldxw dst, [src + off]` /// `dst = (src + off) as u32`.
pub const LD_W_REG: u8 = BPF_LDX | BPF_MEM | BPF_W;
/// BPF opcode: `ldxdw dst, [src + off]` /// `dst = (src + off) as u64`.
pub const LD_DW_REG: u8 = BPF_LDX | BPF_MEM | BPF_DW;
/// BPF opcode: `stb [dst + off], imm` /// `(dst + offset) as u8 = imm`.
pub const ST_B_IMM: u8 = BPF_ST | BPF_MEM | BPF_B;
/// BPF opcode: `sth [dst + off], imm` /// `(dst + offset) as u16 = imm`.
pub const ST_H_IMM: u8 = BPF_ST | BPF_MEM | BPF_H;
/// BPF opcode: `stw [dst + off], imm` /// `(dst + offset) as u32 = imm`.
pub const ST_W_IMM: u8 = BPF_ST | BPF_MEM | BPF_W;
/// BPF opcode: `stdw [dst + off], imm` /// `(dst + offset) as u64 = imm`.
pub const ST_DW_IMM: u8 = BPF_ST | BPF_MEM | BPF_DW;
/// BPF opcode: `stxb [dst + off], src` /// `(dst + offset) as u8 = src`.
pub const ST_B_REG: u8 = BPF_STX | BPF_MEM | BPF_B;
/// BPF opcode: `stxh [dst + off], src` /// `(dst + offset) as u16 = src`.
pub const ST_H_REG: u8 = BPF_STX | BPF_MEM | BPF_H;
/// BPF opcode: `stxw [dst + off], src` /// `(dst + offset) as u32 = src`.
pub const ST_W_REG: u8 = BPF_STX | BPF_MEM | BPF_W;
/// BPF opcode: `stxdw [dst + off], src` /// `(dst + offset) as u64 = src`.
pub const ST_DW_REG: u8 = BPF_STX | BPF_MEM | BPF_DW;

/// BPF opcode: `add32 dst, imm` /// `dst += imm`.
pub const ADD32_IMM: u8 = BPF_ALU | BPF_K | BPF_ADD;
/// BPF opcode: `add32 dst, src` /// `dst += src`.
pub const ADD32_REG: u8 = BPF_ALU | BPF_X | BPF_ADD;
/// BPF opcode: `sub32 dst, imm` /// `dst = imm - dst`.
pub const SUB32_IMM: u8 = BPF_ALU | BPF_K | BPF_SUB;
/// BPF opcode: `sub32 dst, src` /// `dst -= src`.
pub const SUB32_REG: u8 = BPF_ALU | BPF_X | BPF_SUB;
/// BPF opcode: `mul32 dst, imm` /// `dst *= imm`.
pub const MUL32_IMM: u8 = BPF_ALU | BPF_K | BPF_MUL;
/// BPF opcode: `mul32 dst, src` /// `dst *= src`.
pub const MUL32_REG: u8 = BPF_ALU | BPF_X | BPF_MUL;
/// BPF opcode: `div32 dst, imm` /// `dst /= imm`.
pub const DIV32_IMM: u8 = BPF_ALU | BPF_K | BPF_DIV;
/// BPF opcode: `div32 dst, src` /// `dst /= src`.
pub const DIV32_REG: u8 = BPF_ALU | BPF_X | BPF_DIV;
/// BPF opcode: `or32 dst, imm` /// `dst |= imm`.
pub const OR32_IMM: u8 = BPF_ALU | BPF_K | BPF_OR;
/// BPF opcode: `or32 dst, src` /// `dst |= src`.
pub const OR32_REG: u8 = BPF_ALU | BPF_X | BPF_OR;
/// BPF opcode: `and32 dst, imm` /// `dst &= imm`.
pub const AND32_IMM: u8 = BPF_ALU | BPF_K | BPF_AND;
/// BPF opcode: `and32 dst, src` /// `dst &= src`.
pub const AND32_REG: u8 = BPF_ALU | BPF_X | BPF_AND;
/// BPF opcode: `lsh32 dst, imm` /// `dst <<= imm`.
pub const LSH32_IMM: u8 = BPF_ALU | BPF_K | BPF_LSH;
/// BPF opcode: `lsh32 dst, src` /// `dst <<= src`.
pub const LSH32_REG: u8 = BPF_ALU | BPF_X | BPF_LSH;
/// BPF opcode: `rsh32 dst, imm` /// `dst >>= imm`.
pub const RSH32_IMM: u8 = BPF_ALU | BPF_K | BPF_RSH;
/// BPF opcode: `rsh32 dst, src` /// `dst >>= src`.
pub const RSH32_REG: u8 = BPF_ALU | BPF_X | BPF_RSH;
/// BPF opcode: `neg32 dst` /// `dst = -dst`.
pub const NEG32: u8 = BPF_ALU | BPF_NEG;
/// BPF opcode: `mod32 dst, imm` /// `dst %= imm`.
pub const MOD32_IMM: u8 = BPF_ALU | BPF_K | BPF_MOD;
/// BPF opcode: `mod32 dst, src` /// `dst %= src`.
pub const MOD32_REG: u8 = BPF_ALU | BPF_X | BPF_MOD;
/// BPF opcode: `xor32 dst, imm` /// `dst ^= imm`.
pub const XOR32_IMM: u8 = BPF_ALU | BPF_K | BPF_XOR;
/// BPF opcode: `xor32 dst, src` /// `dst ^= src`.
pub const XOR32_REG: u8 = BPF_ALU | BPF_X | BPF_XOR;
/// BPF opcode: `mov32 dst, imm` /// `dst = imm`.
pub const MOV32_IMM: u8 = BPF_ALU | BPF_K | BPF_MOV;
/// BPF opcode: `mov32 dst, src` /// `dst = src`.
pub const MOV32_REG: u8 = BPF_ALU | BPF_X | BPF_MOV;
/// BPF opcode: `arsh32 dst, imm` /// `dst >>= imm (arithmetic)`.
pub const ARSH32_IMM: u8 = BPF_ALU | BPF_K | BPF_ARSH;
/// BPF opcode: `arsh32 dst, src` /// `dst >>= src (arithmetic)`.
pub const ARSH32_REG: u8 = BPF_ALU | BPF_X | BPF_ARSH;

/// BPF opcode: `lmul32 dst, imm` /// `dst *= (dst * imm) as u32`.
pub const LMUL32_IMM: u8 = BPF_PQR | BPF_K | BPF_LMUL;
/// BPF opcode: `lmul32 dst, src` /// `dst *= (dst * src) as u32`.
pub const LMUL32_REG: u8 = BPF_PQR | BPF_X | BPF_LMUL;
/// BPF opcode: `uhmul32 dst, imm` /// `dst = (dst * imm) as u64`.
// pub const UHMUL32_IMM: u8 = BPF_PQR | BPF_K | BPF_UHMUL;
/// BPF opcode: `uhmul32 dst, src` /// `dst = (dst * src) as u64`.
// pub const UHMUL32_REG: u8 = BPF_PQR | BPF_X | BPF_UHMUL;
/// BPF opcode: `udiv32 dst, imm` /// `dst /= imm`.
pub const UDIV32_IMM: u8 = BPF_PQR | BPF_K | BPF_UDIV;
/// BPF opcode: `udiv32 dst, src` /// `dst /= src`.
pub const UDIV32_REG: u8 = BPF_PQR | BPF_X | BPF_UDIV;
/// BPF opcode: `urem32 dst, imm` /// `dst %= imm`.
pub const UREM32_IMM: u8 = BPF_PQR | BPF_K | BPF_UREM;
/// BPF opcode: `urem32 dst, src` /// `dst %= src`.
pub const UREM32_REG: u8 = BPF_PQR | BPF_X | BPF_UREM;
/// BPF opcode: `shmul32 dst, imm` /// `dst = (dst * imm) as i64`.
// pub const SHMUL32_IMM: u8 = BPF_PQR | BPF_K | BPF_SHMUL;
/// BPF opcode: `shmul32 dst, src` /// `dst = (dst * src) as i64`.
// pub const SHMUL32_REG: u8 = BPF_PQR | BPF_X | BPF_SHMUL;
/// BPF opcode: `sdiv32 dst, imm` /// `dst /= imm`.
pub const SDIV32_IMM: u8 = BPF_PQR | BPF_K | BPF_SDIV;
/// BPF opcode: `sdiv32 dst, src` /// `dst /= src`.
pub const SDIV32_REG: u8 = BPF_PQR | BPF_X | BPF_SDIV;
/// BPF opcode: `srem32 dst, imm` /// `dst %= imm`.
pub const SREM32_IMM: u8 = BPF_PQR | BPF_K | BPF_SREM;
/// BPF opcode: `srem32 dst, src` /// `dst %= src`.
pub const SREM32_REG: u8 = BPF_PQR | BPF_X | BPF_SREM;

/// BPF opcode: `le dst` /// `dst = htole<imm>(dst), with imm in {16, 32, 64}`.
pub const LE: u8 = BPF_ALU | BPF_K | BPF_END;
/// BPF opcode: `be dst` /// `dst = htobe<imm>(dst), with imm in {16, 32, 64}`.
pub const BE: u8 = BPF_ALU | BPF_X | BPF_END;

/// BPF opcode: `add64 dst, imm` /// `dst += imm`.
pub const ADD64_IMM: u8 = BPF_ALU64 | BPF_K | BPF_ADD;
/// BPF opcode: `add64 dst, src` /// `dst += src`.
pub const ADD64_REG: u8 = BPF_ALU64 | BPF_X | BPF_ADD;
/// BPF opcode: `sub64 dst, imm` /// `dst -= imm`.
pub const SUB64_IMM: u8 = BPF_ALU64 | BPF_K | BPF_SUB;
/// BPF opcode: `sub64 dst, src` /// `dst -= src`.
pub const SUB64_REG: u8 = BPF_ALU64 | BPF_X | BPF_SUB;
/// BPF opcode: `mul64 dst, imm` /// `dst *= imm`.
pub const MUL64_IMM: u8 = BPF_ALU64 | BPF_K | BPF_MUL;
/// BPF opcode: `mul64 dst, src` /// `dst *= src`.
pub const MUL64_REG: u8 = BPF_ALU64 | BPF_X | BPF_MUL;
/// BPF opcode: `div64 dst, imm` /// `dst /= imm`.
pub const DIV64_IMM: u8 = BPF_ALU64 | BPF_K | BPF_DIV;
/// BPF opcode: `div64 dst, src` /// `dst /= src`.
pub const DIV64_REG: u8 = BPF_ALU64 | BPF_X | BPF_DIV;
/// BPF opcode: `or64 dst, imm` /// `dst |= imm`.
pub const OR64_IMM: u8 = BPF_ALU64 | BPF_K | BPF_OR;
/// BPF opcode: `or64 dst, src` /// `dst |= src`.
pub const OR64_REG: u8 = BPF_ALU64 | BPF_X | BPF_OR;
/// BPF opcode: `and64 dst, imm` /// `dst &= imm`.
pub const AND64_IMM: u8 = BPF_ALU64 | BPF_K | BPF_AND;
/// BPF opcode: `and64 dst, src` /// `dst &= src`.
pub const AND64_REG: u8 = BPF_ALU64 | BPF_X | BPF_AND;
/// BPF opcode: `lsh64 dst, imm` /// `dst <<= imm`.
pub const LSH64_IMM: u8 = BPF_ALU64 | BPF_K | BPF_LSH;
/// BPF opcode: `lsh64 dst, src` /// `dst <<= src`.
pub const LSH64_REG: u8 = BPF_ALU64 | BPF_X | BPF_LSH;
/// BPF opcode: `rsh64 dst, imm` /// `dst >>= imm`.
pub const RSH64_IMM: u8 = BPF_ALU64 | BPF_K | BPF_RSH;
/// BPF opcode: `rsh64 dst, src` /// `dst >>= src`.
pub const RSH64_REG: u8 = BPF_ALU64 | BPF_X | BPF_RSH;
/// BPF opcode: `neg64 dst` /// `dst = -dst`.
pub const NEG64: u8 = BPF_ALU64 | BPF_NEG;
/// BPF opcode: `mod64 dst, imm` /// `dst %= imm`.
pub const MOD64_IMM: u8 = BPF_ALU64 | BPF_K | BPF_MOD;
/// BPF opcode: `mod64 dst, src` /// `dst %= src`.
pub const MOD64_REG: u8 = BPF_ALU64 | BPF_X | BPF_MOD;
/// BPF opcode: `xor64 dst, imm` /// `dst ^= imm`.
pub const XOR64_IMM: u8 = BPF_ALU64 | BPF_K | BPF_XOR;
/// BPF opcode: `xor64 dst, src` /// `dst ^= src`.
pub const XOR64_REG: u8 = BPF_ALU64 | BPF_X | BPF_XOR;
/// BPF opcode: `mov64 dst, imm` /// `dst = imm`.
pub const MOV64_IMM: u8 = BPF_ALU64 | BPF_K | BPF_MOV;
/// BPF opcode: `mov64 dst, src` /// `dst = src`.
pub const MOV64_REG: u8 = BPF_ALU64 | BPF_X | BPF_MOV;
/// BPF opcode: `arsh64 dst, imm` /// `dst >>= imm (arithmetic)`.
pub const ARSH64_IMM: u8 = BPF_ALU64 | BPF_K | BPF_ARSH;
/// BPF opcode: `arsh64 dst, src` /// `dst >>= src (arithmetic)`.
pub const ARSH64_REG: u8 = BPF_ALU64 | BPF_X | BPF_ARSH;
/// BPF opcode: `hor64 dst, imm` /// `dst |= imm << 32`.
pub const HOR64_IMM: u8 = BPF_ALU64 | BPF_K | BPF_HOR;

/// BPF opcode: `lmul64 dst, imm` /// `dst = (dst * imm) as u64`.
pub const LMUL64_IMM: u8 = BPF_PQR | BPF_B | BPF_K | BPF_LMUL;
/// BPF opcode: `lmul64 dst, src` /// `dst = (dst * src) as u64`.
pub const LMUL64_REG: u8 = BPF_PQR | BPF_B | BPF_X | BPF_LMUL;
/// BPF opcode: `uhmul64 dst, imm` /// `dst = (dst * imm) >> 64`.
pub const UHMUL64_IMM: u8 = BPF_PQR | BPF_B | BPF_K | BPF_UHMUL;
/// BPF opcode: `uhmul64 dst, src` /// `dst = (dst * src) >> 64`.
pub const UHMUL64_REG: u8 = BPF_PQR | BPF_B | BPF_X | BPF_UHMUL;
/// BPF opcode: `udiv64 dst, imm` /// `dst /= imm`.
pub const UDIV64_IMM: u8 = BPF_PQR | BPF_B | BPF_K | BPF_UDIV;
/// BPF opcode: `udiv64 dst, src` /// `dst /= src`.
pub const UDIV64_REG: u8 = BPF_PQR | BPF_B | BPF_X | BPF_UDIV;
/// BPF opcode: `urem64 dst, imm` /// `dst %= imm`.
pub const UREM64_IMM: u8 = BPF_PQR | BPF_B | BPF_K | BPF_UREM;
/// BPF opcode: `urem64 dst, src` /// `dst %= src`.
pub const UREM64_REG: u8 = BPF_PQR | BPF_B | BPF_X | BPF_UREM;
/// BPF opcode: `shmul64 dst, imm` /// `dst = (dst * imm) >> 64`.
pub const SHMUL64_IMM: u8 = BPF_PQR | BPF_B | BPF_K | BPF_SHMUL;
/// BPF opcode: `shmul64 dst, src` /// `dst = (dst * src) >> 64`.
pub const SHMUL64_REG: u8 = BPF_PQR | BPF_B | BPF_X | BPF_SHMUL;
/// BPF opcode: `sdiv64 dst, imm` /// `dst /= imm`.
pub const SDIV64_IMM: u8 = BPF_PQR | BPF_B | BPF_K | BPF_SDIV;
/// BPF opcode: `sdiv64 dst, src` /// `dst /= src`.
pub const SDIV64_REG: u8 = BPF_PQR | BPF_B | BPF_X | BPF_SDIV;
/// BPF opcode: `srem64 dst, imm` /// `dst %= imm`.
pub const SREM64_IMM: u8 = BPF_PQR | BPF_B | BPF_K | BPF_SREM;
/// BPF opcode: `srem64 dst, src` /// `dst %= src`.
pub const SREM64_REG: u8 = BPF_PQR | BPF_B | BPF_X | BPF_SREM;

/// BPF opcode: `ja +off` /// `PC += off`.
pub const JA: u8 = BPF_JMP | BPF_JA;
/// BPF opcode: `jeq dst, imm, +off` /// `PC += off if dst == imm`.
pub const JEQ_IMM: u8 = BPF_JMP | BPF_K | BPF_JEQ;
/// BPF opcode: `jeq dst, src, +off` /// `PC += off if dst == src`.
pub const JEQ_REG: u8 = BPF_JMP | BPF_X | BPF_JEQ;
/// BPF opcode: `jgt dst, imm, +off` /// `PC += off if dst > imm`.
pub const JGT_IMM: u8 = BPF_JMP | BPF_K | BPF_JGT;
/// BPF opcode: `jgt dst, src, +off` /// `PC += off if dst > src`.
pub const JGT_REG: u8 = BPF_JMP | BPF_X | BPF_JGT;
/// BPF opcode: `jge dst, imm, +off` /// `PC += off if dst >= imm`.
pub const JGE_IMM: u8 = BPF_JMP | BPF_K | BPF_JGE;
/// BPF opcode: `jge dst, src, +off` /// `PC += off if dst >= src`.
pub const JGE_REG: u8 = BPF_JMP | BPF_X | BPF_JGE;
/// BPF opcode: `jlt dst, imm, +off` /// `PC += off if dst < imm`.
pub const JLT_IMM: u8 = BPF_JMP | BPF_K | BPF_JLT;
/// BPF opcode: `jlt dst, src, +off` /// `PC += off if dst < src`.
pub const JLT_REG: u8 = BPF_JMP | BPF_X | BPF_JLT;
/// BPF opcode: `jle dst, imm, +off` /// `PC += off if dst <= imm`.
pub const JLE_IMM: u8 = BPF_JMP | BPF_K | BPF_JLE;
/// BPF opcode: `jle dst, src, +off` /// `PC += off if dst <= src`.
pub const JLE_REG: u8 = BPF_JMP | BPF_X | BPF_JLE;
/// BPF opcode: `jset dst, imm, +off` /// `PC += off if dst & imm`.
pub const JSET_IMM: u8 = BPF_JMP | BPF_K | BPF_JSET;
/// BPF opcode: `jset dst, src, +off` /// `PC += off if dst & src`.
pub const JSET_REG: u8 = BPF_JMP | BPF_X | BPF_JSET;
/// BPF opcode: `jne dst, imm, +off` /// `PC += off if dst != imm`.
pub const JNE_IMM: u8 = BPF_JMP | BPF_K | BPF_JNE;
/// BPF opcode: `jne dst, src, +off` /// `PC += off if dst != src`.
pub const JNE_REG: u8 = BPF_JMP | BPF_X | BPF_JNE;
/// BPF opcode: `jsgt dst, imm, +off` /// `PC += off if dst > imm (signed)`.
pub const JSGT_IMM: u8 = BPF_JMP | BPF_K | BPF_JSGT;
/// BPF opcode: `jsgt dst, src, +off` /// `PC += off if dst > src (signed)`.
pub const JSGT_REG: u8 = BPF_JMP | BPF_X | BPF_JSGT;
/// BPF opcode: `jsge dst, imm, +off` /// `PC += off if dst >= imm (signed)`.
pub const JSGE_IMM: u8 = BPF_JMP | BPF_K | BPF_JSGE;
/// BPF opcode: `jsge dst, src, +off` /// `PC += off if dst >= src (signed)`.
pub const JSGE_REG: u8 = BPF_JMP | BPF_X | BPF_JSGE;
/// BPF opcode: `jslt dst, imm, +off` /// `PC += off if dst < imm (signed)`.
pub const JSLT_IMM: u8 = BPF_JMP | BPF_K | BPF_JSLT;
/// BPF opcode: `jslt dst, src, +off` /// `PC += off if dst < src (signed)`.
pub const JSLT_REG: u8 = BPF_JMP | BPF_X | BPF_JSLT;
/// BPF opcode: `jsle dst, imm, +off` /// `PC += off if dst <= imm (signed)`.
pub const JSLE_IMM: u8 = BPF_JMP | BPF_K | BPF_JSLE;
/// BPF opcode: `jsle dst, src, +off` /// `PC += off if dst <= src (signed)`.
pub const JSLE_REG: u8 = BPF_JMP | BPF_X | BPF_JSLE;

/// BPF opcode: `call imm` /// syscall function call to syscall with key `imm`.
pub const CALL_IMM: u8 = BPF_JMP | BPF_CALL;
/// BPF opcode: tail call.
pub const CALL_REG: u8 = BPF_JMP | BPF_X | BPF_CALL;
/// BPF opcode: `exit` /// `return r0`.
pub const EXIT: u8 = BPF_JMP | BPF_EXIT;

// Used in JIT
/// Mask to extract the operation class from an operation code.
pub const BPF_CLS_MASK: u8 = 0x07;
/// Mask to extract the arithmetic operation code from an instruction operation code.
pub const BPF_ALU_OP_MASK: u8 = 0xf0;

/// An eBPF instruction.
///
/// See <https://www.kernel.org/doc/Documentation/networking/filter.txt> for the Linux kernel
/// documentation about eBPF, or <https://github.com/iovisor/bpf-docs/blob/master/eBPF.md> for a
/// more concise version.
#[derive(PartialEq, Eq, Clone, Default)]
pub struct Insn {
    /// Instruction pointer.
    pub ptr: usize,
    /// Operation code.
    pub opc: u8,
    /// Destination register operand.
    pub dst: u8,
    /// Source register operand.
    pub src: u8,
    /// Offset operand.
    pub off: i16,
    /// Immediate value operand.
    pub imm: i64,
}

impl fmt::Debug for Insn {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Insn {{ ptr: 0x{:08x?}, opc: 0x{:02x?}, dst: {}, src: {}, off: 0x{:04x?}, imm: 0x{:08x?} }}",
            self.ptr, self.opc, self.dst, self.src, self.off, self.imm
        )
    }
}

impl Insn {
    /// Turn an `Insn` back into an array of bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use solana_rbpf::ebpf;
    ///
    /// let prog: &[u8] = &[
    ///     0xb7, 0x12, 0x56, 0x34, 0xde, 0xbc, 0x9a, 0x78,
    ///     ];
    /// let insn = ebpf::Insn {
    ///     ptr: 0x00,
    ///     opc: 0xb7,
    ///     dst: 2,
    ///     src: 1,
    ///     off: 0x3456,
    ///     imm: 0x789abcde
    /// };
    /// assert_eq!(insn.to_array(), prog);
    /// ```
    pub fn to_array(&self) -> [u8; INSN_SIZE] {
        [
            self.opc,
            self.src.wrapping_shl(4) | self.dst,
            (self.off & 0xff) as u8,
            self.off.wrapping_shr(8) as u8,
            (self.imm & 0xff) as u8,
            (self.imm & 0xff_00).wrapping_shr(8) as u8,
            (self.imm as u32 & 0xff_00_00).wrapping_shr(16) as u8,
            (self.imm as u32 & 0xff_00_00_00).wrapping_shr(24) as u8,
        ]
    }

    /// Turn an `Insn` into an vector of bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use solana_rbpf::ebpf;
    ///
    /// let prog: Vec<u8> = vec![
    ///     0xb7, 0x12, 0x56, 0x34, 0xde, 0xbc, 0x9a, 0x78,
    ///     ];
    /// let insn = ebpf::Insn {
    ///     ptr: 0x00,
    ///     opc: 0xb7,
    ///     dst: 2,
    ///     src: 1,
    ///     off: 0x3456,
    ///     imm: 0x789abcde
    /// };
    /// assert_eq!(insn.to_vec(), prog);
    /// ```
    pub fn to_vec(&self) -> Vec<u8> {
        self.to_array().to_vec()
    }
}

/// Get the instruction at `idx` of an eBPF program. `idx` is the index (number) of the
/// instruction (not a byte offset). The first instruction has index 0.
///
/// # Panics
///
/// Panics if it is not possible to get the instruction (if idx is too high, or last instruction is
/// incomplete).
///
/// # Examples
///
/// ```
/// use solana_rbpf::ebpf;
///
/// let prog = &[
///     0xb7, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
///     0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
///     ];
/// let insn = ebpf::get_insn(prog, 1);
/// assert_eq!(insn.opc, 0x95);
/// ```
///
/// The example below will panic, since the last instruction is not complete and cannot be loaded.
///
/// ```rust,should_panic
/// use solana_rbpf::ebpf;
///
/// let prog = &[
///     0xb7, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
///     0x95, 0x00, 0x00, 0x00, 0x00, 0x00              // two bytes missing
///     ];
/// let insn = ebpf::get_insn(prog, 1);
/// ```
pub fn get_insn(prog: &[u8], pc: usize) -> Insn {
    // This guard should not be needed in most cases, since the verifier already checks the program
    // size, and indexes should be fine in the interpreter/JIT. But this function is publicly
    // available and user can call it with any `pc`, so we have to check anyway.
    debug_assert!(
        (pc + 1) * INSN_SIZE <= prog.len(),
        "cannot reach instruction at index {:?} in program containing {:?} bytes",
        pc,
        prog.len()
    );
    get_insn_unchecked(prog, pc)
}
/// Same as `get_insn` except not checked
pub fn get_insn_unchecked(prog: &[u8], pc: usize) -> Insn {
    Insn {
        ptr: pc,
        opc: prog[INSN_SIZE * pc],
        dst: prog[INSN_SIZE * pc + 1] & 0x0f,
        src: (prog[INSN_SIZE * pc + 1] & 0xf0) >> 4,
        off: LittleEndian::read_i16(&prog[(INSN_SIZE * pc + 2)..]),
        imm: LittleEndian::read_i32(&prog[(INSN_SIZE * pc + 4)..]) as i64,
    }
}

/// Merge the two halves of a LD_DW_IMM instruction
pub fn augment_lddw_unchecked(prog: &[u8], insn: &mut Insn) {
    let more_significant_half = LittleEndian::read_i32(&prog[((insn.ptr + 1) * INSN_SIZE + 4)..]);
    insn.imm = ((insn.imm as u64 & 0xffffffff) | ((more_significant_half as u64) << 32)) as i64;
}

/// Hash a symbol name
///
/// This function is used by both the relocator and the VM to translate symbol names
/// into a 32 bit id used to identify a syscall function.  The 32 bit id is used in the
/// eBPF `call` instruction's imm field.
pub fn hash_symbol_name(name: &[u8]) -> u32 {
    let mut hasher = Murmur3Hasher::default();
    Hash::hash_slice(name, &mut hasher);
    hasher.finish()
}
