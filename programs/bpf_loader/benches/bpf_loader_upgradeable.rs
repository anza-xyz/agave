use {
    criterion::{criterion_group, criterion_main, Criterion},
    solana_bpf_loader_program::Entrypoint,
    solana_program_runtime::invoke_context::mock_process_instruction,
    solana_sdk::{
        account::AccountSharedData,
        account_utils::StateMut,
        bpf_loader_upgradeable::{self, UpgradeableLoaderState},
        instruction::AccountMeta,
        loader_upgradeable_instruction::UpgradeableLoaderInstruction,
        pubkey::Pubkey,
    },
};

#[derive(Default)]
struct TestSetup {
    loader_address: Pubkey,
    buffer_address: Pubkey,
    authority_address: Pubkey,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,

    instruction_accounts: Vec<AccountMeta>,
    instruction_data: Vec<u8>,
}

const ACCOUNT_BALANCE: u64 = u64::MAX / 4;
// TODO(klykov) This parameter might be sensitive, try with different values
const PROGRAM_BUFFER_SIZE: usize = 1024;

impl TestSetup {
    fn new() -> Self {
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

        Self {
            loader_address,
            buffer_address,
            authority_address,
            transaction_accounts,
            ..TestSetup::default()
        }
    }

    fn prep_initialize_buffer(&mut self) {
        self.instruction_accounts = vec![
            AccountMeta {
                pubkey: self.buffer_address,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: self.authority_address,
                is_signer: false,
                is_writable: false,
            },
        ];
        self.instruction_data =
            bincode::serialize(&UpgradeableLoaderInstruction::InitializeBuffer).unwrap();
    }

    fn prep_write(&mut self) {
        self.instruction_accounts = vec![
            AccountMeta {
                pubkey: self.buffer_address,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: self.authority_address,
                is_signer: true,
                is_writable: false,
            },
        ];

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

criterion_group!(benches, bench_initialize_buffer, bench_write);
criterion_main!(benches);
