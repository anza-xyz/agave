#![allow(dead_code)]

use solana_rbpf::insn_builder::{Arch, BpfCode, Cond, Endian, Instruction, MemSize, Source};

#[derive(arbitrary::Arbitrary, Debug, Eq, PartialEq)]
pub enum FuzzedOp {
    Add(Source),
    Sub(Source),
    Mul(Source),
    Div(Source),
    BitOr(Source),
    BitAnd(Source),
    LeftShift(Source),
    RightShift(Source),
    Negate,
    Modulo(Source),
    BitXor(Source),
    Mov(Source),
    Srs(Source),
    SwapBytes(Endian),
    Load(MemSize),
    LoadAbs(MemSize),
    LoadInd(MemSize),
    LoadX(MemSize),
    Store(MemSize),
    StoreX(MemSize),
    Jump,
    JumpC(Cond, Source),
    Call,
    Exit,
}

impl FuzzedOp {
    fn similarity(&self, other: &FuzzedOp) -> Option<u8> {
        if std::mem::discriminant(self) == std::mem::discriminant(other) {
            if self == other {
                Some(0)
            } else {
                Some(8)
            }
        } else {
            None
        }
    }
}

#[derive(arbitrary::Arbitrary, Debug)]
pub struct FuzzedInstruction {
    pub op: FuzzedOp,
    pub dst: u8,
    pub src: u8,
    pub off: i16,
    pub imm: i64,
}

impl FuzzedInstruction {
    pub fn similarity(&self, other: &FuzzedInstruction) -> Option<u8> {
        self.op.similarity(&other.op).map(|s| {
            s.saturating_add((self.dst == other.dst) as u8)
                .saturating_add((self.src == other.src) as u8)
                .saturating_add((self.off == other.off) as u8)
                .saturating_add((self.imm == other.imm) as u8)
        })
    }
}

pub type FuzzProgram = Vec<FuzzedInstruction>;

pub fn make_program(prog: &FuzzProgram, arch: Arch) -> BpfCode {
    let mut code = BpfCode::default();
    for inst in prog {
        match inst.op {
            FuzzedOp::Add(src) => code
                .add(src, arch)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::Sub(src) => code
                .sub(src, arch)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::Mul(src) => code
                .mul(src, arch)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::Div(src) => code
                .div(src, arch)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::BitOr(src) => code
                .bit_or(src, arch)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::BitAnd(src) => code
                .bit_and(src, arch)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::LeftShift(src) => code
                .left_shift(src, arch)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::RightShift(src) => code
                .right_shift(src, arch)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::Negate => code
                .negate(arch)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::Modulo(src) => code
                .modulo(src, arch)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::BitXor(src) => code
                .bit_xor(src, arch)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::Mov(src) => code
                .mov(src, arch)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::Srs(src) => code
                .signed_right_shift(src, arch)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::SwapBytes(endian) => code
                .swap_bytes(endian)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::Load(mem) => code
                .load(mem)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::LoadAbs(mem) => code
                .load_abs(mem)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::LoadInd(mem) => code
                .load_ind(mem)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::LoadX(mem) => code
                .load_x(mem)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::Store(mem) => code
                .store(mem)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::StoreX(mem) => code
                .store_x(mem)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::Jump => code
                .jump_unconditional()
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::JumpC(cond, src) => code
                .jump_conditional(cond, src)
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::Call => code
                .call()
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
            FuzzedOp::Exit => code
                .exit()
                .set_dst(inst.dst)
                .set_src(inst.src)
                .set_off(inst.off)
                .set_imm(inst.imm)
                .push(),
        };
    }
    code
}
