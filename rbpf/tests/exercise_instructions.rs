#![allow(clippy::arithmetic_side_effects)]
#![cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
// Copyright 2020 Solana Maintainers <maintainers@solana.com>
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

extern crate byteorder;
extern crate libc;
extern crate solana_rbpf;
extern crate thiserror;

use {
    rand::{rngs::SmallRng, RngCore, SeedableRng},
    solana_rbpf::{
        assembler::assemble,
        create_vm, ebpf,
        memory_region::MemoryRegion,
        program::{BuiltinFunction, BuiltinProgram, FunctionRegistry},
        static_analysis::Analysis,
        verifier::RequisiteVerifier,
        vm::{Config, ContextObject, TestContextObject},
    },
    std::sync::Arc,
};

macro_rules! test_interpreter_and_jit {
    (register, $function_registry:expr, $location:expr => $syscall_function:expr) => {
        $function_registry
            .register_function_hashed($location.as_bytes(), $syscall_function)
            .unwrap();
    };
    ($executable:expr, $mem:tt, $context_object:expr $(,)?) => {
        let expected_instruction_count = $context_object.get_remaining();
        #[allow(unused_mut)]
        let mut context_object = $context_object;
        $executable.verify::<RequisiteVerifier>().unwrap();
        let (
            instruction_count_interpreter,
            interpreter_final_pc,
            _tracer_interpreter,
            interpreter_result,
            interpreter_mem,
        ) = {
            let mut mem = $mem.clone();
            let mem_region = MemoryRegion::new_writable(&mut mem, ebpf::MM_INPUT_START);
            let mut context_object = context_object.clone();
            create_vm!(
                vm,
                &$executable,
                &mut context_object,
                stack,
                heap,
                vec![mem_region],
                None
            );
            let (instruction_count_interpreter, result) = vm.execute_program(&$executable, true);
            (
                instruction_count_interpreter,
                vm.registers[11],
                vm.context_object_pointer.clone(),
                result.unwrap(),
                mem,
            )
        };
        #[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
        {
            #[allow(unused_mut)]
            $executable.jit_compile().unwrap();
            let mut mem = $mem;
            let mem_region = MemoryRegion::new_writable(&mut mem, ebpf::MM_INPUT_START);
            create_vm!(
                vm,
                &$executable,
                &mut context_object,
                stack,
                heap,
                vec![mem_region],
                None
            );
            let (instruction_count_jit, result) = vm.execute_program(&$executable, false);
            let tracer_jit = &vm.context_object_pointer;
            if !TestContextObject::compare_trace_log(&_tracer_interpreter, tracer_jit) {
                let analysis = Analysis::from_executable(&$executable).unwrap();
                let stdout = std::io::stdout();
                analysis
                    .disassemble_trace_log(&mut stdout.lock(), &_tracer_interpreter.trace_log)
                    .unwrap();
                analysis
                    .disassemble_trace_log(&mut stdout.lock(), &tracer_jit.trace_log)
                    .unwrap();
                panic!();
            }
            assert_eq!(
                result.unwrap(),
                interpreter_result,
                "Unexpected result for JIT"
            );
            assert_eq!(
                instruction_count_interpreter, instruction_count_jit,
                "Interpreter and JIT instruction meter diverged",
            );
            assert_eq!(
                interpreter_final_pc, vm.registers[11],
                "Interpreter and JIT instruction final PC diverged",
            );
            assert_eq!(interpreter_mem, mem, "Interpreter and JIT memory diverged",);
        }
        if $executable.get_config().enable_instruction_meter {
            assert_eq!(
                instruction_count_interpreter, expected_instruction_count,
                "Instruction meter did not consume expected amount"
            );
        }
    };
}

macro_rules! test_interpreter_and_jit_asm {
    ($source:expr, $config:expr, $mem:tt, ($($location:expr => $syscall_function:expr),* $(,)?), $context_object:expr $(,)?) => {
        #[allow(unused_mut)]
        {
            let mut config = $config;
            config.enable_instruction_tracing = true;
            let mut function_registry = FunctionRegistry::<BuiltinFunction<TestContextObject>>::default();
            $(test_interpreter_and_jit!(register, function_registry, $location => $syscall_function);)*
            let loader = Arc::new(BuiltinProgram::new_loader(config, function_registry));
            let mut executable = assemble($source, loader).unwrap();
            test_interpreter_and_jit!(executable, $mem, $context_object);
        }
    };
}

// BPF_ALU : Arithmetic and Logic
#[test]
fn fuzz_alu() {
    let seed = 0xC2DB2F8F282284A0;
    let mut prng = SmallRng::seed_from_u64(seed);

    for src in 0..10 {
        for dst in 0..10 {
            for _ in 0..10 {
                test_ins(false, format!("mov64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("add64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("sub64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("or64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("xor64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("and64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("lmul64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("uhmul64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("shmul64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("udiv64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("urem64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("srem64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("sdiv64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("udiv64 r{src}, r{dst}"), &mut prng, 21);

                test_ins(false, format!("lsh64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("rsh64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("arsh64 r{src}, r{dst}"), &mut prng, 21);

                test_ins(false, format!("mov32 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("add32 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("sub32 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("or32 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("xor32 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("and32 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("lmul32 r{src}, r{dst}"), &mut prng, 21);
                // test_ins(format!("uhmul32 r{src}, r{dst}"), &mut p, 21rng);
                // test_ins(format!("shmul32 r{src}, r{dst}"), &mut p, 21rng);
                test_ins(false, format!("udiv32 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("sdiv32 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("srem32 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("urem32 r{src}, r{dst}"), &mut prng, 21);

                test_ins(false, format!("lsh32 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("rsh32 r{src}, r{dst}"), &mut prng, 21);
                test_ins(false, format!("arsh32 r{src}, r{dst}"), &mut prng, 21);

                test_ins(true, format!("mul64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(true, format!("mod64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(true, format!("div64 r{src}, r{dst}"), &mut prng, 21);
                test_ins(true, format!("mul32 r{src}, r{dst}"), &mut prng, 21);
                test_ins(true, format!("mod32 r{src}, r{dst}"), &mut prng, 21);
                test_ins(true, format!("div32 r{src}, r{dst}"), &mut prng, 21);

                // test load, store
                let rand = prng.next_u32() as i64;
                let offset = prng.next_u32() as i16;
                let addr = rand % 80 + 0x4_0000_0000i64 - offset as i64;
                let mut tmp = (src + 1) % 10;
                if dst == tmp {
                    tmp = (src + 2) % 10;
                }

                test_ins(
                    false,
                    format!(
                        "mov32 r{tmp},{}
                         mov64 r{src},{:#x}
                         lsh64 r{src},32
                         or64 r{src},r{tmp}
                         ldxb r{dst}, [r{src}{offset:+}]",
                        addr as i32,
                        addr >> 32,
                    ),
                    &mut prng,
                    25,
                );

                test_ins(
                    false,
                    format!(
                        "mov32 r{tmp},{}
                         mov64 r{src},{:#x}
                         lsh64 r{src},32
                         or64 r{src},r{tmp}
                         stxb [r{src}{offset:+}], r{dst}",
                        addr as i32,
                        addr >> 32,
                    ),
                    &mut prng,
                    25,
                );

                let addr = rand % 79 + 0x4_0000_0000i64 - offset as i64;

                test_ins(
                    false,
                    format!(
                        "mov32 r{tmp},{}
                         mov64 r{src},{:#x}
                         lsh64 r{src},32
                         or64 r{src},r{tmp}
                         ldxh r{dst}, [r{src}{offset:+}]",
                        addr as i32,
                        addr >> 32,
                    ),
                    &mut prng,
                    25,
                );

                test_ins(
                    false,
                    format!(
                        "mov32 r{tmp},{}
                         mov64 r{src},{:#x}
                         lsh64 r{src},32
                         or64 r{src},r{tmp}
                         stxh [r{src}{offset:+}], r{dst}",
                        addr as i32,
                        addr >> 32,
                    ),
                    &mut prng,
                    25,
                );

                let addr = rand % 77 + 0x4_0000_0000i64 - offset as i64;
                test_ins(
                    false,
                    format!(
                        "mov32 r{tmp},{}
                         mov64 r{src},{:#x}
                         lsh64 r{src},32
                         or64 r{src},r{tmp}
                         ldxw r{dst}, [r{src}{offset:+}]",
                        addr as i32,
                        addr >> 32,
                    ),
                    &mut prng,
                    25,
                );
                test_ins(
                    false,
                    format!(
                        "mov32 r{tmp},{}
                         mov64 r{src},{:#x}
                         lsh64 r{src},32
                         or64 r{src},r{tmp}
                         stxw [r{src}{offset:+}], r{dst}",
                        addr as i32,
                        addr >> 32,
                    ),
                    &mut prng,
                    25,
                );

                let addr = rand % 73 + 0x4_0000_0000i64 - offset as i64;

                test_ins(
                    false,
                    format!(
                        "mov32 r{tmp},{}
                         mov64 r{src},{:#x}
                         lsh64 r{src},32
                         or64 r{src},r{tmp}
                         ldxdw r{dst}, [r{src}{offset:+}]",
                        addr as i32,
                        addr >> 32,
                    ),
                    &mut prng,
                    25,
                );
                test_ins(
                    false,
                    format!(
                        "mov32 r{tmp},{}
                         mov64 r{src},{:#x}
                         lsh64 r{src},32
                         or64 r{src},r{tmp}
                         stxdw [r{src}{offset:+}], r{dst}",
                        addr as i32,
                        addr >> 32,
                    ),
                    &mut prng,
                    25,
                );

                // test conditionals
                for jc in [
                    "jeq", "jgt", "jge", "jlt", "jle", "jset", "jne", "jsgt", "jsge", "jslt",
                    "jsle",
                ] {
                    test_ins(
                        false,
                        format!(
                            "{jc} r{src}, r{dst}, l1
                            or64 r{src},0x12345678
                            ja l2
                        l1:
                            and64 r{dst},0x12345678
                            ja l2
                        l2:",
                        ),
                        &mut prng,
                        23,
                    );
                }
            }
        }

        for _ in 0..10 {
            let imm = prng.next_u64() as i64;
            test_ins(true, format!("lddw r{src}, {imm}"), &mut prng, 21);

            let mut imm = imm as i32;

            test_ins(true, format!("neg64 r{src}"), &mut prng, 21);
            test_ins(true, format!("neg32 r{src}"), &mut prng, 21);

            test_ins(true, format!("mul64 r{src}, {imm}"), &mut prng, 21);
            test_ins(true, format!("mod64 r{src}, {imm}"), &mut prng, 21);
            test_ins(true, format!("div64 r{src}, {imm}"), &mut prng, 21);

            test_ins(true, format!("mul32 r{src}, {imm}"), &mut prng, 21);
            test_ins(true, format!("mod32 r{src}, {imm}"), &mut prng, 21);
            test_ins(true, format!("div32 r{src}, {imm}"), &mut prng, 21);

            test_ins(false, format!("mov64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("add64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("sub64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("or64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("xor64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("and64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("lmul64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("uhmul64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("shmul64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("udiv64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("urem64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("sdiv64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("srem64 r{src}, {imm}"), &mut prng, 21);

            test_ins(false, format!("mov32 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("add32 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("sub32 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("or32 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("xor32 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("and32 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("lmul32 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("udiv32 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("urem32 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("sdiv32 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("srem32 r{src}, {imm}"), &mut prng, 21);

            // test st imm
            let rand = prng.next_u32() as i64;
            let offset = prng.next_u32() as i16;
            let tmp = (src + 1) % 10;

            let addr = rand % 80 + 0x4_0000_0000i64 - offset as i64;
            test_ins(
                false,
                format!(
                    "mov32 r{tmp},{}
                    mov64 r{src},{:#x}
                    lsh64 r{src},32
                    or64 r{src},r{tmp}
                    stb [r{src}{offset:+}], {}",
                    addr as i32,
                    addr >> 32,
                    imm as i8,
                ),
                &mut prng,
                25,
            );

            let addr = rand % 79 + 0x4_0000_0000i64 - offset as i64;
            test_ins(
                false,
                format!(
                    "mov32 r{tmp},{}
                    mov64 r{src},{:#x}
                    lsh64 r{src},32
                    or64 r{src},r{tmp}
                    sth [r{src}{offset:+}], {}",
                    addr as i32,
                    addr >> 32,
                    imm as i16
                ),
                &mut prng,
                25,
            );

            let addr = rand % 77 + 0x4_0000_0000i64 - offset as i64;
            test_ins(
                false,
                format!(
                    "mov32 r{tmp},{}
                    mov64 r{src},{:#x}
                    lsh64 r{src},32
                    or64 r{src},r{tmp}
                    stw [r{src}{offset:+}], {imm}",
                    addr as i32,
                    addr >> 32,
                ),
                &mut prng,
                25,
            );

            let addr = rand % 73 + 0x4_0000_0000i64 - offset as i64;
            test_ins(
                false,
                format!(
                    "mov32 r{tmp},{}
                    mov64 r{src},{:#x}
                    lsh64 r{src},32
                    or64 r{src},r{tmp}
                    stdw [r{src}{offset:+}], {imm}",
                    addr as i32,
                    addr >> 32,
                ),
                &mut prng,
                25,
            );

            // unconditional jump
            test_ins(
                false,
                format!(
                    "ja 1
                     xor64 r{src},0x12345678
            1:",
                ),
                &mut prng,
                21,
            );

            for jc in [
                "jeq", "jgt", "jge", "jlt", "jle", "jset", "jne", "jsgt", "jsge", "jslt", "jsle",
            ] {
                test_ins(
                    false,
                    format!(
                        "{jc} r{src}, {imm}, l1
                        or64 r{src},0x12345678
                        ja l2
                    l1:
                        and64 r{src},0x12345678
                        ja l2
                    l2:",
                    ),
                    &mut prng,
                    23,
                );
            }

            imm &= 63;

            test_ins(false, format!("lsh64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("rsh64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("arsh64 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("hor64 r{src}, {imm}"), &mut prng, 21);

            imm &= 31;

            test_ins(false, format!("lsh32 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("rsh32 r{src}, {imm}"), &mut prng, 21);
            test_ins(false, format!("arsh32 r{src}, {imm}"), &mut prng, 21);

            test_ins(false, format!("be64 r{src}"), &mut prng, 21);
            test_ins(false, format!("be32 r{src}"), &mut prng, 21);
            test_ins(false, format!("be16 r{src}"), &mut prng, 21);

            test_ins(true, format!("le64 r{src}"), &mut prng, 21);
            test_ins(true, format!("le32 r{src}"), &mut prng, 21);
            test_ins(true, format!("le16 r{src}"), &mut prng, 21);
        }
    }
}

fn test_ins(v1: bool, ins: String, prng: &mut SmallRng, cu: u64) {
    let mut input = [0u8; 80];

    prng.fill_bytes(&mut input);

    let asm = format!(
        "
        ldxdw r9, [r1+72]
        ldxdw r8, [r1+64]
        ldxdw r7, [r1+56]
        ldxdw r6, [r1+48]
        ldxdw r5, [r1+40]
        ldxdw r4, [r1+32]
        ldxdw r3, [r1+24]
        ldxdw r2, [r1+16]
        ldxdw r0, [r1+0]
        ldxdw r1, [r1+8]
        {ins}
        xor64 r0, r1
        xor64 r0, r2
        xor64 r0, r3
        xor64 r0, r4
        xor64 r0, r5
        xor64 r0, r6
        xor64 r0, r7
        xor64 r0, r8
        xor64 r0, r9
        exit"
    );

    let mut config = Config::default();
    if v1 {
        config.enable_sbpf_v2 = false;
    }
    test_interpreter_and_jit_asm!(asm.as_str(), config, input, (), TestContextObject::new(cu));
}
