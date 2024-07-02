#![no_main]

use {
    crate::common::ConfigTemplate,
    libfuzzer_sys::fuzz_target,
    semantic_aware::*,
    solana_rbpf::{
        create_vm, ebpf,
        elf::Executable,
        insn_builder::IntoBytes,
        memory_region::MemoryRegion,
        program::{BuiltinProgram, FunctionRegistry, SBPFVersion},
        verifier::{RequisiteVerifier, Verifier},
        vm::TestContextObject,
    },
};

mod common;
mod semantic_aware;

#[derive(arbitrary::Arbitrary, Debug)]
struct FuzzData {
    template: ConfigTemplate,
    prog: FuzzProgram,
    mem: Vec<u8>,
}

fuzz_target!(|data: FuzzData| {
    let prog = make_program(&data.prog);
    let config = data.template.into();
    let function_registry = FunctionRegistry::default();
    if RequisiteVerifier::verify(
        prog.into_bytes(),
        &config,
        &SBPFVersion::V2,
        &function_registry,
    )
    .is_err()
    {
        // verify please
        return;
    }

    #[allow(unused_mut)]
    let mut executable = Executable::<TestContextObject>::from_text_bytes(
        prog.into_bytes(),
        std::sync::Arc::new(BuiltinProgram::new_loader(
            config,
            FunctionRegistry::default(),
        )),
        SBPFVersion::V2,
        function_registry,
    )
    .unwrap();
    let mut interp_mem = data.mem.clone();
    let mut interp_context_object = TestContextObject::new(1 << 16);
    let interp_mem_region = MemoryRegion::new_writable(&mut interp_mem, ebpf::MM_INPUT_START);
    create_vm!(
        interp_vm,
        &executable,
        &mut interp_context_object,
        interp_stack,
        interp_heap,
        vec![interp_mem_region],
        None
    );
    #[allow(unused)]
    let (_interp_ins_count, interp_res) = interp_vm.execute_program(&executable, true);

    #[cfg(all(not(target_os = "windows"), target_arch = "x86_64"))]
    if executable.jit_compile().is_ok() {
        let mut jit_mem = data.mem;
        let mut jit_context_object = TestContextObject::new(1 << 16);
        let jit_mem_region = MemoryRegion::new_writable(&mut jit_mem, ebpf::MM_INPUT_START);
        create_vm!(
            jit_vm,
            &executable,
            &mut jit_context_object,
            jit_stack,
            jit_heap,
            vec![jit_mem_region],
            None
        );
        let (_jit_ins_count, jit_res) = jit_vm.execute_program(&executable, false);
        if format!("{:?}", interp_res) != format!("{:?}", jit_res) {
            // spot check: there's a meaningless bug where ExceededMaxInstructions is different due to jump calculations
            if format!("{:?}", interp_res).contains("ExceededMaxInstructions")
                && format!("{:?}", jit_res).contains("ExceededMaxInstructions")
            {
                return;
            }
            eprintln!("{:#?}", &data.prog);
            panic!("Expected {:?}, but got {:?}", interp_res, jit_res);
        }
        if interp_res.is_ok() {
            // we know jit res must be ok if interp res is by this point
            if interp_context_object.remaining != jit_context_object.remaining {
                panic!(
                    "Expected {} insts remaining, but got {}",
                    interp_context_object.remaining, jit_context_object.remaining
                );
            }
            if interp_mem != jit_mem {
                panic!(
                    "Expected different memory. From interpreter: {:?}\nFrom JIT: {:?}",
                    interp_mem, jit_mem
                );
            }
        }
    }
});
