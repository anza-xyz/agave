//! Fuzz harness for syscall execution.

#![allow(clippy::missing_safety_doc)]

use {
    crate::execute_vm_syscall,
    prost::Message,
    solana_svm_test_harness_fixture::{
        proto::{SyscallContext as ProtoSyscallContext, SyscallEffects as ProtoSyscallEffects},
        syscall_context::SyscallContext,
    },
    std::ffi::c_int,
};

pub fn execute_syscall_proto(input: ProtoSyscallContext) -> Option<ProtoSyscallEffects> {
    let Ok(syscall_context) = SyscallContext::try_from(input) else {
        return None;
    };

    execute_vm_syscall(syscall_context).map(Into::into)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_vm_syscall_execute_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    if in_ptr.is_null() || in_sz == 0 {
        return 0;
    }
    let in_slice = unsafe { std::slice::from_raw_parts(in_ptr, in_sz as usize) };
    let Ok(syscall_context) = ProtoSyscallContext::decode(in_slice) else {
        return 0;
    };
    let Some(syscall_effects) = execute_syscall_proto(syscall_context) else {
        return 0;
    };
    let out_slice = unsafe { std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize) };
    let out_vec = syscall_effects.encode_to_vec();
    if out_vec.len() > out_slice.len() {
        return 0;
    }
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    unsafe {
        *out_psz = out_vec.len() as u64;
    }
    1
}
