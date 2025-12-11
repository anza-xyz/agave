//! Syscall context (input).

use crate::{instr_context::InstrContext, vm_context::VmContext};

#[derive(Debug, Default, Clone)]
pub struct SyscallInvocation {
    pub function_name: Vec<u8>,
    pub heap_prefix: Vec<u8>,
    pub stack_prefix: Vec<u8>,
}

pub struct SyscallContext {
    pub vm_ctx: VmContext,
    pub instr_ctx: InstrContext,
    pub syscall_invocation: SyscallInvocation,
}

#[cfg(feature = "fuzz")]
use crate::{
    error::FixtureError,
    proto::{SyscallContext as ProtoSyscallContext, SyscallInvocation as ProtoSyscallInvocation},
};

#[cfg(feature = "fuzz")]
impl TryFrom<ProtoSyscallContext> for SyscallContext {
    type Error = FixtureError;

    fn try_from(value: ProtoSyscallContext) -> Result<Self, Self::Error> {
        let vm_ctx = value
            .vm_ctx
            .map(Into::into)
            .ok_or(FixtureError::InvalidFixtureInput)?;
        let instr_ctx = value
            .instr_ctx
            .map(TryInto::try_into)
            .transpose()?
            .ok_or(FixtureError::InvalidFixtureInput)?;
        let syscall_invocation = value.syscall_invocation.map(Into::into).unwrap_or_default();

        Ok(Self {
            vm_ctx,
            instr_ctx,
            syscall_invocation,
        })
    }
}

#[cfg(feature = "fuzz")]
impl From<ProtoSyscallInvocation> for SyscallInvocation {
    fn from(value: ProtoSyscallInvocation) -> Self {
        Self {
            function_name: value.function_name,
            heap_prefix: value.heap_prefix,
            stack_prefix: value.stack_prefix,
        }
    }
}
