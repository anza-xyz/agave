#![allow(dead_code)]
// based on: https://sourceware.org/binutils/docs/as/BPF-Opcodes.html

use {
    solana_rbpf::insn_builder::{Arch, BpfCode, Cond, Endian, Instruction, MemSize, Move, Source},
    std::num::NonZeroI32,
};

#[derive(arbitrary::Arbitrary, Debug, Eq, PartialEq, Copy, Clone)]
pub struct Register(u8);

impl Register {
    #[cfg(feature = "only-verified")]
    fn to_dst(&self) -> u8 {
        self.0 % 10 // cannot write to r10
    }

    #[cfg(not(feature = "only-verified"))]
    fn to_dst(self) -> u8 {
        self.0 % 11 // cannot write to r10, but we'll try anyways
    }

    fn to_src(self) -> u8 {
        self.0 % 11
    }
}

#[derive(arbitrary::Arbitrary, Debug, Eq, PartialEq, Copy, Clone)]
pub enum FuzzedSource {
    Reg(Register),
    Imm(i32),
}

#[derive(arbitrary::Arbitrary, Debug, Eq, PartialEq, Copy, Clone)]
pub enum FuzzedNonZeroSource {
    Reg(Register),
    Imm(NonZeroI32),
}

impl From<&FuzzedSource> for Source {
    fn from(src: &FuzzedSource) -> Self {
        match src {
            FuzzedSource::Reg(_) => Source::Reg,
            FuzzedSource::Imm(_) => Source::Imm,
        }
    }
}

impl From<&FuzzedNonZeroSource> for Source {
    fn from(src: &FuzzedNonZeroSource) -> Self {
        match src {
            FuzzedNonZeroSource::Reg(_) => Source::Reg,
            FuzzedNonZeroSource::Imm(_) => Source::Imm,
        }
    }
}

#[derive(arbitrary::Arbitrary, Debug, Eq, PartialEq, Copy, Clone)]
pub enum SwapSize {
    S16 = 16,
    S32 = 32,
    S64 = 64,
}

#[derive(arbitrary::Arbitrary, Debug, Eq, PartialEq, Copy, Clone)]
pub enum FuzzedInstruction {
    Add(Arch, Register, FuzzedSource),
    Sub(Arch, Register, FuzzedSource),
    Mul(Arch, Register, FuzzedSource),
    Div(Arch, Register, FuzzedNonZeroSource),
    BitOr(Arch, Register, FuzzedSource),
    BitAnd(Arch, Register, FuzzedSource),
    LeftShift(Arch, Register, FuzzedSource),
    RightShift(Arch, Register, FuzzedSource),
    Negate(Arch, Register),
    Modulo(Arch, Register, FuzzedNonZeroSource),
    BitXor(Arch, Register, FuzzedSource),
    Mov(Arch, Register, FuzzedSource),
    Srs(Arch, Register, FuzzedSource),
    SwapBytes(Register, Endian, SwapSize),
    #[cfg(feature = "only-verified")]
    // load only has lddw; there are no other variants, and it needs to be split
    Load(Register, i32, i32),
    #[cfg(not(feature = "only-verified"))]
    // illegal load variants
    Load(Register, MemSize, i64),
    LoadAbs(MemSize, i32),
    LoadInd(MemSize, Register, i32),
    LoadX(Register, MemSize, Register, i16),
    Store(Register, MemSize, i16, i32),
    StoreX(Register, MemSize, i16, Register),
    Jump(i16),
    JumpC(Register, Cond, FuzzedSource, i16),
    Call(i32),
    Exit,
}

pub type FuzzProgram = Vec<FuzzedInstruction>;

fn complete_alu_insn(insn: Move<'_>, dst: &Register, src: &FuzzedSource) {
    match src {
        FuzzedSource::Reg(r) => insn.set_dst(dst.to_dst()).set_src(r.to_src()).push(),
        FuzzedSource::Imm(imm) => insn.set_dst(dst.to_dst()).set_imm(*imm as i64).push(),
    };
}

fn complete_alu_insn_shift(insn: Move<'_>, dst: &Register, src: &FuzzedSource, max: i64) {
    match src {
        FuzzedSource::Reg(r) => insn.set_dst(dst.to_dst()).set_src(r.to_src()).push(),
        FuzzedSource::Imm(imm) => insn
            .set_dst(dst.to_dst())
            .set_imm((*imm as i64).rem_euclid(max))
            .push(),
    };
}

fn complete_alu_insn_nonzero(insn: Move<'_>, dst: &Register, src: &FuzzedNonZeroSource) {
    match src {
        FuzzedNonZeroSource::Reg(r) => insn.set_dst(dst.to_dst()).set_src(r.to_src()).push(),
        FuzzedNonZeroSource::Imm(imm) => insn
            .set_dst(dst.to_dst())
            .set_imm(i32::from(*imm) as i64)
            .push(),
    };
}

#[cfg(feature = "only-verified")]
fn fix_jump(prog: &FuzzProgram, off: i16, pos: usize, len: usize) -> i16 {
    let target = (off as usize).rem_euclid(len);
    if target == 0 {
        return target as i16 - pos as i16 - 1;
    }
    let mut remaining = target;
    for insn in prog.iter() {
        let next = match insn {
            FuzzedInstruction::Load(_, _, _) => remaining.checked_sub(2),
            _ => remaining.checked_sub(1),
        };
        match next {
            None => {
                return target as i16 - pos as i16 - 2;
            }
            Some(0) => {
                return target as i16 - pos as i16 - 1;
            }
            Some(next) => remaining = next,
        }
    }
    unreachable!("Incorrectly computed length.")
}

#[cfg(not(feature = "only-verified"))]
fn fix_jump(_: &FuzzProgram, off: i16, _: usize, _: usize) -> i16 {
    off
}

// lddw is twice length
fn calculate_length(prog: &FuzzProgram) -> usize {
    prog.len().saturating_add(
        prog.iter()
            .filter(|&&insn| matches!(insn, FuzzedInstruction::Load(_, _, _)))
            .count(),
    )
}

pub fn make_program(prog: &FuzzProgram) -> BpfCode {
    let mut code = BpfCode::default();
    let len = calculate_length(prog);
    let mut pos = 0;

    #[allow(clippy::explicit_counter_loop)]
    for inst in prog.iter() {
        let op = if let FuzzedInstruction::JumpC(_, Cond::Abs, FuzzedSource::Reg(_), off) = inst {
            FuzzedInstruction::Jump(*off)
        } else {
            *inst
        };
        match &op {
            FuzzedInstruction::Add(a, d, s) => complete_alu_insn(code.add(s.into(), *a), d, s),
            FuzzedInstruction::Sub(a, d, s) => complete_alu_insn(code.sub(s.into(), *a), d, s),
            FuzzedInstruction::Mul(a, d, s) => complete_alu_insn(code.mul(s.into(), *a), d, s),
            FuzzedInstruction::Div(a, d, s) => {
                complete_alu_insn_nonzero(code.div(s.into(), *a), d, s)
            }
            FuzzedInstruction::BitOr(a, d, s) => complete_alu_insn(code.bit_or(s.into(), *a), d, s),
            FuzzedInstruction::BitAnd(a, d, s) => {
                complete_alu_insn(code.bit_and(s.into(), *a), d, s)
            }
            FuzzedInstruction::LeftShift(a, d, s) => match a {
                Arch::X64 => complete_alu_insn_shift(code.left_shift(s.into(), *a), d, s, 64),
                Arch::X32 => complete_alu_insn_shift(code.left_shift(s.into(), *a), d, s, 32),
            },
            FuzzedInstruction::RightShift(a, d, s) => match a {
                Arch::X64 => complete_alu_insn_shift(code.right_shift(s.into(), *a), d, s, 64),
                Arch::X32 => complete_alu_insn_shift(code.right_shift(s.into(), *a), d, s, 32),
            },
            FuzzedInstruction::Negate(a, d) => {
                code.negate(*a).set_dst(d.to_dst()).push();
            }
            FuzzedInstruction::Modulo(a, d, s) => {
                complete_alu_insn_nonzero(code.modulo(s.into(), *a), d, s)
            }
            FuzzedInstruction::BitXor(a, d, s) => {
                complete_alu_insn(code.bit_xor(s.into(), *a), d, s)
            }
            FuzzedInstruction::Mov(a, d, s) => complete_alu_insn(code.mov(s.into(), *a), d, s),
            FuzzedInstruction::Srs(a, d, s) => match a {
                Arch::X64 => {
                    complete_alu_insn_shift(code.signed_right_shift(s.into(), *a), d, s, 64)
                }
                Arch::X32 => {
                    complete_alu_insn_shift(code.signed_right_shift(s.into(), *a), d, s, 32)
                }
            },
            FuzzedInstruction::SwapBytes(d, e, s) => {
                code.swap_bytes(*e)
                    .set_dst(d.to_dst())
                    .set_imm(*s as i64)
                    .push();
            }
            #[cfg(feature = "only-verified")]
            FuzzedInstruction::Load(d, imm1, imm2) => {
                // lddw is split in two
                code.load(MemSize::DoubleWord)
                    .set_dst(d.to_dst())
                    .set_imm(*imm1 as i64)
                    .push()
                    .load(MemSize::Word)
                    .set_imm(*imm2 as i64)
                    .push();
                pos += 1;
            }
            #[cfg(not(feature = "only-verified"))]
            FuzzedInstruction::Load(d, m, imm) => {
                // lddw should be split in two
                code.load(*m).set_dst(d.to_dst()).set_imm(*imm).push();
            }
            FuzzedInstruction::LoadAbs(m, imm) => {
                code.load_abs(*m).set_imm(*imm as i64).push();
            }
            FuzzedInstruction::LoadInd(m, s, imm) => {
                code.load_ind(*m)
                    .set_src(s.to_src())
                    .set_imm(*imm as i64)
                    .push();
            }
            FuzzedInstruction::LoadX(d, m, s, off) => {
                code.load_x(*m)
                    .set_dst(d.to_dst())
                    .set_src(s.to_src())
                    .set_off(*off)
                    .push();
            }
            FuzzedInstruction::Store(d, m, off, imm) => {
                code.store(*m)
                    .set_dst(d.to_src())
                    .set_off(*off)
                    .set_imm(*imm as i64)
                    .push();
            }
            FuzzedInstruction::StoreX(d, m, off, s) => {
                code.store_x(*m)
                    .set_dst(d.to_src())
                    .set_off(*off)
                    .set_src(s.to_src())
                    .push();
            }
            FuzzedInstruction::Jump(off) => {
                code.jump_unconditional()
                    .set_off(fix_jump(prog, *off, pos, len))
                    .push();
            }
            FuzzedInstruction::JumpC(d, c, s, off) => {
                match s {
                    FuzzedSource::Reg(r) => code
                        .jump_conditional(*c, s.into())
                        .set_dst(d.to_dst())
                        .set_src(r.to_src())
                        .set_off(fix_jump(prog, *off, pos, len))
                        .push(),
                    FuzzedSource::Imm(imm) => code
                        .jump_conditional(*c, s.into())
                        .set_dst(d.to_dst())
                        .set_imm(*imm as i64)
                        .set_off(fix_jump(prog, *off, pos, len))
                        .push(),
                };
            }
            FuzzedInstruction::Call(imm) => {
                code.call().set_imm(*imm as i64).push();
            }
            FuzzedInstruction::Exit => {
                code.exit().push();
            }
        };
        pos = pos.saturating_add(1);
    }
    code.exit().push();
    code
}
