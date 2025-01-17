use {
    criterion::{black_box, criterion_group, criterion_main, Criterion},
    solana_account::{create_account_shared_data_for_test, AccountSharedData},
    solana_instruction::AccountMeta,
    solana_program_runtime::{
        invoke_context::mock_process_instruction, loaded_programs::ProgramCacheEntry,
    },
    solana_pubkey::Pubkey,
    solana_sdk::{
        address_lookup_table::instruction::{derive_lookup_table_address, ProgramInstruction},
        clock::{Clock, Slot},
        hash::Hash,
        rent::Rent,
        sysvar::{clock, slot_hashes::SlotHashes},
    },
    solana_sdk_ids::{
        address_lookup_table, system_program,
        sysvar::{rent, slot_hashes},
    },
    std::sync::Arc,
};

const ACCOUNT_BALANCE: u64 = u64::MAX / 4;

#[derive(Default)]
struct TestSetup {
    authority_address: Pubkey,
    payer_address: Pubkey,
    lookup_table_address: Pubkey,
    bump_seed: u8,
    recent_slot: Slot,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
    instruction_data: Vec<u8>,
}

impl TestSetup {
    fn new() -> Self {
        let authority_address = Pubkey::new_unique();
        let payer_address = Pubkey::new_unique();
        let recent_slot = 1;
        let mut slot_hashes = SlotHashes::default();
        slot_hashes.add(recent_slot, Hash::new_unique());

        let (lookup_table_address, bump_seed) =
            derive_lookup_table_address(&authority_address, recent_slot);

        let transaction_accounts: Vec<(Pubkey, AccountSharedData)> = vec![
            // lookup table account is initially owned by system program, it later
            // allocate() by native_invoke System program during create_lookup_table
            (
                lookup_table_address,
                AccountSharedData::new(0, 0, &system_program::id()),
            ),
            (
                authority_address,
                AccountSharedData::new(ACCOUNT_BALANCE, 0, &Pubkey::new_unique()),
            ),
            (
                payer_address,
                AccountSharedData::new(ACCOUNT_BALANCE, 0, &system_program::id()),
            ),
            (
                system_program::id(),
                AccountSharedData::new(0, 0, &system_program::id()),
            ),
            (
                slot_hashes::id(),
                create_account_shared_data_for_test(&slot_hashes),
            ),
            (
                rent::id(),
                create_account_shared_data_for_test(&Rent::default()),
            ),
            (
                clock::id(),
                create_account_shared_data_for_test(&Clock::default()),
            ),
        ];

        Self {
            authority_address,
            payer_address,
            lookup_table_address,
            bump_seed,
            recent_slot,
            transaction_accounts,
            ..TestSetup::default()
        }
    }

    fn prep_create_lookup_table(&mut self) {
        self.instruction_accounts = vec![
            AccountMeta::new(self.lookup_table_address, false),
            AccountMeta::new_readonly(self.authority_address, false),
            AccountMeta::new(self.payer_address, true),
            AccountMeta::new_readonly(system_program::id(), false),
        ];

        self.instruction_data = bincode::serialize(&ProgramInstruction::CreateLookupTable {
            recent_slot: self.recent_slot,
            bump_seed: self.bump_seed,
        })
        .unwrap();
    }

    fn exec_create_lookup_table(&mut self) {
        self.prep_create_lookup_table();
        let accounts = self.run();

        let lookup_table_account = accounts[0].clone();
        self.transaction_accounts[0] = (self.lookup_table_address, lookup_table_account);
    }

    fn prep_extend_lookup_table(&mut self) {
        self.instruction_accounts = vec![
            AccountMeta::new(self.lookup_table_address, false),
            AccountMeta::new_readonly(self.authority_address, true),
            AccountMeta::new(self.payer_address, true),
            AccountMeta::new_readonly(system_program::id(), false),
        ];

        // extend reasonable number of addresses at a time, so bench loop won't
        // add more than LOOKUP_TABLE_MAX_ADDRESSES addresses to lookup_table
        let new_addresses: Vec<_> = (0..16).map(|_| Pubkey::new_unique()).collect();
        self.instruction_data =
            bincode::serialize(&ProgramInstruction::ExtendLookupTable { new_addresses }).unwrap();
    }

    fn exec_extend_lookup_table(&mut self) {
        self.prep_extend_lookup_table();
        let accounts = self.run();

        let lookup_table_account = accounts[0].clone();
        self.transaction_accounts[0] = (self.lookup_table_address, lookup_table_account);
    }

    fn prep_freeze_lookup_table(&mut self) {
        self.instruction_accounts = vec![
            AccountMeta::new(self.lookup_table_address, false),
            AccountMeta::new(self.authority_address, true),
        ];

        self.instruction_data = bincode::serialize(&ProgramInstruction::FreezeLookupTable).unwrap();
    }

    fn prep_deactivate_lookup_table(&mut self) {
        self.instruction_accounts = vec![
            AccountMeta::new(self.lookup_table_address, false),
            AccountMeta::new(self.authority_address, true),
        ];

        self.instruction_data =
            bincode::serialize(&ProgramInstruction::DeactivateLookupTable).unwrap();
    }

    fn exec_deactivate_lookup_table(&mut self) {
        self.prep_deactivate_lookup_table();
        let accounts = self.run();

        let lookup_table_account = accounts[0].clone();
        self.transaction_accounts[0] = (self.lookup_table_address, lookup_table_account);
        // advance clock after deactivating
        self.transaction_accounts[6] = (
            clock::id(),
            create_account_shared_data_for_test(&Clock {
                slot: self.recent_slot.wrapping_add(1),
                ..Clock::default()
            }),
        );
    }

    fn prep_close_lookup_table(&mut self) {
        self.instruction_accounts = vec![
            AccountMeta::new(self.lookup_table_address, false),
            AccountMeta::new(self.authority_address, true),
            AccountMeta::new(self.payer_address, false),
        ];

        self.instruction_data = bincode::serialize(&ProgramInstruction::CloseLookupTable).unwrap();
    }

    fn run(&self) -> Vec<AccountSharedData> {
        mock_process_instruction(
            &address_lookup_table::id(),
            Vec::new(),
            &self.instruction_data,
            self.transaction_accounts.clone(),
            self.instruction_accounts.clone(),
            Ok(()), //expected_result,
            solana_address_lookup_table_program::processor::Entrypoint::vm,
            |invoke_context| {
                // add System to program_cache_for_tx_batch, so it can be native_invoke()
                invoke_context.program_cache_for_tx_batch.replenish(
                    system_program::id(),
                    Arc::new(ProgramCacheEntry::new_builtin(
                        0,
                        0,
                        solana_system_program::system_processor::Entrypoint::vm,
                    )),
                );
            },
            |_invoke_context| {},
        )
    }
}

fn bench_create_lookup_table(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.prep_create_lookup_table();

    c.bench_function("create_lookup_table", |bencher| {
        bencher.iter(|| black_box(test_setup.run()))
    });
}

fn bench_extend_lookup_table(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.exec_create_lookup_table();
    test_setup.prep_extend_lookup_table();

    c.bench_function("extend_lookup_table", |bencher| {
        bencher.iter(|| black_box(test_setup.run()))
    });
}

fn bench_freeze_lookup_table(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.exec_create_lookup_table();
    test_setup.exec_extend_lookup_table();
    test_setup.prep_freeze_lookup_table();

    c.bench_function("freeze_lookup_table", |bencher| {
        bencher.iter(|| black_box(test_setup.run()))
    });
}

fn bench_deactivate_lookup_table(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.exec_create_lookup_table();
    test_setup.prep_deactivate_lookup_table();

    c.bench_function("deactivate_lookup_table", |bencher| {
        bencher.iter(|| black_box(test_setup.run()))
    });
}

fn bench_close_lookup_table(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.exec_create_lookup_table();
    test_setup.exec_deactivate_lookup_table();
    test_setup.prep_close_lookup_table();

    c.bench_function("close_lookup_table", |bencher| {
        bencher.iter(|| black_box(test_setup.run()))
    });
}

criterion_group!(
    benches,
    bench_create_lookup_table,
    bench_extend_lookup_table,
    bench_freeze_lookup_table,
    bench_deactivate_lookup_table,
    bench_close_lookup_table,
);
criterion_main!(benches);
