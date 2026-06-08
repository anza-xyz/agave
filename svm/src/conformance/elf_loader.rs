//! ELF loader conformance harness.

use {
    crate::conformance::{
        fd_hash::{fd_hash_u64_without_seed, fd_hash_without_seed},
        feature_set::feature_set_from_proto,
    },
    prost::Message,
    protosol::protos::{
        ElfLoaderCtx as ProtoElfLoaderCtx, ElfLoaderEffects as ProtoElfLoaderEffects,
    },
    solana_compute_budget::compute_budget::ComputeBudget,
    solana_program_runtime::solana_sbpf::{
        ebpf,
        elf::{ElfError, Executable},
    },
    solana_syscalls::create_program_runtime_environment,
    std::{collections::BTreeSet, ffi::c_int},
};

pub fn execute_elf_loader(input: &ProtoElfLoaderCtx) -> ProtoElfLoaderEffects {
    let feature_set = input
        .features
        .as_ref()
        .map(feature_set_from_proto)
        .unwrap_or_default()
        .runtime_features();
    let simd_0268_active = feature_set.raise_cpi_nesting_limit_to_8;
    let compute_budget = ComputeBudget::new_with_defaults(simd_0268_active);

    let program_runtime_environment = create_program_runtime_environment(
        &feature_set,
        &compute_budget.to_budget(),
        input.deploy_checks,
        std::env::var("ENABLE_VM_TRACING").is_ok(),
    )
    .unwrap();

    let executable = match Executable::load(&input.elf_data, (*program_runtime_environment).clone())
    {
        Ok(executable) => executable,
        Err(err) => {
            return ProtoElfLoaderEffects {
                err_code: elf_err_to_num(&err) as u32,
                ..Default::default()
            };
        }
    };

    let (text_vaddr, text_bytes) = executable.get_text_bytes();
    let calldests: Vec<u64> = executable
        .get_function_registry()
        .iter()
        .map(|(_, (_, address))| address as u64)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    ProtoElfLoaderEffects {
        err_code: 0,
        rodata_hash: fd_hash_without_seed(executable.get_ro_section()),
        entry_pc: executable.get_entrypoint_instruction_offset() as u64,
        text_off: text_vaddr.saturating_sub(ebpf::MM_BYTECODE_START),
        text_cnt: (text_bytes.len() / 8) as u64,
        calldests_hash: fd_hash_u64_without_seed(&calldests),
    }
}

fn elf_err_to_num(error: &ElfError) -> u8 {
    match error {
        ElfError::FailedToParse(_) => 1,
        ElfError::EntrypointOutOfBounds => 2,
        ElfError::InvalidEntrypoint => 3,
        ElfError::FailedToGetSection(_) => 4,
        ElfError::UnresolvedSymbol(_, _, _) => 5,
        ElfError::SectionNotFound(_) => 6,
        ElfError::RelativeJumpOutOfBounds(_) => 7,
        ElfError::SymbolHashCollision(_) => 8,
        ElfError::WrongEndianess => 9,
        ElfError::WrongAbi => 10,
        ElfError::WrongMachine => 11,
        ElfError::WrongClass => 12,
        ElfError::NotOneTextSection => 13,
        ElfError::WritableSectionNotSupported(_) => 14,
        ElfError::AddressOutsideLoadableSection(_) => 15,
        ElfError::InvalidVirtualAddress(_) => 16,
        ElfError::UnknownRelocation(_) => 17,
        ElfError::FailedToReadRelocationInfo => 18,
        ElfError::WrongType => 19,
        ElfError::UnknownSymbol(_) => 20,
        ElfError::ValueOutOfBounds => 21,
        ElfError::UnsupportedSBPFVersion => 22,
        ElfError::InvalidProgramHeader => 23,
    }
}

/// # Safety
///
/// `in_ptr` must point to `in_sz` initialized bytes. `out_ptr` must point
/// to a writable buffer of at least `*out_psz` bytes. On return, `*out_psz`
/// is updated to the number of bytes written.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sol_compat_elf_loader_v1(
    out_ptr: *mut u8,
    out_psz: *mut u64,
    in_ptr: *mut u8,
    in_sz: u64,
) -> c_int {
    if in_ptr.is_null() || in_sz == 0 {
        return 0;
    }
    if out_psz.is_null() || out_ptr.is_null() {
        return 0;
    }
    let in_slice = unsafe { std::slice::from_raw_parts(in_ptr, in_sz as usize) };
    let Ok(ctx) = ProtoElfLoaderCtx::decode(in_slice) else {
        return 0;
    };

    let effects = execute_elf_loader(&ctx);

    let out_slice = unsafe { std::slice::from_raw_parts_mut(out_ptr, (*out_psz) as usize) };
    let out_vec = effects.encode_to_vec();
    if out_vec.len() > out_slice.len() {
        return 0;
    }
    out_slice[..out_vec.len()].copy_from_slice(&out_vec);
    unsafe { *out_psz = out_vec.len() as u64 };

    1
}

#[cfg(test)]
mod tests {
    use {
        super::*, agave_feature_set::enable_sbpf_v3_deployment_and_execution,
        protosol::protos::FeatureSet as ProtoFeatureSet,
    };

    const NOOP_ALIGNED: &[u8] =
        include_bytes!("../../../programs/bpf_loader/test_elfs/out/noop_aligned.so");
    const NOOP_UNALIGNED: &[u8] =
        include_bytes!("../../../programs/bpf_loader/test_elfs/out/noop_unaligned.so");
    const SBPFV3_RETURN_OK: &[u8] =
        include_bytes!("../../../programs/bpf_loader/test_elfs/out/sbpfv3_return_ok.so");

    fn assert_loads_ok(elf: &[u8], deploy_checks: bool, features: Option<ProtoFeatureSet>) {
        let effects = execute_elf_loader(&ProtoElfLoaderCtx {
            features,
            elf_data: elf.to_vec(),
            deploy_checks,
        });
        assert_eq!(effects.err_code, 0);
        assert!(effects.text_cnt > 0);
    }

    #[test]
    fn test_load_noop_aligned() {
        assert_loads_ok(NOOP_ALIGNED, false, None);
    }

    #[test]
    fn test_load_noop_unaligned_with_deploy_checks() {
        assert_loads_ok(NOOP_UNALIGNED, true, None);
    }

    #[test]
    fn test_load_sbpf_v3_with_feature_enabled() {
        // The v3 ELF only loads when the SBPF v3 feature is active.
        let v3 = enable_sbpf_v3_deployment_and_execution::id();
        let features = ProtoFeatureSet {
            features: vec![u64::from_le_bytes(v3.to_bytes()[..8].try_into().unwrap())],
        };
        assert_loads_ok(SBPFV3_RETURN_OK, false, Some(features));
    }
}
