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
        elf::Executable,
        program::{BuiltinFunction, BuiltinProgram, FunctionRegistry},
        syscalls,
        vm::{Config, TestContextObject},
    },
    std::{fs::File, io::Read, sync::Arc},
    test::Bencher,
};

fn loader() -> Arc<BuiltinProgram<TestContextObject>> {
    let mut function_registry = FunctionRegistry::<BuiltinFunction<TestContextObject>>::default();
    function_registry
        .register_function_hashed(*b"log", syscalls::SyscallString::vm)
        .unwrap();
    Arc::new(BuiltinProgram::new_loader(
        Config::default(),
        function_registry,
    ))
}

#[bench]
fn bench_load_sbpfv1(bencher: &mut Bencher) {
    let mut file = File::open("tests/elfs/syscall_reloc_64_32.so").unwrap();
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    let loader = loader();
    bencher.iter(|| Executable::<TestContextObject>::from_elf(&elf, loader.clone()).unwrap());
}

#[bench]
fn bench_load_sbpfv2(bencher: &mut Bencher) {
    let mut file = File::open("tests/elfs/syscall_static.so").unwrap();
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    let loader = loader();
    bencher.iter(|| Executable::<TestContextObject>::from_elf(&elf, loader.clone()).unwrap());
}
