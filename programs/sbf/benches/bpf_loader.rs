#![feature(test)]
#![cfg(feature = "sbf_c")]
#![allow(clippy::uninlined_format_args)]
#![allow(clippy::arithmetic_side_effects)]
#![cfg_attr(
    any(target_os = "windows", not(target_arch = "x86_64")),
    allow(dead_code, unused_imports)
)]

use {solana_keypair::Keypair, std::slice};

extern crate test;

use {
    agave_syscalls::create_program_runtime_environment_v1,
    byteorder::{ByteOrder, LittleEndian, WriteBytesExt},
    solana_account::AccountSharedData,
    solana_bpf_loader_program::create_vm,
    solana_client_traits::SyncClient,
    solana_instruction::{AccountMeta, Instruction},
    solana_measure::measure::Measure,
    solana_message::Message,
    solana_program_entrypoint::SUCCESS,
    solana_program_runtime::{
        execution_budget::SVMTransactionExecutionBudget, invoke_context::InvokeContext,
        serialization::serialize_parameters,
    },
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        bank_client::BankClient,
        genesis_utils::{create_genesis_config, GenesisConfigInfo},
        loader_utils::{load_program_from_file, load_program_of_loader_v4},
    },
    solana_sbpf::{
        ebpf::MM_INPUT_START, elf::Executable, memory_region::MemoryRegion,
        verifier::RequisiteVerifier, vm::ContextObject,
    },
    solana_sdk_ids::{bpf_loader, native_loader},
    solana_signer::Signer,
    solana_svm_feature_set::SVMFeatureSet,
    solana_transaction_context::instruction_accounts::InstructionAccount,
    std::{mem, sync::Arc},
    test::Bencher,
};

const ARMSTRONG_LIMIT: u64 = 500;
const ARMSTRONG_EXPECTED: u64 = 5;

macro_rules! with_mock_invoke_context {
    ($invoke_context:ident, $loader_id:expr, $account_size:expr) => {
        let program_key = Pubkey::new_unique();
        let transaction_accounts = vec![
            (
                $loader_id,
                AccountSharedData::new(0, 0, &native_loader::id()),
            ),
            (program_key, AccountSharedData::new(1, 0, &$loader_id)),
            (
                Pubkey::new_unique(),
                AccountSharedData::new(2, $account_size, &program_key),
            ),
        ];
        let instruction_accounts = vec![InstructionAccount::new(2, false, true)];
        solana_program_runtime::with_mock_invoke_context!(
            $invoke_context,
            transaction_context,
            transaction_accounts
        );
        $invoke_context
            .transaction_context
            .configure_next_instruction_for_tests(1, instruction_accounts, vec![])
            .unwrap();
        $invoke_context.push().unwrap();
    };
}

#[bench]
fn bench_program_create_executable(bencher: &mut Bencher) {
    let elf = load_program_from_file("bench_alu");

    let feature_set = SVMFeatureSet::default();
    let program_runtime_environment = create_program_runtime_environment_v1(
        &feature_set,
        &SVMTransactionExecutionBudget::new_with_defaults(feature_set.raise_cpi_nesting_limit_to_8),
        true,
        false,
    );
    let program_runtime_environment = Arc::new(program_runtime_environment.unwrap());
    bencher.iter(|| {
        let _ = Executable::<InvokeContext>::from_elf(&elf, program_runtime_environment.clone())
            .unwrap();
    });
}

#[bench]
#[cfg(all(not(target_os = "windows"), target_arch = "x86_64"))]
fn bench_program_alu(bencher: &mut Bencher) {
    let ns_per_s = 1000000000;
    let one_million = 1000000;
    let mut inner_iter = vec![];
    inner_iter
        .write_u64::<LittleEndian>(ARMSTRONG_LIMIT)
        .unwrap();
    inner_iter.write_u64::<LittleEndian>(0).unwrap();
    let elf = load_program_from_file("bench_alu");
    with_mock_invoke_context!(invoke_context, bpf_loader::id(), 10000001);

    let feature_set = invoke_context.get_feature_set();
    let program_runtime_environment = create_program_runtime_environment_v1(
        feature_set,
        &SVMTransactionExecutionBudget::new_with_defaults(feature_set.raise_cpi_nesting_limit_to_8),
        true,
        false,
    );
    let mut executable =
        Executable::<InvokeContext>::from_elf(&elf, Arc::new(program_runtime_environment.unwrap()))
            .unwrap();

    executable.verify::<RequisiteVerifier>().unwrap();

    executable.jit_compile().unwrap();
    create_vm!(
        vm,
        &executable,
        vec![MemoryRegion::new_writable(&mut inner_iter, MM_INPUT_START)],
        vec![],
        &mut invoke_context,
    );
    let (mut vm, _, _) = vm.unwrap();

    println!("Interpreted:");
    vm.context_object_pointer
        .mock_set_remaining(i64::MAX as u64);
    let (instructions, result) = vm.execute_program(&executable, true);
    assert_eq!(SUCCESS, result.unwrap());
    assert_eq!(ARMSTRONG_LIMIT, LittleEndian::read_u64(&inner_iter));
    assert_eq!(
        ARMSTRONG_EXPECTED,
        LittleEndian::read_u64(&inner_iter[mem::size_of::<u64>()..])
    );

    bencher.iter(|| {
        vm.context_object_pointer
            .mock_set_remaining(i64::MAX as u64);
        vm.execute_program(&executable, true).1.unwrap();
    });
    let summary = bencher.bench(|_bencher| Ok(())).unwrap().unwrap();
    println!("  {:?} instructions", instructions);
    println!("  {:?} ns/iter median", summary.median as u64);
    assert!(0f64 != summary.median);
    let mips = (instructions * (ns_per_s / summary.median as u64)) / one_million;
    println!("  {:?} MIPS", mips);
    println!(
        "{{ \"type\": \"bench\", \"name\": \"bench_program_alu_interpreted_mips\", \"median\": \
         {mips:?}, \"deviation\": 0 }}",
    );

    println!("JIT to native:");
    assert_eq!(SUCCESS, vm.execute_program(&executable, false).1.unwrap());
    assert_eq!(ARMSTRONG_LIMIT, LittleEndian::read_u64(&inner_iter));
    assert_eq!(
        ARMSTRONG_EXPECTED,
        LittleEndian::read_u64(&inner_iter[mem::size_of::<u64>()..])
    );

    bencher.iter(|| {
        vm.context_object_pointer
            .mock_set_remaining(i64::MAX as u64);
        vm.execute_program(&executable, false).1.unwrap();
    });
    let summary = bencher.bench(|_bencher| Ok(())).unwrap().unwrap();
    println!("  {:?} instructions", instructions);
    println!("  {:?} ns/iter median", summary.median as u64);
    assert!(0f64 != summary.median);
    let mips = (instructions * (ns_per_s / summary.median as u64)) / one_million;
    println!("  {:?} MIPS", mips);
    println!(
        "{{ \"type\": \"bench\", \"name\": \"bench_program_alu_jit_to_native_mips\", \"median\": \
         {mips:?}, \"deviation\": 0 }}",
    );
}

#[bench]
fn bench_program_execute_noop(bencher: &mut Bencher) {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);

    let bank = Bank::new_for_benches(&genesis_config);
    let (bank, bank_forks) = bank.wrap_with_bank_forks_for_tests();
    let mut bank_client = BankClient::new_shared(bank.clone());

    let authority_keypair = Keypair::new();
    let mint_pubkey = mint_keypair.pubkey();

    let (_bank, invoke_program_id) = load_program_of_loader_v4(
        &mut bank_client,
        &bank_forks,
        &mint_keypair,
        &authority_keypair,
        "noop",
    );

    let account_metas = vec![AccountMeta::new(mint_pubkey, true)];

    let instruction =
        Instruction::new_with_bincode(invoke_program_id, &[u8::MAX, 0, 0, 0], account_metas);
    let message = Message::new(&[instruction], Some(&mint_pubkey));

    bank_client
        .send_and_confirm_message(&[&mint_keypair], message.clone())
        .unwrap();

    bencher.iter(|| {
        bank.clear_signatures();
        bank_client
            .send_and_confirm_message(&[&mint_keypair], message.clone())
            .unwrap();
    });
}

#[bench]
fn bench_create_vm(bencher: &mut Bencher) {
    let elf = load_program_from_file("noop");
    with_mock_invoke_context!(invoke_context, bpf_loader::id(), 10000001);
    const BUDGET: u64 = 200_000;
    invoke_context.mock_set_remaining(BUDGET);

    let stricter_abi_and_runtime_constraints = invoke_context
        .get_feature_set()
        .stricter_abi_and_runtime_constraints;
    let account_data_direct_mapping = invoke_context.get_feature_set().account_data_direct_mapping;
    let raise_cpi_nesting_limit_to_8 = invoke_context
        .get_feature_set()
        .raise_cpi_nesting_limit_to_8;
    let program_runtime_environment = create_program_runtime_environment_v1(
        invoke_context.get_feature_set(),
        &SVMTransactionExecutionBudget::new_with_defaults(raise_cpi_nesting_limit_to_8),
        true,
        false,
    );
    let executable =
        Executable::<InvokeContext>::from_elf(&elf, Arc::new(program_runtime_environment.unwrap()))
            .unwrap();

    executable.verify::<RequisiteVerifier>().unwrap();

    // Serialize account data
    let (_serialized, regions, account_lengths, _instruction_data_offset) = serialize_parameters(
        &invoke_context
            .transaction_context
            .get_current_instruction_context()
            .unwrap(),
        stricter_abi_and_runtime_constraints,
        account_data_direct_mapping,
        true, // mask_out_rent_epoch_in_vm_serialization
    )
    .unwrap();

    bencher.iter(|| {
        create_vm!(
            vm,
            &executable,
            clone_regions(&regions),
            account_lengths.clone(),
            &mut invoke_context,
        );
        vm.unwrap();
    });
}

#[bench]
fn bench_instruction_count_tuner(_bencher: &mut Bencher) {
    let elf = load_program_from_file("tuner");
    with_mock_invoke_context!(invoke_context, bpf_loader::id(), 10000001);
    const BUDGET: u64 = 200_000;
    invoke_context.mock_set_remaining(BUDGET);

    let stricter_abi_and_runtime_constraints = invoke_context
        .get_feature_set()
        .stricter_abi_and_runtime_constraints;
    let account_data_direct_mapping = invoke_context.get_feature_set().account_data_direct_mapping;

    // Serialize account data
    let (_serialized, regions, account_lengths, _instruction_data_offset) = serialize_parameters(
        &invoke_context
            .transaction_context
            .get_current_instruction_context()
            .unwrap(),
        stricter_abi_and_runtime_constraints,
        account_data_direct_mapping,
        true, // mask_out_rent_epoch_in_vm_serialization
    )
    .unwrap();

    let feature_set = invoke_context.get_feature_set();
    let program_runtime_environment = create_program_runtime_environment_v1(
        feature_set,
        &SVMTransactionExecutionBudget::new_with_defaults(feature_set.raise_cpi_nesting_limit_to_8),
        true,
        false,
    );
    let executable =
        Executable::<InvokeContext>::from_elf(&elf, Arc::new(program_runtime_environment.unwrap()))
            .unwrap();

    executable.verify::<RequisiteVerifier>().unwrap();

    create_vm!(
        vm,
        &executable,
        regions,
        account_lengths,
        &mut invoke_context,
    );
    let (mut vm, _, _) = vm.unwrap();

    let mut measure = Measure::start("tune");
    let (instructions, _result) = vm.execute_program(&executable, true);
    measure.stop();

    assert_eq!(
        0,
        vm.context_object_pointer.get_remaining(),
        "Tuner must consume the whole budget"
    );
    println!(
        "{:?} compute units took {:?} us ({:?} instructions)",
        BUDGET - vm.context_object_pointer.get_remaining(),
        measure.as_us(),
        instructions,
    );
}

fn clone_regions(regions: &[MemoryRegion]) -> Vec<MemoryRegion> {
    unsafe {
        regions
            .iter()
            .map(|region| {
                let mut new_region = if region.writable {
                    MemoryRegion::new_writable(
                        slice::from_raw_parts_mut(region.host_addr as *mut _, region.len as usize),
                        region.vm_addr,
                    )
                } else {
                    MemoryRegion::new_readonly(
                        slice::from_raw_parts(region.host_addr as *const _, region.len as usize),
                        region.vm_addr,
                    )
                };
                new_region.access_violation_handler_payload =
                    region.access_violation_handler_payload;
                new_region
            })
            .collect()
    }
}
