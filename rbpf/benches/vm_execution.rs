// Copyright 2020 Solana Maintainers <maintainers@solana.com>
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

#![feature(test)]

extern crate solana_rbpf;
extern crate test;

#[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
use solana_rbpf::{ebpf, memory_region::MemoryRegion, program::FunctionRegistry, vm::Config};
use {
    solana_rbpf::{
        create_vm, elf::Executable, program::BuiltinProgram, verifier::RequisiteVerifier,
        vm::TestContextObject,
    },
    std::{fs::File, io::Read, sync::Arc},
    test::Bencher,
};

#[bench]
fn bench_init_interpreter_start(bencher: &mut Bencher) {
    let mut file = File::open("tests/elfs/rodata_section.so").unwrap();
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    let executable =
        Executable::<TestContextObject>::from_elf(&elf, Arc::new(BuiltinProgram::new_mock()))
            .unwrap();
    executable.verify::<RequisiteVerifier>().unwrap();
    let mut context_object = TestContextObject::default();
    create_vm!(
        vm,
        &executable,
        &mut context_object,
        stack,
        heap,
        Vec::new(),
        None
    );
    bencher.iter(|| {
        vm.context_object_pointer.remaining = 37;
        vm.execute_program(&executable, true).1.unwrap()
    });
}

#[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
#[bench]
fn bench_init_jit_start(bencher: &mut Bencher) {
    let mut file = File::open("tests/elfs/rodata_section.so").unwrap();
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    let mut executable =
        Executable::<TestContextObject>::from_elf(&elf, Arc::new(BuiltinProgram::new_mock()))
            .unwrap();
    executable.verify::<RequisiteVerifier>().unwrap();
    executable.jit_compile().unwrap();
    let mut context_object = TestContextObject::default();
    create_vm!(
        vm,
        &executable,
        &mut context_object,
        stack,
        heap,
        Vec::new(),
        None
    );
    bencher.iter(|| {
        vm.context_object_pointer.remaining = 37;
        vm.execute_program(&executable, false).1.unwrap()
    });
}

#[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
fn bench_jit_vs_interpreter(
    bencher: &mut Bencher,
    assembly: &str,
    config: Config,
    instruction_meter: u64,
    mem: &mut [u8],
) {
    let mut executable = solana_rbpf::assembler::assemble::<TestContextObject>(
        assembly,
        Arc::new(BuiltinProgram::new_loader(
            config,
            FunctionRegistry::default(),
        )),
    )
    .unwrap();
    executable.verify::<RequisiteVerifier>().unwrap();
    executable.jit_compile().unwrap();
    let mut context_object = TestContextObject::default();
    let mem_region = MemoryRegion::new_writable(mem, ebpf::MM_INPUT_START);
    create_vm!(
        vm,
        &executable,
        &mut context_object,
        stack,
        heap,
        vec![mem_region],
        None
    );
    let interpreter_summary = bencher
        .bench(|bencher| {
            bencher.iter(|| {
                vm.context_object_pointer.remaining = instruction_meter;
                let (instruction_count_interpreter, result) = vm.execute_program(&executable, true);
                assert!(result.is_ok(), "{:?}", result);
                assert_eq!(instruction_count_interpreter, instruction_meter);
            });
            Ok(())
        })
        .unwrap()
        .unwrap();
    let jit_summary = bencher
        .bench(|bencher| {
            bencher.iter(|| {
                vm.context_object_pointer.remaining = instruction_meter;
                let (instruction_count_jit, result) = vm.execute_program(&executable, false);
                assert!(result.is_ok(), "{:?}", result);
                assert_eq!(instruction_count_jit, instruction_meter);
            });
            Ok(())
        })
        .unwrap()
        .unwrap();
    println!(
        "jit_vs_interpreter_ratio={}",
        interpreter_summary.mean / jit_summary.mean
    );
}

#[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
#[bench]
fn bench_jit_vs_interpreter_address_translation(bencher: &mut Bencher) {
    bench_jit_vs_interpreter(
        bencher,
        "
    ldxb r0, [r1]
    add r1, 1
    mov r0, r1
    and r0, 0xFFFFFF
    jlt r0, 0x20000, -5
    exit",
        Config::default(),
        655361,
        &mut [0; 0x20000],
    );
}

#[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
static ADDRESS_TRANSLATION_STACK_CODE: &str = "
    mov r1, r2
    and r1, 4095
    mov r3, r10
    sub r3, r1
    add r3, -1
    ldxb r4, [r3]
    add r2, 1
    jlt r2, 0x10000, -8
    exit";

#[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
#[bench]
fn bench_jit_vs_interpreter_address_translation_stack_fixed(bencher: &mut Bencher) {
    bench_jit_vs_interpreter(
        bencher,
        ADDRESS_TRANSLATION_STACK_CODE,
        Config {
            enable_sbpf_v2: false,
            ..Config::default()
        },
        524289,
        &mut [],
    );
}

#[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
#[bench]
fn bench_jit_vs_interpreter_address_translation_stack_dynamic(bencher: &mut Bencher) {
    bench_jit_vs_interpreter(
        bencher,
        ADDRESS_TRANSLATION_STACK_CODE,
        Config {
            enable_sbpf_v2: true,
            ..Config::default()
        },
        524289,
        &mut [],
    );
}

#[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
#[bench]
fn bench_jit_vs_interpreter_empty_for_loop(bencher: &mut Bencher) {
    bench_jit_vs_interpreter(
        bencher,
        "
    mov r1, r2
    and r1, 1023
    add r2, 1
    jlt r2, 0x10000, -4
    exit",
        Config::default(),
        262145,
        &mut [0; 0],
    );
}

#[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
#[bench]
fn bench_jit_vs_interpreter_call_depth_fixed(bencher: &mut Bencher) {
    bench_jit_vs_interpreter(
        bencher,
        "
    mov r6, 0
    add r6, 1
    mov r1, 18
    call function_foo
    jlt r6, 1024, -4
    exit
    function_foo:
    stw [r10-4], 0x11223344
    mov r6, r1
    jgt r6, 0, +1
    exit
    mov r1, r6
    add r1, -1
    call function_foo
    exit",
        Config {
            enable_sbpf_v2: false,
            ..Config::default()
        },
        137218,
        &mut [],
    );
}

#[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
#[bench]
fn bench_jit_vs_interpreter_call_depth_dynamic(bencher: &mut Bencher) {
    bench_jit_vs_interpreter(
        bencher,
        "
    mov r6, 0
    add r6, 1
    mov r1, 18
    call function_foo
    jlt r6, 1024, -4
    exit
    function_foo:
    add r11, -4
    stw [r10-4], 0x11223344
    mov r6, r1
    jeq r6, 0, +3
    mov r1, r6
    add r1, -1
    call function_foo
    add r11, 4
    exit",
        Config {
            enable_sbpf_v2: true,
            ..Config::default()
        },
        176130,
        &mut [],
    );
}
