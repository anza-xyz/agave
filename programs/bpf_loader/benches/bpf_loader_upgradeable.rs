use {
    criterion::{criterion_group, criterion_main, Criterion},
    solana_bpf_loader_program::Entrypoint,
    solana_program_runtime::invoke_context::mock_process_instruction,
    solana_sdk::{
        account::{
            create_account_for_test, create_account_shared_data_for_test, AccountSharedData,
            WritableAccount,
        },
        account_utils::StateMut,
        bpf_loader_upgradeable::{self, UpgradeableLoaderState},
        clock::Clock,
        instruction::AccountMeta,
        loader_upgradeable_instruction::UpgradeableLoaderInstruction,
        pubkey::Pubkey,
        rent::Rent,
        system_program, sysvar,
    },
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
// TODO(klykov) This parameter might be sensitive, try with different values
const PROGRAM_BUFFER_SIZE: usize = 1024;

impl TestSetup {
    fn new_initialize_buffer() -> Self {
        let loader_address = bpf_loader_upgradeable::id();
        let buffer_address = Pubkey::new_unique();
        let authority_address = Pubkey::new_unique();

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

        let instruction_accounts = vec![
            AccountMeta {
                pubkey: buffer_address,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: authority_address,
                is_signer: false,
                is_writable: false,
            },
        ];
        let instruction_data =
            bincode::serialize(&UpgradeableLoaderInstruction::InitializeBuffer).unwrap();

        Self {
            loader_address,
            authority_address,
            transaction_accounts,
            instruction_accounts,
            instruction_data,
        }
    }

    fn new_write() -> Self {
        let loader_address = bpf_loader_upgradeable::id();
        let buffer_address = Pubkey::new_unique();
        let authority_address = Pubkey::new_unique();

        let mut transaction_accounts = vec![
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
        transaction_accounts[0]
            .1
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(authority_address),
            })
            .unwrap();

        let instruction_accounts = vec![
            AccountMeta {
                pubkey: buffer_address,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: authority_address,
                is_signer: true,
                is_writable: false,
            },
        ];

        let instruction_data = bincode::serialize(&UpgradeableLoaderInstruction::Write {
            offset: 0,
            bytes: vec![64; PROGRAM_BUFFER_SIZE],
        })
        .unwrap();

        Self {
            loader_address,
            authority_address,
            transaction_accounts,
            instruction_accounts,
            instruction_data,
        }
    }

    fn new_deploy_with_max_data_len() -> Self {
        let loader_address = bpf_loader_upgradeable::id();

        let recipient_address = Pubkey::new_unique();
        let authority_address = Pubkey::new_unique();

        let buffer_address = Pubkey::new_unique();
        let mut buffer_account = AccountSharedData::new(
            ACCOUNT_BALANCE,
            UpgradeableLoaderState::size_of_buffer(PROGRAM_BUFFER_SIZE),
            &loader_address,
        );
        buffer_account
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(authority_address),
            })
            .unwrap();

        let program_address = Pubkey::new_unique();
        let mut program_account = AccountSharedData::new(
            ACCOUNT_BALANCE,
            UpgradeableLoaderState::size_of_program(),
            &loader_address,
        );
        program_account.set_executable(true);
        program_account
            .set_state(&UpgradeableLoaderState::Uninitialized)
            .unwrap();

        let (programdata_address, _) =
            Pubkey::find_program_address(&[program_address.as_ref()], &loader_address);
        let mut programdata_account = AccountSharedData::new(
            ACCOUNT_BALANCE,
            UpgradeableLoaderState::size_of_programdata(PROGRAM_BUFFER_SIZE),
            &loader_address,
        );
        programdata_account
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: Some(authority_address),
            })
            .unwrap();

        let clock_account = create_account_shared_data_for_test(&Clock {
            slot: 1,
            ..Clock::default()
        });

        let transaction_accounts = vec![
            (
                recipient_address,
                AccountSharedData::new(ACCOUNT_BALANCE, 0, &Pubkey::new_unique()),
            ),
            (programdata_address, programdata_account),
            (program_address, program_account),
            (buffer_address, buffer_account),
            (
                sysvar::rent::id(),
                create_account_shared_data_for_test(&Rent::default()),
            ),
            (sysvar::clock::id(), clock_account),
            (
                system_program::id(),
                AccountSharedData::new(0, 0, &system_program::id()),
            ),
            (
                authority_address,
                AccountSharedData::new(ACCOUNT_BALANCE, 0, &loader_address),
            ),
        ];

        let instruction_accounts = vec![
            AccountMeta {
                pubkey: recipient_address,
                is_signer: true,
                is_writable: true,
            },
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
                pubkey: buffer_address,
                is_signer: false,
                is_writable: true, //or false?
            },
            AccountMeta::new_readonly(sysvar::rent::id(), false),
            AccountMeta::new_readonly(sysvar::clock::id(), false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta {
                pubkey: authority_address,
                is_signer: true,
                is_writable: false,
            },
        ];

        let instruction_data =
            bincode::serialize(&UpgradeableLoaderInstruction::DeployWithMaxDataLen {
                max_data_len: PROGRAM_BUFFER_SIZE,
            })
            .unwrap();
        Self {
            loader_address,
            authority_address,
            transaction_accounts,
            instruction_accounts,
            instruction_data,
        }
    }

    fn run(&self) {
        let accounts = mock_process_instruction(
            &self.loader_address,
            Vec::new(),
            &self.instruction_data,
            self.transaction_accounts.clone(),
            self.instruction_accounts.clone(),
            Ok(()), //expected_result,
            Entrypoint::vm,
            |_invoke_context| {},
            |_invoke_context| {},
        );
        let state: UpgradeableLoaderState = accounts.first().unwrap().state().unwrap();
        assert_eq!(
            state,
            UpgradeableLoaderState::Buffer {
                authority_address: Some(self.authority_address)
            }
        );
    }
}

fn bench_initialize_buffer(c: &mut Criterion) {
    let test_setup = TestSetup::new_initialize_buffer();

    c.bench_function("initialize_buffer", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_write(c: &mut Criterion) {
    let test_setup = TestSetup::new_write();

    c.bench_function("write", |bencher| {
        bencher.iter(|| {
            test_setup.run();
        })
    });
}

fn bench_deploy_with_max_data_len(c: &mut Criterion) {
    solana_logger::setup();
    let test_setup = TestSetup::new_deploy_with_max_data_len();

    c.bench_function("deploy_with_max_data_len", |bencher| {
        bencher.iter(|| {
            test_setup.run();
        })
    });
}

criterion_group!(
    benches,
    bench_initialize_buffer,
    bench_write,
    bench_deploy_with_max_data_len
);
criterion_main!(benches);
