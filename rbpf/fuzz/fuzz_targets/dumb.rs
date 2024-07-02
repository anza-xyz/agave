#![no_main]

use {
    crate::common::ConfigTemplate,
    libfuzzer_sys::fuzz_target,
    solana_rbpf::{
        create_vm, ebpf,
        elf::Executable,
        memory_region::MemoryRegion,
        program::{BuiltinProgram, FunctionRegistry, SBPFVersion},
        verifier::{RequisiteVerifier, Verifier},
        vm::TestContextObject,
    },
    std::hint::black_box,
};

mod common;

#[derive(arbitrary::Arbitrary, Debug)]
struct DumbFuzzData {
    template: ConfigTemplate,
    prog: Vec<u8>,
    mem: Vec<u8>,
}

fuzz_target!(|data: DumbFuzzData| {
    let prog = data.prog;
    let config = data.template.into();
    let function_registry = FunctionRegistry::default();
    if RequisiteVerifier::verify(&prog, &config, &SBPFVersion::V2, &function_registry).is_err() {
        // verify please
        return;
    }
    let mut mem = data.mem;
    let executable = Executable::<TestContextObject>::from_text_bytes(
        &prog,
        std::sync::Arc::new(BuiltinProgram::new_loader(
            config,
            FunctionRegistry::default(),
        )),
        SBPFVersion::V2,
        function_registry,
    )
    .unwrap();
    let mem_region = MemoryRegion::new_writable(&mut mem, ebpf::MM_INPUT_START);
    let mut context_object = TestContextObject::new(29);
    create_vm!(
        interp_vm,
        &executable,
        &mut context_object,
        stack,
        heap,
        vec![mem_region],
        None
    );

    let (_interp_ins_count, interp_res) = interp_vm.execute_program(&executable, true);
    drop(black_box(interp_res));
});
