//! Syscall effects (output).

#[derive(Debug, Default, Clone)]
pub struct InputDataRegion {
    pub offset: u64,
    pub content: Vec<u8>,
    pub is_writable: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ErrKind {
    #[default]
    Unspecified = 0,
    Ebpf = 1,
    Syscall = 2,
    Instruction = 3,
}

#[derive(Debug, Default, Clone)]
pub struct SyscallEffects {
    pub error: i64,
    pub error_kind: ErrKind,
    pub r0: u64,
    pub cu_avail: u64,
    pub heap: Vec<u8>,
    pub stack: Vec<u8>,
    pub inputdata: Vec<u8>,
    pub input_data_regions: Vec<InputDataRegion>,
    pub frame_count: u64,
    pub log: Vec<u8>,
    pub rodata: Vec<u8>,
    pub pc: u64,
    pub r1: u64,
    pub r2: u64,
    pub r3: u64,
    pub r4: u64,
    pub r5: u64,
    pub r6: u64,
    pub r7: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
}

#[cfg(feature = "fuzz")]
use crate::proto::{
    InputDataRegion as ProtoInputDataRegion, SyscallEffects as ProtoSyscallEffects,
};

#[cfg(feature = "fuzz")]
impl From<SyscallEffects> for ProtoSyscallEffects {
    fn from(value: SyscallEffects) -> Self {
        Self {
            error: value.error,
            error_kind: value.error_kind as i32,
            r0: value.r0,
            cu_avail: value.cu_avail,
            heap: value.heap,
            stack: value.stack,
            inputdata: value.inputdata,
            input_data_regions: value
                .input_data_regions
                .into_iter()
                .map(Into::into)
                .collect(),
            frame_count: value.frame_count,
            log: value.log,
            rodata: value.rodata,
            pc: value.pc,
            r1: value.r1,
            r2: value.r2,
            r3: value.r3,
            r4: value.r4,
            r5: value.r5,
            r6: value.r6,
            r7: value.r7,
            r8: value.r8,
            r9: value.r9,
            r10: value.r10,
        }
    }
}

#[cfg(feature = "fuzz")]
impl From<InputDataRegion> for ProtoInputDataRegion {
    fn from(value: InputDataRegion) -> Self {
        Self {
            offset: value.offset,
            content: value.content,
            is_writable: value.is_writable,
        }
    }
}
