#![allow(clippy::arithmetic_side_effects)]
#![allow(clippy::upper_case_acronyms)]
#![allow(dead_code)]
use crate::{
    jit::{JitCompiler, OperandSize},
    vm::ContextObject,
};

macro_rules! exclude_operand_sizes {
    ($size:expr, $($to_exclude:path)|+ $(,)?) => {
        debug_assert!(match $size {
            $($to_exclude)|+ => false,
            _ => true,
        });
    }
}

pub const X0: u8 = 0;
pub const X1: u8 = 1;
pub const X2: u8 = 2;
pub const X3: u8 = 3;
pub const X4: u8 = 4;
pub const X5: u8 = 5;
pub const X6: u8 = 6;
pub const X7: u8 = 7;
pub const X8: u8 = 8;
pub const XR: u8 = X8;
pub const X9: u8 = 9;
pub const X10: u8 = 10;
pub const X11: u8 = 11;
pub const X12: u8 = 12;
pub const X13: u8 = 13;
pub const X14: u8 = 14;
pub const X15: u8 = 15;

// There are more registers, but I'm not sure we have a use for them
// NOTE: x18 is reserved on Apple platforms

pub const X19: u8 = 19;
pub const X20: u8 = 20;
pub const X21: u8 = 21;
pub const X22: u8 = 22;
pub const X23: u8 = 23;
pub const X24: u8 = 24;
pub const X25: u8 = 25;
pub const X26: u8 = 26;
pub const X27: u8 = 27;
pub const X28: u8 = 28;
pub const FP: u8 = 29; // NOTE: On Apple platforms, they say FP must always address a valid frame record
pub const LR: u8 = 30;
pub const SP_XZR: u8 = 31; // SP or XZR, depending on context

// Tested on Linux, macOS, but not on Windows
pub const ARGUMENT_REGISTERS: [u8; 8] = [X0, X1, X2, X3, X4, X5, X6, X7];
pub const CALLER_SAVED_REGISTERS: [u8; 7] = [X9, X10, X11, X12, X13, X14, X15];
pub const CALLEE_SAVED_REGISTERS: [u8; 12] =
    [X19, X20, X21, X22, X23, X24, X25, X26, X27, X28, LR, FP];

#[derive(Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum ShiftType {
    LSL = 0, // logical shift left
             // LSR = 1, // logical shift right
             // ASR = 2, // arithmetic shift right
             // ROR = 3, // rotate right
}

#[derive(Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum Condition {
    EQ = 0,  // equal
    NE = 1,  // not equal
    CS = 2,  // carry set, also HS (unsigned >=)
    CC = 3,  // carry clear, also LO (unsigned <)
    HI = 8,  // unsigned >
    LS = 9,  // unsigned <=
    GE = 10, // signed >=
    LT = 11, // signed <
    GT = 12, // signed >
    LE = 13, // signed <=
             // AL = 14, // always
}

impl Condition {
    // Aliases
    pub const HS: Condition = Condition::CS; // unsigned >=
    pub const LO: Condition = Condition::CC; // unsigned <
}

pub struct ARM64BitwiseImm {
    immr: u8, // 6 bits
    imms: u8, // 6 bits
    n: u8,    // 1 bit
}

impl ARM64BitwiseImm {
    pub const ONE: ARM64BitwiseImm = ARM64BitwiseImm {
        immr: 0,
        imms: 0,
        n: 1,
    };
}

#[derive(PartialEq, Eq, Copy, Clone)]
pub enum ARM64MemoryOperand {
    #[allow(dead_code)]
    OffsetScaled(u16), // ldr dst, [src, #offset] (unsigned offset, scaled)
    Offset(i16),                // ldur dst, [src, #offset] (signed offset, unscaled)
    OffsetPreIndex(i16), // ldr dst, [src, #offset]! (signed offset, unscaled) **autoincrement**
    OffsetPostIndex(i16), // ldr dst, [src], #offset (signed offset, unscaled) **autoincrement**
    OffsetIndexShift(u8, bool), // ldr dst, [src, idx << 3]; u8 is idx register, bool is whether to shift
}

// Instructions are broken up based on the encoding scheme used
#[derive(Copy, Clone)]
pub enum ARM64Instruction {
    LogicalRegister(ARM64InstructionLogicalShiftedRegister),
    AddSubRegister(ARM64InstructionLogicalShiftedRegister),
    AddSubImm(ARM64InstructionAddSubImm),
    ConditionalBranch(ARM64InstructionConditonalBranch),
    LogicalImm(ARM64InstructionLogicalImm),
    BitfieldImm(ARM64InstructionLogicalImm),
    MovWideImm(ARM64InstructionWideImm),
    DataProcessing1Src(ARM64InstructionDataProcessing),
    DataProcessing2Src(ARM64InstructionDataProcessing),
    DataProcessing3Src(ARM64InstructionDataProcessing),
    BranchImm26(ARM64InstructionImm26),
    BLR(ARM64InstructionBLR),
    Load(ARM64InstructionLoadStore),
    Store(ARM64InstructionLoadStore),
    RET,
}

#[derive(Copy, Clone)]
pub struct ARM64InstructionLogicalShiftedRegister {
    pub size: OperandSize,
    pub opcode: u8,            // 2 bits
    pub n: u8,                 // negation (1 bit)
    pub shift_type: ShiftType, // 2 bits
    pub dest: u8,              // Rd, 5 bits
    pub src1: u8,              // Rn, 5 bits
    pub src2: u8,              // Rm, 5 bits
    pub imm6: u8,              // shift amount (0-31 or 0-63)
}

impl Default for ARM64InstructionLogicalShiftedRegister {
    fn default() -> Self {
        Self {
            size: OperandSize::S64,
            opcode: 0,
            n: 0,
            shift_type: ShiftType::LSL,
            dest: 0,
            src1: 0,
            src2: 0,
            imm6: 0,
        }
    }
}

#[derive(Copy, Clone)]
pub struct ARM64InstructionDataProcessing {
    pub size: OperandSize,
    pub opcode: u8, // 6 bits
    pub dest: u8,   // Rd, 5 bits
    pub src1: u8,   // Rn, 5 bits
    pub src2: u8,   // Rm, 5 bits, only used in 2, 3-src insts
    pub src3: u8,   // R1, 5 bits, only used in 3-src insts
    pub o0: u8,     // 1 bit, used only in 3-src insts
}

impl Default for ARM64InstructionDataProcessing {
    fn default() -> Self {
        Self {
            size: OperandSize::S64,
            opcode: 0,
            dest: 0,
            src1: 0,
            src2: 0,
            src3: 0,
            o0: 0,
        }
    }
}

#[derive(Copy, Clone)]
pub struct ARM64InstructionAddSubImm {
    pub size: OperandSize,
    pub opcode: u8,     // 1 bit
    pub sets_flags: u8, // 1 bit
    pub shift_mode: u8, // 00 (LSL 0) and 01 (LSL 12) are supported
    pub dest: u8,       // Rd, 5 bits
    pub src: u8,        // Rn, 5 bits
    pub imm12: u16,     // unsigned imm12
}

impl Default for ARM64InstructionAddSubImm {
    fn default() -> Self {
        Self {
            size: OperandSize::S64,
            opcode: 0,
            sets_flags: 0,
            shift_mode: 0,
            dest: 0,
            src: 0,
            imm12: 0,
        }
    }
}

#[derive(Copy, Clone)]
pub struct ARM64InstructionLogicalImm {
    pub size: OperandSize,
    pub opcode: u8, // 2 bits
    pub n: u8,      // negation (1 bit)
    pub dest: u8,   // Rd, 5 bits
    pub src: u8,    // Rn, 5 bits
    pub immr: u8,   // imm6
    pub imms: u8,   // imm6
}

impl Default for ARM64InstructionLogicalImm {
    fn default() -> Self {
        Self {
            size: OperandSize::S64,
            opcode: 0,
            n: 0,
            dest: 0,
            src: 0,
            immr: 0,
            imms: 0,
        }
    }
}

#[derive(Copy, Clone)]
pub struct ARM64InstructionWideImm {
    pub size: OperandSize,
    pub opcode: u8, // 2 bits
    pub hw: u8,     // shift (0, 16, 32, 48), encoded as 2-bits
    pub dest: u8,   // Rd, 5 bits
    pub imm16: u16, // imm6
}

impl Default for ARM64InstructionWideImm {
    fn default() -> Self {
        Self {
            size: OperandSize::S64,
            opcode: 0,
            hw: 0,
            dest: 0,
            imm16: 0,
        }
    }
}

#[derive(Copy, Clone)]
pub struct ARM64InstructionConditonalBranch {
    pub cond: u8,   // 4 bits
    pub imm19: i32, // offset from current instruction, divided by 4
}

#[derive(Copy, Clone)]
pub struct ARM64InstructionImm26 {
    pub opcode: u8, // 6 bits
    pub imm26: i32, // offset from current instruction, divided by 4
}

#[derive(Copy, Clone)]
pub struct ARM64InstructionBLR {
    pub target: u8, // 5 bit target register
}

// Load

#[derive(Copy, Clone)]
pub struct ARM64InstructionLoadStore {
    pub size: OperandSize,
    pub data: u8, // Rt, 5 bits
    pub base: u8, // Rn, 5 bits (base register)
    pub mem: ARM64MemoryOperand,
}

impl Default for ARM64InstructionLoadStore {
    fn default() -> Self {
        Self {
            size: OperandSize::S64,
            data: 0,
            base: 0,
            mem: ARM64MemoryOperand::Offset(0), // default to an LDUR (no autoincrement)
        }
    }
}

impl ARM64Instruction {
    pub fn emit<C: ContextObject>(&self, jit: &mut JitCompiler<C>) {
        let mut ins: u32 = 0;

        match self {
            ARM64Instruction::LogicalRegister(s) | ARM64Instruction::AddSubRegister(s) => {
                ins |= (s.dest & 0b11111) as u32;
                ins |= ((s.src1 & 0b11111) as u32) << 5;
                ins |= ((s.src2 & 0b11111) as u32) << 16;
                ins |= ((s.imm6 & 0b111111) as u32) << 10;
                ins |= ((s.n & 0b1) as u32) << 21;
                ins |= (s.shift_type as u32) << 22;
                ins |= ((s.opcode & 0b11) as u32) << 29;

                match self {
                    ARM64Instruction::LogicalRegister(_) => ins |= 0b01010u32 << 24,
                    ARM64Instruction::AddSubRegister(_) => ins |= 0b01011u32 << 24,
                    _ => unreachable!(),
                };

                let sf: u8 = match s.size {
                    OperandSize::S64 => 1,
                    _ => 0,
                };

                ins |= (sf as u32) << 31;
            }
            ARM64Instruction::DataProcessing2Src(s) | ARM64Instruction::DataProcessing1Src(s) => {
                ins |= (s.dest & 0b11111) as u32;
                ins |= ((s.src1 & 0b11111) as u32) << 5;
                ins |= ((s.src2 & 0b11111) as u32) << 16;
                ins |= ((s.opcode & 0b111111) as u32) << 10;
                if let ARM64Instruction::DataProcessing1Src(_) = self {
                    ins |= 0b1u32 << 30
                }
                ins |= 0b11010110u32 << 21;
                let sf: u8 = match s.size {
                    OperandSize::S64 => 1,
                    _ => 0,
                };

                ins |= (sf as u32) << 31;
            }
            ARM64Instruction::DataProcessing3Src(s) => {
                ins |= (s.dest & 0b11111) as u32;
                ins |= ((s.src1 & 0b11111) as u32) << 5;
                ins |= ((s.src2 & 0b11111) as u32) << 16;
                ins |= ((s.src3 & 0b11111) as u32) << 10;
                ins |= ((s.opcode & 0b111) as u32) << 21;
                ins |= ((s.o0 & 0b1) as u32) << 15;

                ins |= 0b11011u32 << 24;
                let sf: u8 = match s.size {
                    OperandSize::S64 => 1,
                    _ => 0,
                };

                ins |= (sf as u32) << 31;
            }
            ARM64Instruction::AddSubImm(s) => {
                ins |= (s.dest & 0b11111) as u32;
                ins |= ((s.src & 0b11111) as u32) << 5;
                ins |= ((s.imm12 & 0b111111111111) as u32) << 10;
                ins |= (s.shift_mode as u32) << 22;
                ins |= ((s.sets_flags & 0b1) as u32) << 29;
                ins |= ((s.opcode & 0b1) as u32) << 30;

                match self {
                    ARM64Instruction::AddSubImm(_) => ins |= 0b10001u32 << 24,
                    _ => unreachable!(),
                };

                let sf: u8 = match s.size {
                    OperandSize::S64 => 1,
                    _ => 0,
                };

                ins |= (sf as u32) << 31;
            }
            ARM64Instruction::LogicalImm(s) | ARM64Instruction::BitfieldImm(s) => {
                ins |= (s.dest & 0b11111) as u32;
                ins |= ((s.src & 0b11111) as u32) << 5;
                ins |= ((s.imms & 0b111111) as u32) << 10;
                ins |= ((s.immr & 0b111111) as u32) << 16;
                ins |= ((s.n & 0b1) as u32) << 22;
                ins |= ((s.opcode & 0b11) as u32) << 29;

                match self {
                    ARM64Instruction::LogicalImm(_) => ins |= 0b100100u32 << 23,
                    ARM64Instruction::BitfieldImm(_) => ins |= 0b100110u32 << 23,
                    _ => unreachable!(),
                };

                let sf: u8 = match s.size {
                    OperandSize::S64 => 1,
                    _ => 0,
                };

                ins |= (sf as u32) << 31;
            }
            ARM64Instruction::MovWideImm(s) => {
                ins |= (s.dest & 0b11111) as u32;
                ins |= (s.imm16 as u32) << 5;
                ins |= ((s.hw & 0b11) as u32) << 21;
                ins |= ((s.opcode & 0b11) as u32) << 29;

                match self {
                    ARM64Instruction::MovWideImm(_) => ins |= 0b100101u32 << 23,
                    _ => unreachable!(),
                };

                let sf: u8 = match s.size {
                    OperandSize::S64 => 1,
                    _ => 0,
                };

                ins |= (sf as u32) << 31;
            }
            ARM64Instruction::ConditionalBranch(s) => {
                ins |= (s.cond & 0b1111) as u32;
                ins |= ((s.imm19 as u32) & ((1u32 << 19) - 1u32)) << 5;
                ins |= 0b01010100u32 << 24;
            }
            ARM64Instruction::BranchImm26(s) => {
                ins |= (s.imm26 as u32) & ((1u32 << 26) - 1u32);
                ins |= (s.opcode as u32) << 26;
            }
            ARM64Instruction::BLR(s) => {
                ins |= 0b11010110001111110000000000000000u32;
                ins |= ((s.target & 0b11111) as u32) << 5;
            }
            ARM64Instruction::RET => {
                ins = 0xd65f03c0;
            }
            ARM64Instruction::Load(s) | ARM64Instruction::Store(s) => {
                ins |= (s.data & 0b11111) as u32;
                ins |= ((s.base & 0b11111) as u32) << 5;
                let mode = match s.mem {
                    ARM64MemoryOperand::OffsetPreIndex(_) => 0b11,
                    ARM64MemoryOperand::OffsetPostIndex(_) => 0b01,
                    ARM64MemoryOperand::OffsetScaled(_) => 0b00, // spot used for imm12,
                    ARM64MemoryOperand::OffsetIndexShift(_, _) => 0b10,
                    ARM64MemoryOperand::Offset(_) => 0b00,
                };
                ins |= (mode as u32) << 10;

                // Encode the memory operand
                match s.mem {
                    ARM64MemoryOperand::OffsetPreIndex(imm9)
                    | ARM64MemoryOperand::OffsetPostIndex(imm9)
                    | ARM64MemoryOperand::Offset(imm9) => {
                        ins |= ((imm9 & 0b111111111) as u32) << 12;
                    }
                    ARM64MemoryOperand::OffsetScaled(imm12) => {
                        ins |= ((imm12 & 0b111111111111) as u32) << 10;
                    }
                    ARM64MemoryOperand::OffsetIndexShift(idx_reg, should_shift) => {
                        if should_shift {
                            ins |= 0b1u32 << 12;
                        }
                        ins |= 0b011u32 << 13;
                        ins |= ((idx_reg & 0b11111) as u32) << 16;
                    }
                };

                // Opcode (we choose the zero-extending version for all)
                match s.mem {
                    ARM64MemoryOperand::OffsetPreIndex(_)
                    | ARM64MemoryOperand::OffsetPostIndex(_)
                    | ARM64MemoryOperand::Offset(_) => {
                        ins |= (if matches!(self, ARM64Instruction::Load(_)) {
                            0b111000010u32
                        } else {
                            0b111000000
                        }) << 21;
                    }
                    ARM64MemoryOperand::OffsetScaled(_) => {
                        ins |= (if matches!(self, ARM64Instruction::Load(_)) {
                            0b11100101u32
                        } else {
                            0b11100100
                        }) << 22;
                    }
                    ARM64MemoryOperand::OffsetIndexShift(_, _) => {
                        ins |= (if matches!(self, ARM64Instruction::Load(_)) {
                            0b111000011u32
                        } else {
                            0b111000001
                        }) << 21;
                    }
                };

                // Encode size
                let size: u32 = match s.size {
                    OperandSize::S64 => 0b11,
                    OperandSize::S32 => 0b10,
                    OperandSize::S16 => 0b01,
                    OperandSize::S8 => 0b00,
                    OperandSize::S0 => panic!("bad operand size"),
                };
                ins |= size << 30;
            }
        }

        jit.emit::<u32>(ins);
    }

    /// Move source to destination
    #[must_use]
    pub fn mov(size: OperandSize, source: u8, destination: u8) -> Self {
        // mov is same as ORR <dst>, XZR, <src>
        Self::LogicalRegister(ARM64InstructionLogicalShiftedRegister {
            size,
            opcode: 1,
            n: 0,
            dest: destination,
            src1: SP_XZR,
            src2: source,
            ..ARM64InstructionLogicalShiftedRegister::default()
        })
    }

    #[must_use]
    pub fn orr(size: OperandSize, source: u8, destination: u8) -> Self {
        Self::LogicalRegister(ARM64InstructionLogicalShiftedRegister {
            size,
            opcode: 1,
            n: 0,
            dest: destination,
            src1: destination,
            src2: source,
            ..ARM64InstructionLogicalShiftedRegister::default()
        })
    }

    #[must_use]
    pub fn and(size: OperandSize, src1: u8, src2: u8, destination: u8) -> Self {
        Self::LogicalRegister(ARM64InstructionLogicalShiftedRegister {
            size,
            opcode: 0,
            n: 0,
            dest: destination,
            src1,
            src2,
            ..ARM64InstructionLogicalShiftedRegister::default()
        })
    }

    #[must_use]
    pub fn eor(size: OperandSize, source: u8, destination: u8) -> Self {
        Self::LogicalRegister(ARM64InstructionLogicalShiftedRegister {
            size,
            opcode: 2,
            n: 0,
            dest: destination,
            src1: destination,
            src2: source,
            ..ARM64InstructionLogicalShiftedRegister::default()
        })
    }

    #[must_use]
    pub fn tst(size: OperandSize, source: u8, destination: u8) -> Self {
        Self::LogicalRegister(ARM64InstructionLogicalShiftedRegister {
            size,
            opcode: 3,
            n: 0,
            dest: SP_XZR, // discard result
            src1: destination,
            src2: source,
            ..ARM64InstructionLogicalShiftedRegister::default()
        })
    }

    #[must_use]
    pub fn tst_imm(source: u8, imm: ARM64BitwiseImm) -> Self {
        Self::LogicalImm(ARM64InstructionLogicalImm {
            size: OperandSize::S64,
            opcode: 3,
            n: imm.n,
            immr: imm.immr,
            imms: imm.imms,
            dest: SP_XZR, // discard result
            src: source,
        })
    }

    // Here we implement the "shifted register" variant of ADD and SUB.
    //
    // There is also exists an "extended register" variant, which includes a zero/sign-extend of the 2nd
    // source register. Implementing this variant instead of using a separate instruction for
    // the extension might have performance benefits for some BPF instructions.

    #[must_use]
    pub fn add(size: OperandSize, src1: u8, src2: u8, destination: u8) -> Self {
        Self::AddSubRegister(ARM64InstructionLogicalShiftedRegister {
            size,
            opcode: 0,
            n: 0,
            dest: destination,
            src1,
            src2,
            ..ARM64InstructionLogicalShiftedRegister::default()
        })
    }

    #[must_use]
    pub fn add_imm(size: OperandSize, src: u8, imm12: u16, destination: u8) -> Self {
        debug_assert!(imm12 < (1u16 << 12));
        Self::AddSubImm(ARM64InstructionAddSubImm {
            size,
            opcode: 0,
            dest: destination,
            src,
            imm12,
            ..ARM64InstructionAddSubImm::default()
        })
    }

    // destination -= source
    #[must_use]
    pub fn sub(size: OperandSize, src1: u8, src2: u8, destination: u8) -> Self {
        Self::AddSubRegister(ARM64InstructionLogicalShiftedRegister {
            size,
            opcode: 2,
            n: 0,
            dest: destination,
            src1,
            src2,
            ..ARM64InstructionLogicalShiftedRegister::default()
        })
    }

    #[must_use]
    pub fn sub_imm(size: OperandSize, src: u8, imm12: u16, destination: u8) -> Self {
        debug_assert!(imm12 < (1u16 << 12));
        Self::AddSubImm(ARM64InstructionAddSubImm {
            size,
            opcode: 1,
            dest: destination,
            src,
            imm12,
            ..ARM64InstructionAddSubImm::default()
        })
    }

    // destination <=> source
    #[must_use]
    pub fn cmp(size: OperandSize, source: u8, destination: u8) -> Self {
        Self::AddSubRegister(ARM64InstructionLogicalShiftedRegister {
            size,
            opcode: 3,
            n: 0,
            dest: SP_XZR,
            src1: destination,
            src2: source,
            ..ARM64InstructionLogicalShiftedRegister::default()
        })
    }

    #[must_use]
    pub fn cmp_imm(size: OperandSize, src: u8, imm12: u16) -> Self {
        debug_assert!(imm12 < (1u16 << 12));
        Self::AddSubImm(ARM64InstructionAddSubImm {
            size,
            opcode: 1,
            sets_flags: 1,
            dest: SP_XZR,
            src,
            imm12,
            ..ARM64InstructionAddSubImm::default()
        })
    }

    #[must_use]
    pub fn zero_extend_to_u64(from_size: OperandSize, source: u8, destination: u8) -> Self {
        match from_size {
            // UXTB
            OperandSize::S8 => Self::BitfieldImm(ARM64InstructionLogicalImm {
                size: OperandSize::S32,
                opcode: 2,
                immr: 0,
                imms: 7,
                dest: destination,
                src: source,
                ..ARM64InstructionLogicalImm::default()
            }),
            // UXTH
            OperandSize::S16 => Self::BitfieldImm(ARM64InstructionLogicalImm {
                size: OperandSize::S32,
                opcode: 2,
                immr: 0,
                imms: 15,
                dest: destination,
                src: source,
                ..ARM64InstructionLogicalImm::default()
            }),
            OperandSize::S32 => Self::mov(OperandSize::S32, source, destination), // 32-bit ops clear the upper bits
            OperandSize::S0 | OperandSize::S64 => {
                panic!("zero_extend is only valid on S8, S16, and S32")
            }
        }
    }

    #[must_use]
    pub fn sign_extend_to_i64(from_size: OperandSize, source: u8, destination: u8) -> Self {
        match from_size {
            // SXTB
            OperandSize::S8 => Self::BitfieldImm(ARM64InstructionLogicalImm {
                size: OperandSize::S64,
                n: 1,
                opcode: 0,
                immr: 0,
                imms: 7,
                dest: destination,
                src: source,
            }),
            // SXTH
            OperandSize::S16 => Self::BitfieldImm(ARM64InstructionLogicalImm {
                size: OperandSize::S64,
                n: 1,
                opcode: 0,
                immr: 0,
                imms: 15,
                dest: destination,
                src: source,
            }),
            // SXTW
            OperandSize::S32 => Self::BitfieldImm(ARM64InstructionLogicalImm {
                size: OperandSize::S64,
                n: 1,
                opcode: 0,
                immr: 0,
                imms: 31,
                dest: destination,
                src: source,
            }),
            OperandSize::S0 | OperandSize::S64 => {
                panic!("zero_extend is only valid on S8, S16, and S32")
            }
        }
    }

    #[must_use]
    pub fn lsl_imm(source: u8, shift_imm: u8, destination: u8) -> Self {
        debug_assert!(shift_imm > 0 && shift_imm < 64);
        Self::BitfieldImm(ARM64InstructionLogicalImm {
            size: OperandSize::S64,
            n: 1,
            opcode: 2,
            immr: (-(shift_imm as i8)).rem_euclid(64) as u8,
            imms: 63 - shift_imm,
            dest: destination,
            src: source,
        })
    }

    #[must_use]
    pub fn lsl_reg(size: OperandSize, src: u8, shift: u8, destination: u8) -> Self {
        Self::DataProcessing2Src(ARM64InstructionDataProcessing {
            size,
            opcode: 0b001000,
            dest: destination,
            src1: src,
            src2: shift,
            ..ARM64InstructionDataProcessing::default()
        })
    }

    #[must_use]
    pub fn lsr_imm(source: u8, shift_imm: u8, destination: u8) -> Self {
        debug_assert!(shift_imm > 0 && shift_imm < 64);
        Self::BitfieldImm(ARM64InstructionLogicalImm {
            size: OperandSize::S64,
            n: 1,
            opcode: 2,
            immr: shift_imm,
            imms: 0b111111,
            dest: destination,
            src: source,
        })
    }

    #[must_use]
    pub fn lsr_reg(size: OperandSize, src: u8, shift: u8, destination: u8) -> Self {
        Self::DataProcessing2Src(ARM64InstructionDataProcessing {
            size,
            opcode: 0b001001,
            dest: destination,
            src1: src,
            src2: shift,
            ..ARM64InstructionDataProcessing::default()
        })
    }

    #[must_use]
    pub fn asr_reg(size: OperandSize, src: u8, shift: u8, destination: u8) -> Self {
        Self::DataProcessing2Src(ARM64InstructionDataProcessing {
            size,
            opcode: 0b001010,
            dest: destination,
            src1: src,
            src2: shift,
            ..ARM64InstructionDataProcessing::default()
        })
    }

    #[must_use]
    pub fn rev(size: OperandSize, src: u8, destination: u8) -> Self {
        Self::DataProcessing1Src(ARM64InstructionDataProcessing {
            size,
            opcode: match size {
                OperandSize::S16 => 0b1,
                OperandSize::S32 => 0b10,
                OperandSize::S64 => 0b11,
                _ => panic!("bad operand size for rev"),
            },
            dest: destination,
            src1: src,
            src2: 0,
            ..ARM64InstructionDataProcessing::default()
        })
    }
    // conditional branch
    // WARNING: You need to divide the byte offset by 4 before passing as imm19
    #[must_use]
    pub fn b_cond(cond: Condition, imm19: i32) -> Self {
        Self::ConditionalBranch(ARM64InstructionConditonalBranch {
            cond: cond as u8,
            imm19,
        })
    }

    // call
    #[must_use]
    pub fn bl(imm26: i32) -> Self {
        Self::BranchImm26(ARM64InstructionImm26 {
            opcode: 0b100101,
            imm26,
        })
    }

    // call
    #[must_use]
    pub fn blr(target: u8) -> Self {
        Self::BLR(ARM64InstructionBLR { target })
    }

    // jump
    #[must_use]
    pub fn b(imm26: i32) -> Self {
        Self::BranchImm26(ARM64InstructionImm26 {
            opcode: 0b000101,
            imm26,
        })
    }

    #[must_use]
    pub fn ret() -> Self {
        Self::RET
    }

    // movk (64-bit)
    #[must_use]
    pub fn movk(destination: u8, shift_16: u8, immediate: u16) -> Self {
        debug_assert!((0..4).contains(&shift_16));
        Self::MovWideImm(ARM64InstructionWideImm {
            size: OperandSize::S64,
            dest: destination,
            hw: shift_16,
            imm16: immediate,
            opcode: 3,
        })
    }

    // movn (64-bit)
    #[must_use]
    pub fn movn(destination: u8, shift_16: u8, immediate: u16) -> Self {
        debug_assert!((0..4).contains(&shift_16));
        Self::MovWideImm(ARM64InstructionWideImm {
            size: OperandSize::S64,
            dest: destination,
            hw: shift_16,
            imm16: immediate,
            opcode: 0,
        })
    }

    // mvn (bitwise NOT)
    #[must_use]
    pub fn mvn(size: OperandSize, source: u8, destination: u8) -> Self {
        Self::LogicalRegister(ARM64InstructionLogicalShiftedRegister {
            size,
            opcode: 1,
            n: 1,
            dest: destination,
            src1: SP_XZR,
            src2: source,
            ..ARM64InstructionLogicalShiftedRegister::default()
        })
    }

    /// Load data from [source + offset]
    #[must_use]
    pub fn load(size: OperandSize, source: u8, indirect: ARM64MemoryOperand, data: u8) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0);
        match indirect {
            ARM64MemoryOperand::OffsetPreIndex(_) | ARM64MemoryOperand::OffsetPostIndex(_) => {
                // in arm64, loads with writeback to the base register cannot also use this
                // register as the dest
                debug_assert_ne!(source, data);
            }
            _ => {}
        }
        Self::Load(ARM64InstructionLoadStore {
            size,
            data,
            base: source,
            mem: indirect,
        })
    }

    #[must_use]
    pub fn store(size: OperandSize, data: u8, source: u8, indirect: ARM64MemoryOperand) -> Self {
        exclude_operand_sizes!(size, OperandSize::S0);
        match indirect {
            ARM64MemoryOperand::OffsetPreIndex(_) | ARM64MemoryOperand::OffsetPostIndex(_) => {
                // in arm64, loads with writeback to the base register cannot also use this
                // register as the dest
                debug_assert_ne!(source, data);
            }
            _ => {}
        }
        Self::Store(ARM64InstructionLoadStore {
            size,
            data,
            base: source,
            mem: indirect,
        })
    }

    // Important: We have to maintain 16-byte SP alignment (enforced in hardware, at least on Apple
    // platforms). To allow for minimal changes from the x86 code, we still want an 8-byte push,
    // but the trade-off is that we have to allocate 16 bytes on the stack for every 8 byte push.
    //
    // A more efficient approach in many circumstances is to manually move SP down and load/store
    // as desired.
    #[must_use]
    pub fn push64(reg: u8) -> Self {
        debug_assert_ne!(SP_XZR, reg);
        Self::Store(ARM64InstructionLoadStore {
            size: OperandSize::S64,
            data: reg,
            base: SP_XZR, // this is SP in this context
            mem: ARM64MemoryOperand::OffsetPreIndex(-16),
        })
    }

    #[must_use]
    pub fn pop64(reg: u8) -> Self {
        debug_assert_ne!(SP_XZR, reg);
        Self::Load(ARM64InstructionLoadStore {
            size: OperandSize::S64,
            data: reg,
            base: SP_XZR, // this is SP in this context
            mem: ARM64MemoryOperand::OffsetPostIndex(16),
        })
    }

    // multiply-add
    #[must_use]
    pub fn madd(size: OperandSize, src1: u8, src2: u8, src3: u8, destination: u8) -> Self {
        Self::DataProcessing3Src(ARM64InstructionDataProcessing {
            size,
            opcode: 0b000,
            dest: destination,
            src1,
            src2,
            src3,
            ..ARM64InstructionDataProcessing::default()
        })
    }

    // multiply-sub
    #[must_use]
    pub fn msub(size: OperandSize, src1: u8, src2: u8, src3: u8, destination: u8) -> Self {
        Self::DataProcessing3Src(ARM64InstructionDataProcessing {
            size,
            opcode: 0b000,
            dest: destination,
            src1,
            src2,
            src3,
            o0: 1,
        })
    }

    #[must_use]
    pub fn udiv(size: OperandSize, src1: u8, src2: u8, destination: u8) -> Self {
        Self::DataProcessing2Src(ARM64InstructionDataProcessing {
            size,
            opcode: 0b000010,
            dest: destination,
            src1,
            src2,
            ..ARM64InstructionDataProcessing::default()
        })
    }

    #[must_use]
    pub fn sdiv(size: OperandSize, src1: u8, src2: u8, destination: u8) -> Self {
        Self::DataProcessing2Src(ARM64InstructionDataProcessing {
            size,
            opcode: 0b000011,
            dest: destination,
            src1,
            src2,
            ..ARM64InstructionDataProcessing::default()
        })
    }
}
