use {
    criterion::{criterion_group, criterion_main, Criterion},
    solana_account::{Account, AccountSharedData},
    solana_program_runtime::serialization::serialize_parameters,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::{bpf_loader, bpf_loader_deprecated},
    solana_transaction_context::{instruction_accounts::InstructionAccount, TransactionContext},
};

fn create_inputs(owner: Pubkey, num_instruction_accounts: usize) -> TransactionContext<'static> {
    let program_id = solana_pubkey::new_rand();
    let transaction_accounts = vec![
        (
            program_id,
            AccountSharedData::from(Account {
                lamports: 0,
                data: vec![],
                owner,
                executable: true,
                rent_epoch: 0,
            }),
        ),
        (
            solana_pubkey::new_rand(),
            AccountSharedData::from(Account {
                lamports: 1,
                data: vec![1u8; 100000],
                owner,
                executable: false,
                rent_epoch: 100,
            }),
        ),
        (
            solana_pubkey::new_rand(),
            AccountSharedData::from(Account {
                lamports: 2,
                data: vec![11u8; 100000],
                owner,
                executable: true,
                rent_epoch: 200,
            }),
        ),
        (
            solana_pubkey::new_rand(),
            AccountSharedData::from(Account {
                lamports: 3,
                data: vec![],
                owner,
                executable: false,
                rent_epoch: 3100,
            }),
        ),
        (
            solana_pubkey::new_rand(),
            AccountSharedData::from(Account {
                lamports: 4,
                data: vec![1u8; 100000],
                owner,
                executable: false,
                rent_epoch: 100,
            }),
        ),
        (
            solana_pubkey::new_rand(),
            AccountSharedData::from(Account {
                lamports: 5,
                data: vec![11u8; 10000],
                owner,
                executable: true,
                rent_epoch: 200,
            }),
        ),
        (
            solana_pubkey::new_rand(),
            AccountSharedData::from(Account {
                lamports: 6,
                data: vec![],
                owner,
                executable: false,
                rent_epoch: 3100,
            }),
        ),
    ];
    let mut instruction_accounts: Vec<InstructionAccount> = Vec::new();
    for (instruction_account_index, index_in_transaction) in [1, 1, 2, 3, 4, 4, 5, 6]
        .into_iter()
        .cycle()
        .take(num_instruction_accounts)
        .enumerate()
    {
        instruction_accounts.push(InstructionAccount::new(
            index_in_transaction,
            false,
            instruction_account_index >= 4,
        ));
    }

    let mut transaction_context =
        TransactionContext::new(transaction_accounts, Rent::default(), 1, 1);
    let instruction_data = vec![1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];
    transaction_context
        .configure_next_instruction_for_tests(0, instruction_accounts, instruction_data)
        .unwrap();
    transaction_context.push().unwrap();
    transaction_context
}

fn bench_serialize_unaligned(c: &mut Criterion) {
    let transaction_context = create_inputs(bpf_loader_deprecated::id(), 7);
    let instruction_context = transaction_context
        .get_current_instruction_context()
        .unwrap();

    c.bench_function("serialize_unaligned", |b| {
        b.iter(|| {
            let _ = serialize_parameters(
                &instruction_context,
                true, // stricter_abi_and_runtime_constraints
                true, // account_data_direct_mapping
                true, // mask_out_rent_epoch_in_vm_serialization
            )
            .unwrap();
        });
    });
}

fn bench_serialize_unaligned_copy_account_data(c: &mut Criterion) {
    let transaction_context = create_inputs(bpf_loader_deprecated::id(), 7);
    let instruction_context = transaction_context
        .get_current_instruction_context()
        .unwrap();
    c.bench_function("serialize_unaligned_copy_account_data", |b| {
        b.iter(|| {
            let _ = serialize_parameters(
                &instruction_context,
                false, // stricter_abi_and_runtime_constraints
                false, // account_data_direct_mapping
                true,  // mask_out_rent_epoch_in_vm_serialization
            )
            .unwrap();
        });
    });
}

fn bench_serialize_aligned(c: &mut Criterion) {
    let transaction_context = create_inputs(bpf_loader::id(), 7);
    let instruction_context = transaction_context
        .get_current_instruction_context()
        .unwrap();

    c.bench_function("serialize_aligned", |b| {
        b.iter(|| {
            let _ = serialize_parameters(
                &instruction_context,
                true, // stricter_abi_and_runtime_constraints
                true, // account_data_direct_mapping
                true, // mask_out_rent_epoch_in_vm_serialization
            )
            .unwrap();
        });
    });
}

fn bench_serialize_aligned_copy_account_data(c: &mut Criterion) {
    let transaction_context = create_inputs(bpf_loader::id(), 7);
    let instruction_context = transaction_context
        .get_current_instruction_context()
        .unwrap();

    c.bench_function("serialize_aligned_copy_account_data", |b| {
        b.iter(|| {
            let _ = serialize_parameters(
                &instruction_context,
                false, // stricter_abi_and_runtime_constraints
                false, // account_data_direct_mapping
                true,  // mask_out_rent_epoch_in_vm_serialization
            )
            .unwrap();
        });
    });
}

fn bench_serialize_unaligned_max_accounts(c: &mut Criterion) {
    let transaction_context = create_inputs(bpf_loader_deprecated::id(), 255);
    let instruction_context = transaction_context
        .get_current_instruction_context()
        .unwrap();

    c.bench_function("serialize_unaligned_max_accounts", |b| {
        b.iter(|| {
            let _ = serialize_parameters(
                &instruction_context,
                true, // stricter_abi_and_runtime_constraints
                true, // account_data_direct_mapping
                true, // mask_out_rent_epoch_in_vm_serialization
            )
            .unwrap();
        });
    });
}

fn bench_serialize_aligned_max_accounts(c: &mut Criterion) {
    let transaction_context = create_inputs(bpf_loader::id(), 255);
    let instruction_context = transaction_context
        .get_current_instruction_context()
        .unwrap();

    c.bench_function("serialize_aligned_max_accounts", |b| {
        b.iter(|| {
            let _ = serialize_parameters(
                &instruction_context,
                true, // stricter_abi_and_runtime_constraints
                true, // account_data_direct_mapping
                true, // mask_out_rent_epoch_in_vm_serialization
            )
            .unwrap();
        });
    });
}

criterion_group!(
    benches,
    bench_serialize_unaligned,
    bench_serialize_unaligned_copy_account_data,
    bench_serialize_aligned,
    bench_serialize_aligned_copy_account_data,
    bench_serialize_unaligned_max_accounts,
    bench_serialize_aligned_max_accounts
);
criterion_main!(benches);
