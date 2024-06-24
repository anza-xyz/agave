#![allow(clippy::arithmetic_side_effects)]
// Copyright 2017 Alex Dukhno <alex.dukhno@icloud.com>
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Module provides API to create eBPF programs by Rust programming language

use crate::ebpf::*;

/// Represents single eBPF instruction
pub trait Instruction: Sized {
    /// returns instruction opt code
    fn opt_code_byte(&self) -> u8;

    /// returns destination register
    fn get_dst(&self) -> u8 {
        self.get_insn().dst
    }

    /// returns source register
    fn get_src(&self) -> u8 {
        self.get_insn().src
    }

    /// returns offset bytes
    fn get_off(&self) -> i16 {
        self.get_insn().off
    }

    /// returns immediate value
    fn get_imm(&self) -> i64 {
        self.get_insn().imm
    }

    /// sets destination register
    #[must_use]
    fn set_dst(mut self, dst: u8) -> Self {
        self.get_insn_mut().dst = dst;
        self
    }

    /// sets source register
    #[must_use]
    fn set_src(mut self, src: u8) -> Self {
        self.get_insn_mut().src = src;
        self
    }

    /// sets offset bytes
    #[must_use]
    fn set_off(mut self, offset: i16) -> Self {
        self.get_insn_mut().off = offset;
        self
    }

    /// sets immediate value
    #[must_use]
    fn set_imm(mut self, imm: i64) -> Self {
        self.get_insn_mut().imm = imm;
        self
    }

    /// get `ebpf::Insn` struct
    fn get_insn(&self) -> &Insn;

    /// get mutable `ebpf::Insn` struct
    fn get_insn_mut(&mut self) -> &mut Insn;
}

/// General trait for `Instruction`s and `BpfCode`.
/// Provides functionality to transform `struct` into collection of bytes
pub trait IntoBytes {
    /// type of targeted transformation
    type Bytes;

    /// consume `Self` with transformation into `Self::Bytes`
    fn into_bytes(self) -> Self::Bytes;
}

/// General implementation of `IntoBytes` for `Instruction`
impl<'i, I: Instruction> IntoBytes for &'i I {
    type Bytes = Vec<u8>;

    /// transform immutable reference of `Instruction` into `Vec<u8>` with size of 8
    /// [ 1 byte ,      1 byte      , 2 bytes,  4 bytes  ]
    /// [ OP_CODE, SRC_REG | DST_REG, OFFSET , IMMEDIATE ]
    fn into_bytes(self) -> Self::Bytes {
        vec![
            self.opt_code_byte(),
            self.get_src() << 4 | self.get_dst(),
            self.get_off() as u8,
            (self.get_off() >> 8) as u8,
            self.get_imm() as u8,
            (self.get_imm() >> 8) as u8,
            (self.get_imm() >> 16) as u8,
            (self.get_imm() >> 24) as u8,
        ]
    }
}

/// BPF instruction stack in byte representation
#[derive(Default)]
pub struct BpfCode {
    instructions: Vec<u8>,
}

impl BpfCode {
    /// creates new empty BPF instruction stack
    pub fn new() -> Self {
        BpfCode {
            instructions: vec![],
        }
    }

    /// create ADD instruction
    pub fn add(&mut self, source: Source, arch: Arch) -> Move {
        self.mov_internal(source, arch, OpBits::Add)
    }

    /// create SUB instruction
    pub fn sub(&mut self, source: Source, arch: Arch) -> Move {
        self.mov_internal(source, arch, OpBits::Sub)
    }

    /// create MUL instruction
    pub fn mul(&mut self, source: Source, arch: Arch) -> Move {
        self.mov_internal(source, arch, OpBits::Mul)
    }

    /// create DIV instruction
    pub fn div(&mut self, source: Source, arch: Arch) -> Move {
        self.mov_internal(source, arch, OpBits::Div)
    }

    /// create OR instruction
    pub fn bit_or(&mut self, source: Source, arch: Arch) -> Move {
        self.mov_internal(source, arch, OpBits::BitOr)
    }

    /// create AND instruction
    pub fn bit_and(&mut self, source: Source, arch: Arch) -> Move {
        self.mov_internal(source, arch, OpBits::BitAnd)
    }

    /// create LSHIFT instruction
    pub fn left_shift(&mut self, source: Source, arch: Arch) -> Move {
        self.mov_internal(source, arch, OpBits::LShift)
    }

    /// create RSHIFT instruction
    pub fn right_shift(&mut self, source: Source, arch: Arch) -> Move {
        self.mov_internal(source, arch, OpBits::RShift)
    }

    /// create NEGATE instruction
    pub fn negate(&mut self, arch: Arch) -> Move {
        self.mov_internal(Source::Imm, arch, OpBits::Negate)
    }

    /// create MOD instruction
    pub fn modulo(&mut self, source: Source, arch: Arch) -> Move {
        self.mov_internal(source, arch, OpBits::Mod)
    }

    /// create XOR instruction
    pub fn bit_xor(&mut self, source: Source, arch: Arch) -> Move {
        self.mov_internal(source, arch, OpBits::BitXor)
    }

    /// create MOV instruction
    pub fn mov(&mut self, source: Source, arch: Arch) -> Move {
        self.mov_internal(source, arch, OpBits::Mov)
    }

    /// create SIGNED RSHIFT instruction
    pub fn signed_right_shift(&mut self, source: Source, arch: Arch) -> Move {
        self.mov_internal(source, arch, OpBits::SignRShift)
    }

    #[inline]
    fn mov_internal(&mut self, source: Source, arch_bits: Arch, op_bits: OpBits) -> Move {
        Move {
            bpf_code: self,
            src_bit: source,
            op_bits,
            arch_bits,
            insn: Insn::default(),
        }
    }

    /// create byte swap instruction
    pub fn swap_bytes(&mut self, endian: Endian) -> SwapBytes {
        SwapBytes {
            bpf_code: self,
            endian,
            insn: Insn::default(),
        }
    }

    /// create LOAD instruction, IMMEDIATE is the source
    pub fn load(&mut self, mem_size: MemSize) -> Load {
        self.load_internal(mem_size, Addressing::Imm, BPF_LD)
    }

    /// create ABSOLUTE LOAD instruction
    pub fn load_abs(&mut self, mem_size: MemSize) -> Load {
        self.load_internal(mem_size, Addressing::Abs, BPF_LD)
    }

    /// create INDIRECT LOAD instruction
    pub fn load_ind(&mut self, mem_size: MemSize) -> Load {
        self.load_internal(mem_size, Addressing::Ind, BPF_LD)
    }

    /// create LOAD instruction, MEMORY is the source
    pub fn load_x(&mut self, mem_size: MemSize) -> Load {
        self.load_internal(mem_size, Addressing::Mem, BPF_LDX)
    }

    #[inline]
    fn load_internal(&mut self, mem_size: MemSize, addressing: Addressing, source: u8) -> Load {
        Load {
            bpf_code: self,
            addressing,
            mem_size,
            source,
            insn: Insn::default(),
        }
    }

    /// creates STORE instruction, IMMEDIATE is the source
    pub fn store(&mut self, mem_size: MemSize) -> Store {
        self.store_internal(mem_size, BPF_IMM)
    }

    /// creates STORE instruction, MEMORY is the source
    pub fn store_x(&mut self, mem_size: MemSize) -> Store {
        self.store_internal(mem_size, BPF_MEM | BPF_STX)
    }

    #[inline]
    fn store_internal(&mut self, mem_size: MemSize, source: u8) -> Store {
        Store {
            bpf_code: self,
            mem_size,
            source,
            insn: Insn::default(),
        }
    }

    /// create unconditional JMP instruction
    pub fn jump_unconditional(&mut self) -> Jump {
        self.jump_conditional(Cond::Abs, Source::Imm)
    }

    /// create conditional JMP instruction
    pub fn jump_conditional(&mut self, cond: Cond, src_bit: Source) -> Jump {
        Jump {
            bpf_code: self,
            cond,
            src_bit,
            insn: Insn::default(),
        }
    }

    /// create CALL instruction
    pub fn call(&mut self) -> FunctionCall {
        FunctionCall {
            bpf_code: self,
            insn: Insn::default(),
        }
    }

    /// create EXIT instruction
    pub fn exit(&mut self) -> Exit {
        Exit {
            bpf_code: self,
            insn: Insn::default(),
        }
    }
}

/// Transform `BpfCode` into assemble representation
impl<'a> IntoBytes for &'a BpfCode {
    type Bytes = &'a [u8];

    /// returns `BpfCode` instruction stack as `&[u8]`
    fn into_bytes(self) -> Self::Bytes {
        self.instructions.as_slice()
    }
}

/// struct to represent `MOV ALU` instructions
pub struct Move<'i> {
    bpf_code: &'i mut BpfCode,
    src_bit: Source,
    op_bits: OpBits,
    arch_bits: Arch,
    insn: Insn,
}

impl<'i> Move<'i> {
    /// push MOV instruction into BpfCode instruction stack
    pub fn push(self) -> &'i mut BpfCode {
        let mut asm = self.into_bytes();
        self.bpf_code.instructions.append(&mut asm);
        self.bpf_code
    }
}

impl<'i> Instruction for Move<'i> {
    fn opt_code_byte(&self) -> u8 {
        let op_bits = self.op_bits as u8;
        let src_bit = self.src_bit as u8;
        let arch_bits = self.arch_bits as u8;
        op_bits | src_bit | arch_bits
    }

    fn get_insn_mut(&mut self) -> &mut Insn {
        &mut self.insn
    }

    fn get_insn(&self) -> &Insn {
        &self.insn
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "fuzzer-not-safe-for-production",
    derive(arbitrary::Arbitrary, Debug)
)]
/// The source of ALU and JMP instructions
pub enum Source {
    /// immediate field will be used as a source
    Imm = BPF_IMM as isize,
    /// src register will be used as a source
    Reg = BPF_X as isize,
}

#[derive(Copy, Clone)]
enum OpBits {
    Add = BPF_ADD as isize,
    Sub = BPF_SUB as isize,
    Mul = BPF_MUL as isize,
    Div = BPF_DIV as isize,
    BitOr = BPF_OR as isize,
    BitAnd = BPF_AND as isize,
    LShift = BPF_LSH as isize,
    RShift = BPF_RSH as isize,
    Negate = BPF_NEG as isize,
    Mod = BPF_MOD as isize,
    BitXor = BPF_XOR as isize,
    Mov = BPF_MOV as isize,
    SignRShift = BPF_ARSH as isize,
}

#[derive(Copy, Clone)]
#[cfg_attr(
    feature = "fuzzer-not-safe-for-production",
    derive(arbitrary::Arbitrary, Debug, PartialEq, Eq)
)]
/// Architecture of instructions
pub enum Arch {
    /// 64-bit instructions
    X64 = BPF_ALU64 as isize,
    /// 32-bit instructions
    X32 = BPF_ALU as isize,
}

/// struct representation of byte swap operation
pub struct SwapBytes<'i> {
    bpf_code: &'i mut BpfCode,
    endian: Endian,
    insn: Insn,
}

impl<'i> SwapBytes<'i> {
    /// push bytes swap instruction into BpfCode instruction stack
    pub fn push(self) -> &'i mut BpfCode {
        let mut asm = self.into_bytes();
        self.bpf_code.instructions.append(&mut asm);
        self.bpf_code
    }
}

impl<'i> Instruction for SwapBytes<'i> {
    fn opt_code_byte(&self) -> u8 {
        self.endian as u8
    }

    fn get_insn_mut(&mut self) -> &mut Insn {
        &mut self.insn
    }

    fn get_insn(&self) -> &Insn {
        &self.insn
    }
}

#[derive(Copy, Clone)]
#[cfg_attr(
    feature = "fuzzer-not-safe-for-production",
    derive(arbitrary::Arbitrary, Debug, PartialEq, Eq)
)]
/// Bytes endian
pub enum Endian {
    /// Little endian
    Little = LE as isize,
    /// Big endian
    Big = BE as isize,
}

/// struct representation of LOAD instructions
pub struct Load<'i> {
    bpf_code: &'i mut BpfCode,
    addressing: Addressing,
    mem_size: MemSize,
    source: u8,
    insn: Insn,
}

impl<'i> Load<'i> {
    /// push LOAD instruction into BpfCode instruction stack
    pub fn push(self) -> &'i mut BpfCode {
        let mut asm = self.into_bytes();
        self.bpf_code.instructions.append(&mut asm);
        self.bpf_code
    }
}

impl<'i> Instruction for Load<'i> {
    fn opt_code_byte(&self) -> u8 {
        let size = self.mem_size as u8;
        let addressing = self.addressing as u8;
        addressing | size | self.source
    }

    fn get_insn_mut(&mut self) -> &mut Insn {
        &mut self.insn
    }

    fn get_insn(&self) -> &Insn {
        &self.insn
    }
}

/// struct representation of STORE instructions
pub struct Store<'i> {
    bpf_code: &'i mut BpfCode,
    mem_size: MemSize,
    source: u8,
    insn: Insn,
}

impl<'i> Store<'i> {
    /// push STORE instruction into BpfCode instruction stack
    pub fn push(self) -> &'i mut BpfCode {
        let mut asm = self.into_bytes();
        self.bpf_code.instructions.append(&mut asm);
        self.bpf_code
    }
}

impl<'i> Instruction for Store<'i> {
    fn opt_code_byte(&self) -> u8 {
        let size = self.mem_size as u8;
        BPF_MEM | BPF_ST | size | self.source
    }

    fn get_insn_mut(&mut self) -> &mut Insn {
        &mut self.insn
    }

    fn get_insn(&self) -> &Insn {
        &self.insn
    }
}

#[derive(Copy, Clone)]
#[cfg_attr(
    feature = "fuzzer-not-safe-for-production",
    derive(arbitrary::Arbitrary, Debug, PartialEq, Eq)
)]
/// Memory size for LOAD and STORE instructions
pub enum MemSize {
    /// 8-bit size
    Byte = BPF_B as isize,
    /// 16-bit size
    HalfWord = BPF_H as isize,
    /// 32-bit size
    Word = BPF_W as isize,
    /// 64-bit size
    DoubleWord = BPF_DW as isize,
}

#[derive(Copy, Clone)]
enum Addressing {
    Imm = BPF_IMM as isize,
    Abs = BPF_ABS as isize,
    Ind = BPF_IND as isize,
    Mem = BPF_MEM as isize,
}

/// struct representation of JMP instructions
pub struct Jump<'i> {
    bpf_code: &'i mut BpfCode,
    cond: Cond,
    src_bit: Source,
    insn: Insn,
}

impl<'i> Jump<'i> {
    /// push JMP instruction into BpfCode instruction stack
    pub fn push(self) -> &'i mut BpfCode {
        let mut asm = self.into_bytes();
        self.bpf_code.instructions.append(&mut asm);
        self.bpf_code
    }
}

impl<'i> Instruction for Jump<'i> {
    fn opt_code_byte(&self) -> u8 {
        let cmp: u8 = self.cond as u8;
        let src_bit = self.src_bit as u8;
        cmp | src_bit | BPF_JMP
    }

    fn get_insn_mut(&mut self) -> &mut Insn {
        &mut self.insn
    }

    fn get_insn(&self) -> &Insn {
        &self.insn
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "fuzzer-not-safe-for-production",
    derive(arbitrary::Arbitrary, Debug)
)]
/// Conditions for JMP instructions
pub enum Cond {
    /// Absolute or unconditional
    Abs = BPF_JA as isize,
    /// Jump if `==`
    Equals = BPF_JEQ as isize,
    /// Jump if `>`
    Greater = BPF_JGT as isize,
    /// Jump if `>=`
    GreaterEquals = BPF_JGE as isize,
    /// Jump if `<`
    Lower = BPF_JLT as isize,
    /// Jump if `<=`
    LowerEquals = BPF_JLE as isize,
    /// Jump if `src` & `dst`
    BitAnd = BPF_JSET as isize,
    /// Jump if `!=`
    NotEquals = BPF_JNE as isize,
    /// Jump if `>` (signed)
    GreaterSigned = BPF_JSGT as isize,
    /// Jump if `>=` (signed)
    GreaterEqualsSigned = BPF_JSGE as isize,
    /// Jump if `<` (signed)
    LowerSigned = BPF_JSLT as isize,
    /// Jump if `<=` (signed)
    LowerEqualsSigned = BPF_JSLE as isize,
}

/// struct representation of CALL instruction
pub struct FunctionCall<'i> {
    bpf_code: &'i mut BpfCode,
    insn: Insn,
}

impl<'i> FunctionCall<'i> {
    /// push CALL instruction into BpfCode instruction stack
    pub fn push(self) -> &'i mut BpfCode {
        let mut asm = self.into_bytes();
        self.bpf_code.instructions.append(&mut asm);
        self.bpf_code
    }
}

impl<'i> Instruction for FunctionCall<'i> {
    fn opt_code_byte(&self) -> u8 {
        BPF_CALL | BPF_JMP
    }

    fn get_insn_mut(&mut self) -> &mut Insn {
        &mut self.insn
    }

    fn get_insn(&self) -> &Insn {
        &self.insn
    }
}

/// struct representation of EXIT instruction
pub struct Exit<'i> {
    bpf_code: &'i mut BpfCode,
    insn: Insn,
}

impl<'i> Exit<'i> {
    /// push EXIT instruction into BpfCode instruction stack
    pub fn push(self) -> &'i mut BpfCode {
        let mut asm = self.into_bytes();
        self.bpf_code.instructions.append(&mut asm);
        self.bpf_code
    }
}

impl<'i> Instruction for Exit<'i> {
    fn opt_code_byte(&self) -> u8 {
        BPF_EXIT | BPF_JMP
    }

    fn get_insn_mut(&mut self) -> &mut Insn {
        &mut self.insn
    }

    fn get_insn(&self) -> &Insn {
        &self.insn
    }
}

#[cfg(test)]
mod tests {
    #[cfg(test)]
    mod special {
        use super::super::*;

        #[test]
        fn call_immediate() {
            let mut program = BpfCode::new();
            program.call().set_imm(0x11_22_33_44).push();

            assert_eq!(
                program.into_bytes(),
                &[0x85, 0x00, 0x00, 0x00, 0x44, 0x33, 0x22, 0x11]
            );
        }

        #[test]
        fn exit_operation() {
            let mut program = BpfCode::new();
            program.exit().push();

            assert_eq!(
                program.into_bytes(),
                &[0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
            );
        }
    }

    #[cfg(test)]
    mod jump_instructions {
        #[cfg(test)]
        mod register {
            use super::super::super::*;

            #[test]
            fn jump_on_dst_equals_src() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::Equals, Source::Reg)
                    .set_dst(0x01)
                    .set_src(0x02)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x1d, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_greater_than_src() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::Greater, Source::Reg)
                    .set_dst(0x03)
                    .set_src(0x02)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x2d, 0x23, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_greater_or_equals_to_src() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::GreaterEquals, Source::Reg)
                    .set_dst(0x04)
                    .set_src(0x01)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x3d, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_lower_than_src() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::Lower, Source::Reg)
                    .set_dst(0x03)
                    .set_src(0x02)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0xad, 0x23, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_lower_or_equals_to_src() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::LowerEquals, Source::Reg)
                    .set_dst(0x04)
                    .set_src(0x01)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0xbd, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_bit_and_with_src_not_equal_zero() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::BitAnd, Source::Reg)
                    .set_dst(0x05)
                    .set_src(0x02)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x4d, 0x25, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_not_equals_src() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::NotEquals, Source::Reg)
                    .set_dst(0x03)
                    .set_src(0x05)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x5d, 0x53, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_greater_than_src_signed() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::GreaterSigned, Source::Reg)
                    .set_dst(0x04)
                    .set_src(0x01)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x6d, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_greater_or_equals_src_signed() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::GreaterEqualsSigned, Source::Reg)
                    .set_dst(0x01)
                    .set_src(0x03)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x7d, 0x31, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_lower_than_src_signed() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::LowerSigned, Source::Reg)
                    .set_dst(0x04)
                    .set_src(0x01)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0xcd, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_lower_or_equals_src_signed() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::LowerEqualsSigned, Source::Reg)
                    .set_dst(0x01)
                    .set_src(0x03)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0xdd, 0x31, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }
        }

        #[cfg(test)]
        mod immediate {
            use super::super::super::*;

            #[test]
            fn jump_to_label() {
                let mut program = BpfCode::new();
                program.jump_unconditional().set_off(0x00_11).push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x05, 0x00, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_equals_const() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::Equals, Source::Imm)
                    .set_dst(0x01)
                    .set_imm(0x00_11_22_33)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x15, 0x01, 0x00, 0x00, 0x33, 0x22, 0x11, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_greater_than_const() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::Greater, Source::Imm)
                    .set_dst(0x02)
                    .set_imm(0x00_11_00_11)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x25, 0x02, 0x00, 0x00, 0x11, 0x00, 0x11, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_greater_or_equals_to_const() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::GreaterEquals, Source::Imm)
                    .set_dst(0x04)
                    .set_imm(0x00_22_11_00)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x35, 0x04, 0x00, 0x00, 0x00, 0x11, 0x22, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_lower_than_const() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::Lower, Source::Imm)
                    .set_dst(0x02)
                    .set_imm(0x00_11_00_11)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0xa5, 0x02, 0x00, 0x00, 0x11, 0x00, 0x11, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_lower_or_equals_to_const() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::LowerEquals, Source::Imm)
                    .set_dst(0x04)
                    .set_imm(0x00_22_11_00)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0xb5, 0x04, 0x00, 0x00, 0x00, 0x11, 0x22, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_bit_and_with_const_not_equal_zero() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::BitAnd, Source::Imm)
                    .set_dst(0x05)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x45, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_not_equals_const() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::NotEquals, Source::Imm)
                    .set_dst(0x03)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x55, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_greater_than_const_signed() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::GreaterSigned, Source::Imm)
                    .set_dst(0x04)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x65, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_greater_or_equals_src_signed() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::GreaterEqualsSigned, Source::Imm)
                    .set_dst(0x01)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x75, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_lower_than_const_signed() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::LowerSigned, Source::Imm)
                    .set_dst(0x04)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0xc5, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn jump_on_dst_lower_or_equals_src_signed() {
                let mut program = BpfCode::new();
                program
                    .jump_conditional(Cond::LowerEqualsSigned, Source::Imm)
                    .set_dst(0x01)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0xd5, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }
        }
    }

    #[cfg(test)]
    mod store_instructions {
        use super::super::*;

        #[test]
        fn store_word_from_dst_into_immediate_address() {
            let mut program = BpfCode::new();
            program
                .store(MemSize::Word)
                .set_dst(0x01)
                .set_off(0x00_11)
                .set_imm(0x11_22_33_44)
                .push();

            assert_eq!(
                program.into_bytes(),
                &[0x62, 0x01, 0x11, 0x00, 0x44, 0x33, 0x22, 0x11]
            );
        }

        #[test]
        fn store_half_word_from_dst_into_immediate_address() {
            let mut program = BpfCode::new();
            program
                .store(MemSize::HalfWord)
                .set_dst(0x02)
                .set_off(0x11_22)
                .push();

            assert_eq!(
                program.into_bytes(),
                &[0x6a, 0x02, 0x22, 0x11, 0x00, 0x00, 0x00, 0x00]
            );
        }

        #[test]
        fn store_byte_from_dst_into_immediate_address() {
            let mut program = BpfCode::new();
            program.store(MemSize::Byte).push();

            assert_eq!(
                program.into_bytes(),
                &[0x72, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
            );
        }

        #[test]
        fn store_double_word_from_dst_into_immediate_address() {
            let mut program = BpfCode::new();
            program.store(MemSize::DoubleWord).push();

            assert_eq!(
                program.into_bytes(),
                &[0x7a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
            );
        }

        #[test]
        fn store_word_from_dst_into_src_address() {
            let mut program = BpfCode::new();
            program
                .store_x(MemSize::Word)
                .set_dst(0x01)
                .set_src(0x02)
                .push();

            assert_eq!(
                program.into_bytes(),
                &[0x63, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
            );
        }

        #[test]
        fn store_half_word_from_dst_into_src_address() {
            let mut program = BpfCode::new();
            program.store_x(MemSize::HalfWord).push();

            assert_eq!(
                program.into_bytes(),
                &[0x6b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
            );
        }

        #[test]
        fn store_byte_from_dst_into_src_address() {
            let mut program = BpfCode::new();
            program.store_x(MemSize::Byte).push();

            assert_eq!(
                program.into_bytes(),
                &[0x73, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
            );
        }

        #[test]
        fn store_double_word_from_dst_into_src_address() {
            let mut program = BpfCode::new();
            program.store_x(MemSize::DoubleWord).push();

            assert_eq!(
                program.into_bytes(),
                &[0x7b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
            );
        }
    }

    #[cfg(test)]
    mod load_instructions {
        #[cfg(test)]
        mod register {
            use super::super::super::*;

            #[test]
            fn load_word_from_set_src_with_offset() {
                let mut program = BpfCode::new();
                program
                    .load_x(MemSize::Word)
                    .set_dst(0x01)
                    .set_src(0x02)
                    .set_off(0x00_02)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x61, 0x21, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn load_half_word_from_set_src_with_offset() {
                let mut program = BpfCode::new();
                program
                    .load_x(MemSize::HalfWord)
                    .set_dst(0x02)
                    .set_src(0x01)
                    .set_off(0x11_22)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x69, 0x12, 0x22, 0x11, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn load_byte_from_set_src_with_offset() {
                let mut program = BpfCode::new();
                program
                    .load_x(MemSize::Byte)
                    .set_dst(0x01)
                    .set_src(0x04)
                    .set_off(0x00_11)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x71, 0x41, 0x11, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn load_double_word_from_set_src_with_offset() {
                let mut program = BpfCode::new();
                program
                    .load_x(MemSize::DoubleWord)
                    .set_dst(0x04)
                    .set_src(0x05)
                    .set_off(0x44_55)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x79, 0x54, 0x55, 0x44, 0x00, 0x00, 0x00, 0x00]
                );
            }
        }

        #[cfg(test)]
        mod immediate {
            use super::super::super::*;

            #[test]
            fn load_double_word() {
                let mut program = BpfCode::new();
                program
                    .load(MemSize::DoubleWord)
                    .set_dst(0x01)
                    .set_imm(0x00_01_02_03)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x18, 0x01, 0x00, 0x00, 0x03, 0x02, 0x01, 0x00]
                );
            }

            #[test]
            fn load_abs_word() {
                let mut program = BpfCode::new();
                program.load_abs(MemSize::Word).push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn load_abs_half_word() {
                let mut program = BpfCode::new();
                program.load_abs(MemSize::HalfWord).set_dst(0x05).push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x28, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn load_abs_byte() {
                let mut program = BpfCode::new();
                program.load_abs(MemSize::Byte).set_dst(0x01).push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x30, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn load_abs_double_word() {
                let mut program = BpfCode::new();
                program
                    .load_abs(MemSize::DoubleWord)
                    .set_dst(0x01)
                    .set_imm(0x01_02_03_04)
                    .push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x38, 0x01, 0x00, 0x00, 0x04, 0x03, 0x02, 0x01]
                );
            }

            #[test]
            fn load_indirect_word() {
                let mut program = BpfCode::new();
                program.load_ind(MemSize::Word).push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn load_indirect_half_word() {
                let mut program = BpfCode::new();
                program.load_ind(MemSize::HalfWord).push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x48, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn load_indirect_byte() {
                let mut program = BpfCode::new();
                program.load_ind(MemSize::Byte).push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }

            #[test]
            fn load_indirect_double_word() {
                let mut program = BpfCode::new();
                program.load_ind(MemSize::DoubleWord).push();

                assert_eq!(
                    program.into_bytes(),
                    &[0x58, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                );
            }
        }
    }

    #[cfg(test)]
    mod byte_swap_instructions {
        use super::super::*;

        #[test]
        fn convert_host_to_little_endian_16bits() {
            let mut program = BpfCode::new();
            program
                .swap_bytes(Endian::Little)
                .set_dst(0x01)
                .set_imm(0x00_00_00_10)
                .push();

            assert_eq!(
                program.into_bytes(),
                &[0xd4, 0x01, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00]
            );
        }

        #[test]
        fn convert_host_to_little_endian_32bits() {
            let mut program = BpfCode::new();
            program
                .swap_bytes(Endian::Little)
                .set_dst(0x02)
                .set_imm(0x00_00_00_20)
                .push();

            assert_eq!(
                program.into_bytes(),
                &[0xd4, 0x02, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00]
            );
        }

        #[test]
        fn convert_host_to_little_endian_64bit() {
            let mut program = BpfCode::new();
            program
                .swap_bytes(Endian::Little)
                .set_dst(0x03)
                .set_imm(0x00_00_00_40)
                .push();

            assert_eq!(
                program.into_bytes(),
                &[0xd4, 0x03, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00]
            );
        }

        #[test]
        fn convert_host_to_big_endian_16bits() {
            let mut program = BpfCode::new();
            program
                .swap_bytes(Endian::Big)
                .set_dst(0x01)
                .set_imm(0x00_00_00_10)
                .push();

            assert_eq!(
                program.into_bytes(),
                &[0xdc, 0x01, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00]
            );
        }

        #[test]
        fn convert_host_to_big_endian_32bits() {
            let mut program = BpfCode::new();
            program
                .swap_bytes(Endian::Big)
                .set_dst(0x02)
                .set_imm(0x00_00_00_20)
                .push();

            assert_eq!(
                program.into_bytes(),
                &[0xdc, 0x02, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00]
            );
        }

        #[test]
        fn convert_host_to_big_endian_64bit() {
            let mut program = BpfCode::new();
            program
                .swap_bytes(Endian::Big)
                .set_dst(0x03)
                .set_imm(0x00_00_00_40)
                .push();

            assert_eq!(
                program.into_bytes(),
                &[0xdc, 0x03, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00]
            );
        }
    }

    #[cfg(test)]
    mod moves_instructions {
        #[cfg(test)]
        mod arch_x64 {
            #[cfg(test)]
            mod immediate {
                use super::super::super::super::*;

                #[test]
                fn move_and_add_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .add(Source::Imm, Arch::X64)
                        .set_dst(0x02)
                        .set_imm(0x01_02_03_04)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x07, 0x02, 0x00, 0x00, 0x04, 0x03, 0x02, 0x01]
                    );
                }

                #[test]
                fn move_sub_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .sub(Source::Imm, Arch::X64)
                        .set_dst(0x04)
                        .set_imm(0x00_01_02_03)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x17, 0x04, 0x00, 0x00, 0x03, 0x02, 0x01, 0x00]
                    );
                }

                #[test]
                fn move_mul_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .mul(Source::Imm, Arch::X64)
                        .set_dst(0x05)
                        .set_imm(0x04_03_02_01)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x27, 0x05, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04]
                    );
                }

                #[test]
                fn move_div_constant_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .div(Source::Imm, Arch::X64)
                        .set_dst(0x02)
                        .set_imm(0x00_ff_00_ff)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x37, 0x02, 0x00, 0x00, 0xff, 0x00, 0xff, 0x00]
                    );
                }

                #[test]
                fn move_bit_or_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .bit_or(Source::Imm, Arch::X64)
                        .set_dst(0x02)
                        .set_imm(0x00_11_00_22)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x47, 0x02, 0x00, 0x00, 0x22, 0x00, 0x11, 0x00]
                    );
                }

                #[test]
                fn move_bit_and_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .bit_and(Source::Imm, Arch::X64)
                        .set_dst(0x02)
                        .set_imm(0x11_22_33_44)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x57, 0x02, 0x00, 0x00, 0x44, 0x33, 0x22, 0x11]
                    );
                }

                #[test]
                fn move_left_shift_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .left_shift(Source::Imm, Arch::X64)
                        .set_dst(0x01)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x67, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_logical_right_shift_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .right_shift(Source::Imm, Arch::X64)
                        .set_dst(0x01)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x77, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_negate_register() {
                    let mut program = BpfCode::new();
                    program.negate(Arch::X64).set_dst(0x02).push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x87, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_mod_const_to_register() {
                    let mut program = BpfCode::new();
                    program.modulo(Source::Imm, Arch::X64).set_dst(0x02).push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x97, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_bit_xor_const_to_register() {
                    let mut program = BpfCode::new();
                    program.bit_xor(Source::Imm, Arch::X64).set_dst(0x03).push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0xa7, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .mov(Source::Imm, Arch::X64)
                        .set_dst(0x01)
                        .set_imm(0x00_00_00_FF)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0xb7, 0x01, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_signed_right_shift_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .signed_right_shift(Source::Imm, Arch::X64)
                        .set_dst(0x05)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0xc7, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }
            }

            #[cfg(test)]
            mod register {
                use super::super::super::super::*;

                #[test]
                fn move_and_add_from_register() {
                    let mut program = BpfCode::new();
                    program
                        .add(Source::Reg, Arch::X64)
                        .set_dst(0x03)
                        .set_src(0x02)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x0f, 0x23, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_sub_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .sub(Source::Reg, Arch::X64)
                        .set_dst(0x03)
                        .set_src(0x04)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x1f, 0x43, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_mul_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .mul(Source::Reg, Arch::X64)
                        .set_dst(0x04)
                        .set_src(0x03)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x2f, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_div_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .div(Source::Reg, Arch::X64)
                        .set_dst(0x01)
                        .set_src(0x00)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x3f, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_bit_or_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .bit_or(Source::Reg, Arch::X64)
                        .set_dst(0x03)
                        .set_src(0x01)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x4f, 0x13, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_bit_and_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .bit_and(Source::Reg, Arch::X64)
                        .set_dst(0x03)
                        .set_src(0x02)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x5f, 0x23, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_left_shift_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .left_shift(Source::Reg, Arch::X64)
                        .set_dst(0x02)
                        .set_src(0x03)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x6f, 0x32, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_logical_right_shift_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .right_shift(Source::Reg, Arch::X64)
                        .set_dst(0x02)
                        .set_src(0x04)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x7f, 0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_mod_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .modulo(Source::Reg, Arch::X64)
                        .set_dst(0x01)
                        .set_src(0x02)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x9f, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_bit_xor_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .bit_xor(Source::Reg, Arch::X64)
                        .set_dst(0x02)
                        .set_src(0x04)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0xaf, 0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_from_register_to_another_register() {
                    let mut program = BpfCode::new();
                    program.mov(Source::Reg, Arch::X64).set_src(0x01).push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0xbf, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_signed_right_shift_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .signed_right_shift(Source::Reg, Arch::X64)
                        .set_dst(0x02)
                        .set_src(0x03)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0xcf, 0x32, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }
            }
        }

        #[cfg(test)]
        mod arch_x32 {
            #[cfg(test)]
            mod immediate {
                use super::super::super::super::*;

                #[test]
                fn move_and_add_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .add(Source::Imm, Arch::X32)
                        .set_dst(0x02)
                        .set_imm(0x01_02_03_04)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x04, 0x02, 0x00, 0x00, 0x04, 0x03, 0x02, 0x01]
                    );
                }

                #[test]
                fn move_sub_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .sub(Source::Imm, Arch::X32)
                        .set_dst(0x04)
                        .set_imm(0x00_01_02_03)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x14, 0x04, 0x00, 0x00, 0x03, 0x02, 0x01, 0x00]
                    );
                }

                #[test]
                fn move_mul_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .mul(Source::Imm, Arch::X32)
                        .set_dst(0x05)
                        .set_imm(0x04_03_02_01)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x24, 0x05, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04]
                    );
                }

                #[test]
                fn move_div_constant_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .div(Source::Imm, Arch::X32)
                        .set_dst(0x02)
                        .set_imm(0x00_ff_00_ff)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x34, 0x02, 0x00, 0x00, 0xff, 0x00, 0xff, 0x00]
                    );
                }

                #[test]
                fn move_bit_or_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .bit_or(Source::Imm, Arch::X32)
                        .set_dst(0x02)
                        .set_imm(0x00_11_00_22)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x44, 0x02, 0x00, 0x00, 0x22, 0x00, 0x11, 0x00]
                    );
                }

                #[test]
                fn move_bit_and_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .bit_and(Source::Imm, Arch::X32)
                        .set_dst(0x02)
                        .set_imm(0x11_22_33_44)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x54, 0x02, 0x00, 0x00, 0x44, 0x33, 0x22, 0x11]
                    );
                }

                #[test]
                fn move_left_shift_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .left_shift(Source::Imm, Arch::X32)
                        .set_dst(0x01)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x64, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_logical_right_shift_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .right_shift(Source::Imm, Arch::X32)
                        .set_dst(0x01)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x74, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_negate_register() {
                    let mut program = BpfCode::new();
                    program.negate(Arch::X32).set_dst(0x02).push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x84, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_mod_const_to_register() {
                    let mut program = BpfCode::new();
                    program.modulo(Source::Imm, Arch::X32).set_dst(0x02).push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x94, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_bit_xor_const_to_register() {
                    let mut program = BpfCode::new();
                    program.bit_xor(Source::Imm, Arch::X32).set_dst(0x03).push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0xa4, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .mov(Source::Imm, Arch::X32)
                        .set_dst(0x01)
                        .set_imm(0x00_00_00_FF)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0xb4, 0x01, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_signed_right_shift_const_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .signed_right_shift(Source::Imm, Arch::X32)
                        .set_dst(0x05)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0xc4, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }
            }

            #[cfg(test)]
            mod register {
                use super::super::super::super::*;

                #[test]
                fn move_and_add_from_register() {
                    let mut program = BpfCode::new();
                    program
                        .add(Source::Reg, Arch::X32)
                        .set_dst(0x03)
                        .set_src(0x02)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x0c, 0x23, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_sub_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .sub(Source::Reg, Arch::X32)
                        .set_dst(0x03)
                        .set_src(0x04)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x1c, 0x43, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_mul_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .mul(Source::Reg, Arch::X32)
                        .set_dst(0x04)
                        .set_src(0x03)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x2c, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_div_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .div(Source::Reg, Arch::X32)
                        .set_dst(0x01)
                        .set_src(0x00)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x3c, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_bit_or_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .bit_or(Source::Reg, Arch::X32)
                        .set_dst(0x03)
                        .set_src(0x01)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x4c, 0x13, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_bit_and_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .bit_and(Source::Reg, Arch::X32)
                        .set_dst(0x03)
                        .set_src(0x02)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x5c, 0x23, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_left_shift_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .left_shift(Source::Reg, Arch::X32)
                        .set_dst(0x02)
                        .set_src(0x03)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x6c, 0x32, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_logical_right_shift_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .right_shift(Source::Reg, Arch::X32)
                        .set_dst(0x02)
                        .set_src(0x04)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x7c, 0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_mod_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .modulo(Source::Reg, Arch::X32)
                        .set_dst(0x01)
                        .set_src(0x02)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0x9c, 0x21, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_bit_xor_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .bit_xor(Source::Reg, Arch::X32)
                        .set_dst(0x02)
                        .set_src(0x04)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0xac, 0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_from_register_to_another_register() {
                    let mut program = BpfCode::new();
                    program
                        .mov(Source::Reg, Arch::X32)
                        .set_dst(0x00)
                        .set_src(0x01)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0xbc, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }

                #[test]
                fn move_signed_right_shift_from_register_to_register() {
                    let mut program = BpfCode::new();
                    program
                        .signed_right_shift(Source::Reg, Arch::X32)
                        .set_dst(0x02)
                        .set_src(0x03)
                        .push();

                    assert_eq!(
                        program.into_bytes(),
                        &[0xcc, 0x32, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
                    );
                }
            }
        }
    }

    #[cfg(test)]
    mod programs {
        use super::super::*;

        #[test]
        fn example_from_assembler() {
            let mut program = BpfCode::new();
            program
                .add(Source::Imm, Arch::X64)
                .set_dst(1)
                .set_imm(0x605)
                .push()
                .mov(Source::Imm, Arch::X64)
                .set_dst(2)
                .set_imm(0x32)
                .push()
                .mov(Source::Reg, Arch::X64)
                .set_src(0)
                .set_dst(1)
                .push()
                .swap_bytes(Endian::Big)
                .set_dst(0)
                .set_imm(0x10)
                .push()
                .negate(Arch::X64)
                .set_dst(2)
                .push()
                .exit()
                .push();

            let bytecode = program.into_bytes();
            let ref_prog = &[
                0x07, 0x01, 0x00, 0x00, 0x05, 0x06, 0x00, 0x00, 0xb7, 0x02, 0x00, 0x00, 0x32, 0x00,
                0x00, 0x00, 0xbf, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xdc, 0x00, 0x00, 0x00,
                0x10, 0x00, 0x00, 0x00, 0x87, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x95, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ];
            // cargo says: "`[{integer}; 48]` cannot be formatted using `{:?}`
            //              because it doesn't implement `std::fmt::Debug`"
            // So let's check in two steps.
            assert_eq!(bytecode[..32], ref_prog[..32]);
            assert_eq!(bytecode[33..], ref_prog[33..]);
        }
    }
}
