//! VM context for syscall execution.

use solana_pubkey::Pubkey;

#[derive(Debug, Default, Clone)]
pub struct ReturnData {
    pub program_id: Pubkey,
    pub data: Vec<u8>,
}

#[derive(Debug, Default, Clone)]
pub struct VmContext {
    pub heap_max: u64,
    pub rodata: Vec<u8>,
    pub rodata_text_section_offset: u64,
    pub rodata_text_section_length: u64,
    pub r0: u64,
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
    pub r11: u64,
    pub entry_pc: u64,
    pub call_whitelist: Vec<u8>,
    pub tracing_enabled: bool,
    pub return_data: Option<ReturnData>,
    pub sbpf_version: u32,
}

#[cfg(feature = "fuzz")]
use crate::proto::VmContext as ProtoVmContext;

#[cfg(feature = "fuzz")]
impl From<ProtoVmContext> for VmContext {
    fn from(value: ProtoVmContext) -> Self {
        Self {
            heap_max: value.heap_max,
            rodata: value.rodata,
            rodata_text_section_offset: value.rodata_text_section_offset,
            rodata_text_section_length: value.rodata_text_section_length,
            r0: value.r0,
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
            r11: value.r11,
            entry_pc: value.entry_pc,
            call_whitelist: value.call_whitelist,
            tracing_enabled: value.tracing_enabled,
            return_data: value.return_data.map(|rd| ReturnData {
                program_id: Pubkey::try_from(rd.program_id.as_slice()).unwrap_or_default(),
                data: rd.data,
            }),
            sbpf_version: value.sbpf_version,
        }
    }
}
