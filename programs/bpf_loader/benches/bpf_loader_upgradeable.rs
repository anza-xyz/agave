use {
    criterion::{criterion_group, criterion_main, Criterion},
    solana_account::{state_traits::StateMut, AccountSharedData},
    solana_bpf_loader_program::Entrypoint,
    solana_instruction::AccountMeta,
    solana_program::{
        bpf_loader_upgradeable::{self, UpgradeableLoaderState},
        loader_upgradeable_instruction::UpgradeableLoaderInstruction,
    },
    solana_program_runtime::invoke_context::mock_process_instruction,
    solana_pubkey::Pubkey,
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
    bench_set_authority,
    bench_close,
    bench_set_authority_checked
);
criterion_main!(benches);
