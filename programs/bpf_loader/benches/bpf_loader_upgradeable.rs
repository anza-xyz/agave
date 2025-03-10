use {
    criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput},
    solana_account::{
        create_account_shared_data_for_test, state_traits::StateMut, AccountSharedData,
        WritableAccount,
    },
    solana_bpf_loader_program::Entrypoint,
    solana_instruction::AccountMeta,
    solana_program::{
        bpf_loader_upgradeable::{self, UpgradeableLoaderState},
        loader_upgradeable_instruction::UpgradeableLoaderInstruction,
        system_program,
        sysvar::{self, rent::Rent},
    },
    solana_program_runtime::invoke_context::mock_process_instruction,
    solana_pubkey::Pubkey,
    solana_sysvar::clock::Clock,
    std::{fs::File, io::Read},
};

#[derive(Default)]
struct TestSetup {
    loader_address: Pubkey,
    authority_address: Pubkey,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,

    instruction_accounts: Vec<AccountMeta>,
    instruction_data: Vec<u8>,
}

const ACCOUNT_BALANCE: u64 = u64::MAX / 4;
const PROGRAM_BUFFER_SIZE: usize = 1024;
const SLOT: u64 = 42;

impl TestSetup {
    fn new() -> Self {
        let loader_address = bpf_loader_upgradeable::id();
        let buffer_address = Pubkey::new_unique();
        let authority_address = Pubkey::new_unique();

        let instruction_accounts = vec![AccountMeta {
            pubkey: buffer_address,
            is_signer: false,
            is_writable: true,
        }];
        let transaction_accounts = vec![
            (
                buffer_address,
                AccountSharedData::new(
                    ACCOUNT_BALANCE,
                    UpgradeableLoaderState::size_of_buffer(PROGRAM_BUFFER_SIZE),
                    &loader_address,
                ),
            ),
            (
                authority_address,
                AccountSharedData::new(ACCOUNT_BALANCE, 0, &loader_address),
            ),
        ];

        Self {
            loader_address,
            authority_address,
            transaction_accounts,
            instruction_accounts,
            ..TestSetup::default()
        }
    }

    fn prep_initialize_buffer(&mut self) {
        self.instruction_accounts.push(AccountMeta {
            pubkey: self.authority_address,
            is_signer: false,
            is_writable: false,
        });
        self.instruction_data =
            bincode::serialize(&UpgradeableLoaderInstruction::InitializeBuffer).unwrap();
    }

    fn prep_write(&mut self) {
        self.instruction_accounts.push(AccountMeta {
            pubkey: self.authority_address,
            is_signer: true,
            is_writable: false,
        });

        self.transaction_accounts[0]
            .1
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(self.authority_address),
            })
            .unwrap();

        self.instruction_data = bincode::serialize(&UpgradeableLoaderInstruction::Write {
            offset: 0,
            bytes: vec![64; PROGRAM_BUFFER_SIZE],
        })
        .unwrap();
    }

    fn prep_set_authority(&mut self, checked: bool) {
        let new_authority_address = Pubkey::new_unique();
        self.instruction_accounts.push(AccountMeta {
            pubkey: self.authority_address,
            is_signer: true,
            is_writable: false,
        });
        self.instruction_accounts.push(AccountMeta {
            pubkey: new_authority_address,
            is_signer: checked,
            is_writable: false,
        });

        self.transaction_accounts[0]
            .1
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: Some(self.authority_address),
            })
            .unwrap();
        self.transaction_accounts.push((
            new_authority_address,
            AccountSharedData::new(ACCOUNT_BALANCE, 0, &self.loader_address),
        ));
        let instruction = if checked {
            UpgradeableLoaderInstruction::SetAuthorityChecked
        } else {
            UpgradeableLoaderInstruction::SetAuthority
        };
        self.instruction_data = bincode::serialize(&instruction).unwrap();
    }

    fn prep_close(&mut self) {
        self.instruction_accounts.push(AccountMeta {
            pubkey: self.authority_address,
            is_signer: false,
            is_writable: true,
        });
        self.instruction_accounts.push(AccountMeta {
            pubkey: self.authority_address,
            is_signer: true,
            is_writable: false,
        });

        // lets use the same account for recipient and authority
        self.transaction_accounts
            .push(self.transaction_accounts[1].clone());
        self.instruction_data = bincode::serialize(&UpgradeableLoaderInstruction::Close).unwrap();
    }

    fn run(&self) {
        mock_process_instruction(
            &self.loader_address,
            Vec::new(),
            &self.instruction_data,
            self.transaction_accounts.clone(),
            self.instruction_accounts.clone(),
            Ok(()),
            Entrypoint::vm,
            |_invoke_context| {},
            |_invoke_context| {},
        );
    }
}

fn bench_initialize_buffer(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.prep_initialize_buffer();

    c.bench_function("initialize_buffer", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_write(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.prep_write();

    c.bench_function("write", |bencher| {
        bencher.iter(|| {
            test_setup.run();
        })
    });
}

fn get_accounts_for_bpf_loader_upgradeable_upgrade(
    buffer_address: &Pubkey,
    buffer_authority: &Pubkey,
    upgrade_authority_address: &Pubkey,
    elf_orig: &[u8],
    elf_new: &[u8],
) -> (Vec<(Pubkey, AccountSharedData)>, Vec<AccountMeta>) {
    let loader_id = bpf_loader_upgradeable::id();
    let program_address = Pubkey::new_unique();
    let spill_address = Pubkey::new_unique();
    let rent = Rent::default();
    let min_program_balance =
        1.max(rent.minimum_balance(UpgradeableLoaderState::size_of_program()));
    let min_programdata_balance = 1.max(rent.minimum_balance(
        UpgradeableLoaderState::size_of_programdata(elf_orig.len().max(elf_new.len())),
    ));
    let (programdata_address, _) =
        Pubkey::find_program_address(&[program_address.as_ref()], &loader_id);
    let mut buffer_account = AccountSharedData::new(
        1,
        UpgradeableLoaderState::size_of_buffer(elf_new.len()),
        &bpf_loader_upgradeable::id(),
    );
    buffer_account
        .set_state(&UpgradeableLoaderState::Buffer {
            authority_address: Some(*buffer_authority),
        })
        .unwrap();
    buffer_account
        .data_as_mut_slice()
        .get_mut(UpgradeableLoaderState::size_of_buffer_metadata()..)
        .unwrap()
        .copy_from_slice(elf_new);
    let mut programdata_account = AccountSharedData::new(
        min_programdata_balance,
        UpgradeableLoaderState::size_of_programdata(elf_orig.len().max(elf_new.len())),
        &bpf_loader_upgradeable::id(),
    );
    programdata_account
        .set_state(&UpgradeableLoaderState::ProgramData {
            slot: SLOT,
            upgrade_authority_address: Some(*upgrade_authority_address),
        })
        .unwrap();
    let mut program_account = AccountSharedData::new(
        min_program_balance,
        UpgradeableLoaderState::size_of_program(),
        &bpf_loader_upgradeable::id(),
    );
    program_account.set_executable(true);
    program_account
        .set_state(&UpgradeableLoaderState::Program {
            programdata_address,
        })
        .unwrap();
    let spill_account = AccountSharedData::new(0, 0, &Pubkey::new_unique());
    let rent_account = create_account_shared_data_for_test(&rent);
    let clock_account = create_account_shared_data_for_test(&Clock {
        slot: SLOT.saturating_add(1),
        ..Clock::default()
    });

    let upgrade_authority_account = AccountSharedData::new(1, 0, &Pubkey::new_unique());
    let transaction_accounts = vec![
        (programdata_address, programdata_account),
        (program_address, program_account),
        (*buffer_address, buffer_account),
        (spill_address, spill_account),
        (sysvar::rent::id(), rent_account),
        (sysvar::clock::id(), clock_account),
        (*upgrade_authority_address, upgrade_authority_account),
    ];
    let instruction_accounts = vec![
        AccountMeta {
            pubkey: programdata_address,
            is_signer: false,
            is_writable: true,
        },
        AccountMeta {
            pubkey: program_address,
            is_signer: false,
            is_writable: true,
        },
        AccountMeta {
            pubkey: *buffer_address,
            is_signer: false,
            is_writable: true,
        },
        AccountMeta {
            pubkey: spill_address,
            is_signer: false,
            is_writable: true,
        },
        AccountMeta {
            pubkey: sysvar::rent::id(),
            is_signer: false,
            is_writable: false,
        },
        AccountMeta {
            pubkey: sysvar::clock::id(),
            is_signer: false,
            is_writable: false,
        },
        AccountMeta {
            pubkey: *upgrade_authority_address,
            is_signer: true,
            is_writable: false,
        },
    ];
    (transaction_accounts, instruction_accounts)
}

fn bench_upgradeable_upgrade(c: &mut Criterion) {
    // For now load programs that are available, but need to create some custom programs for this case.
    let mut file = File::open("test_elfs/out/noop_aligned.so").expect("file open failed");
    let mut elf_orig = Vec::new();
    file.read_to_end(&mut elf_orig).unwrap();
    let mut file = File::open("test_elfs/out/noop_unaligned.so").expect("file open failed");
    let mut elf_new = Vec::new();
    file.read_to_end(&mut elf_new).unwrap();
    assert_ne!(elf_orig.len(), elf_new.len());
    let buffer_address = Pubkey::new_unique();
    let upgrade_authority_address = Pubkey::new_unique();
    let (transaction_accounts, instruction_accounts) =
        get_accounts_for_bpf_loader_upgradeable_upgrade(
            &buffer_address,
            &upgrade_authority_address,
            &upgrade_authority_address,
            &elf_orig,
            &elf_new,
        );

    let instruction_data = bincode::serialize(&UpgradeableLoaderInstruction::Upgrade).unwrap();
    c.bench_function("upgradeable_upgrade", |bencher| {
        bencher.iter(|| {
            mock_process_instruction(
                &bpf_loader_upgradeable::id(),
                Vec::new(),
                &instruction_data,
                transaction_accounts.clone(),
                instruction_accounts.clone(),
                Ok(()),
                Entrypoint::vm,
                |_invoke_context| {},
                |_invoke_context| {},
            )
        })
    });
}

fn bench_set_authority(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.prep_set_authority(false);

    c.bench_function("set_authority", |bencher| {
        bencher.iter(|| {
            test_setup.run();
        })
    });
}

fn bench_close(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.prep_close();

    c.bench_function("close", |bencher| {
        bencher.iter(|| {
            test_setup.run();
        })
    });
}

fn get_accounts_for_bpf_loader_extend_program(
    elf: &[u8],
) -> (Vec<(Pubkey, AccountSharedData)>, Vec<AccountMeta>) {
    let loader_id = bpf_loader_upgradeable::id();
    let account_data_len = elf.len();
    let rent = Rent::default();

    let program_address = Pubkey::new_unique();
    let (programdata_address, _) =
        Pubkey::find_program_address(&[program_address.as_ref()], &loader_id);
    let min_program_balance = ACCOUNT_BALANCE;
    let mut program_account = AccountSharedData::new(
        min_program_balance,
        UpgradeableLoaderState::size_of_program(),
        &loader_id,
    );
    program_account
        .set_state(&UpgradeableLoaderState::Program {
            programdata_address,
        })
        .expect("state failed to serialize into account data");

    let programdata_data_offset = UpgradeableLoaderState::size_of_programdata_metadata();
    let programdata_len: usize = account_data_len
        .checked_add(programdata_data_offset)
        .expect("Arithmetic overflow should not happen for programdata size.");
    let mut programdata_account =
        AccountSharedData::new(min_program_balance, programdata_len, &loader_id);
    programdata_account
        .set_state(&UpgradeableLoaderState::ProgramData {
            slot: 0,
            upgrade_authority_address: Some(Pubkey::new_unique()),
        })
        .expect("state failed to serialize into account data");
    programdata_account.data_as_mut_slice()[programdata_data_offset..].copy_from_slice(elf);

    let authority_address = Pubkey::new_unique();
    let authority_account = AccountSharedData::new(ACCOUNT_BALANCE, 0, &Pubkey::new_unique());
    let rent_account = create_account_shared_data_for_test(&rent);
    let clock_account = create_account_shared_data_for_test(&Clock {
        slot: SLOT.saturating_add(1),
        ..Clock::default()
    });
    let transaction_accounts = vec![
        (programdata_address, programdata_account),
        (program_address, program_account),
        (
            system_program::id(),
            AccountSharedData::new(0, 0, &system_program::id()),
        ),
        (authority_address, authority_account),
        (sysvar::rent::id(), rent_account),
        (sysvar::clock::id(), clock_account),
    ];
    let instruction_accounts = vec![
        AccountMeta::new(programdata_address, false),
        AccountMeta::new(program_address, false),
        AccountMeta::new_readonly(system_program::id(), false),
        AccountMeta::new(authority_address, true),
        AccountMeta {
            pubkey: sysvar::rent::id(),
            is_signer: false,
            is_writable: false,
        },
        AccountMeta {
            pubkey: sysvar::clock::id(),
            is_signer: false,
            is_writable: false,
        },
    ];
    (transaction_accounts, instruction_accounts)
}

fn bench_extend_program(c: &mut Criterion) {
    let mut file = File::open("test_elfs/out/noop_aligned.so").expect("file open failed");
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();

    let (transaction_accounts, instruction_accounts) =
        get_accounts_for_bpf_loader_extend_program(&elf);

    let mut group = c.benchmark_group("extend_program");

    for size in [64u32, 256, 1024, 4096].iter() {
        group.throughput(Throughput::Bytes(*size as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(size),
            size,
            |bencher, additional_bytes| {
                bencher.iter(|| {
                    let instruction_data =
                        bincode::serialize(&UpgradeableLoaderInstruction::ExtendProgram {
                            additional_bytes: *additional_bytes,
                        })
                        .unwrap();
                    mock_process_instruction(
                        &bpf_loader_upgradeable::id(),
                        Vec::new(),
                        &instruction_data,
                        transaction_accounts.clone(),
                        instruction_accounts.clone(),
                        Ok(()),
                        Entrypoint::vm,
                        |_invoke_context| {},
                        |_invoke_context| {},
                    )
                })
            },
        );
    }
    group.finish();
}

fn bench_set_authority_checked(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.prep_set_authority(true);

    c.bench_function("set_authority_checked", |bencher| {
        bencher.iter(|| {
            test_setup.run();
        })
    });
}

criterion_group!(
    benches,
    bench_initialize_buffer,
    bench_write,
    bench_upgradeable_upgrade,
    bench_set_authority,
    bench_close,
    bench_extend_program,
    bench_set_authority_checked
);
criterion_main!(benches);
