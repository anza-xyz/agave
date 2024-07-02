// Copyright 2020 Solana Maintainers <maintainers@solana.com>
//
// Licensed under the Apache License, Version 2.0 <http://www.apache.org/licenses/LICENSE-2.0> or
// the MIT license <http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

#![feature(test)]

extern crate solana_rbpf;
extern crate test;

use {
    solana_rbpf::{
        create_vm, elf::Executable, program::BuiltinProgram, verifier::RequisiteVerifier,
        vm::TestContextObject,
    },
    std::{fs::File, io::Read, sync::Arc},
    test::Bencher,
};

#[bench]
fn bench_init_vm(bencher: &mut Bencher) {
    let mut file = File::open("tests/elfs/relative_call.so").unwrap();
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    let executable =
        Executable::<TestContextObject>::from_elf(&elf, Arc::new(BuiltinProgram::new_mock()))
            .unwrap();
    executable.verify::<RequisiteVerifier>().unwrap();
    bencher.iter(|| {
        let mut context_object = TestContextObject::default();
        create_vm!(
            _vm,
            &executable,
            &mut context_object,
            stack,
            heap,
            Vec::new(),
            None
        );
    });
}

#[cfg(all(feature = "jit", not(target_os = "windows"), target_arch = "x86_64"))]
#[bench]
fn bench_jit_compile(bencher: &mut Bencher) {
    let mut file = File::open("tests/elfs/relative_call.so").unwrap();
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    let mut executable =
        Executable::<TestContextObject>::from_elf(&elf, Arc::new(BuiltinProgram::new_mock()))
            .unwrap();
    executable.verify::<RequisiteVerifier>().unwrap();
    bencher.iter(|| executable.jit_compile().unwrap());
}
