use {
    bincode::serialize,
    criterion::{criterion_group, criterion_main, Criterion},
    solana_account::{self as account, AccountSharedData},
    solana_clock::Clock,
    solana_instruction::{error::InstructionError, AccountMeta},
    solana_program_runtime::invoke_context::mock_process_instruction,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk::sysvar,
    solana_sdk_ids::vote::id,
    solana_vote_program::{
        vote_instruction::VoteInstruction,
        vote_processor::Entrypoint,
        vote_state::{create_account, VoteAuthorize, VoteInit, VoteState},
    },
};

fn create_default_rent_account() -> AccountSharedData {
    account::create_account_shared_data_for_test(&Rent::free())
}

fn create_default_clock_account() -> AccountSharedData {
    account::create_account_shared_data_for_test(&Clock::default())
}

fn create_test_account() -> (Pubkey, AccountSharedData) {
    let rent = Rent::default();
    let balance = VoteState::get_rent_exempt_reserve(&rent);
    let vote_pubkey = solana_pubkey::new_rand();
    (
        vote_pubkey,
        create_account(&vote_pubkey, &solana_pubkey::new_rand(), 0, balance),
    )
}

fn process_instruction(
    instruction_data: &[u8],
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
    expected_result: Result<(), InstructionError>,
) -> Vec<AccountSharedData> {
    mock_process_instruction(
        &id(),
        Vec::new(),
        instruction_data,
        transaction_accounts,
        instruction_accounts,
        expected_result,
        Entrypoint::vm,
        |_invoke_context| {},
        |_invoke_context| {},
    )
}

struct BenchAuthorize {
    instruction_data: Vec<u8>,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl BenchAuthorize {
    fn new() -> Self {
        let (vote_pubkey, vote_account) = create_test_account();
        let authorized_voter_pubkey = solana_pubkey::new_rand();
        let clock = Clock {
            epoch: 1,
            leader_schedule_epoch: 2,
            ..Clock::default()
        };
        let clock_account = account::create_account_shared_data_for_test(&clock);
        let instruction_data = serialize(&VoteInstruction::Authorize(
            authorized_voter_pubkey,
            VoteAuthorize::Voter,
        ))
        .unwrap();
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (sysvar::clock::id(), clock_account),
            (authorized_voter_pubkey, AccountSharedData::default()),
        ];
        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: true,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
        ];
        Self {
            instruction_data,
            transaction_accounts,
            instruction_accounts,
        }
    }

    fn run(&self) {
        let _accounts = process_instruction(
            &self.instruction_data,
            self.transaction_accounts.clone(),
            self.instruction_accounts.clone(),
            Ok(()),
        );
    }
}

struct BenchInitializeAccount {
    instruction_data: Vec<u8>,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl BenchInitializeAccount {
    fn new() -> Self {
        let vote_pubkey = solana_pubkey::new_rand();
        let vote_account = AccountSharedData::new(100, VoteState::size_of(), &id());
        let node_pubkey = solana_pubkey::new_rand();
        let node_account = AccountSharedData::default();
        let instruction_data = serialize(&VoteInstruction::InitializeAccount(VoteInit {
            node_pubkey,
            authorized_voter: vote_pubkey,
            authorized_withdrawer: vote_pubkey,
            commission: 0,
        }))
        .unwrap();
        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
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
                pubkey: node_pubkey,
                is_signer: true,
                is_writable: false,
            },
        ];
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (sysvar::rent::id(), create_default_rent_account()),
            (sysvar::clock::id(), create_default_clock_account()),
            (node_pubkey, node_account),
        ];
        Self {
            instruction_data,
            transaction_accounts,
            instruction_accounts,
        }
    }
    pub fn run(&self) {
        let _accounts = process_instruction(
            &self.instruction_data,
            self.transaction_accounts.clone(),
            self.instruction_accounts.clone(),
            Ok(()),
        );
    }
}

fn bench_initialize_account(c: &mut Criterion) {
    let test_setup = BenchInitializeAccount::new();
    c.bench_function("vote_instruction_initialize_account", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_authorize(c: &mut Criterion) {
    let test_setup = BenchAuthorize::new();
    c.bench_function("vote_instruction_authorize", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

criterion_group!(benches, bench_initialize_account, bench_authorize);
criterion_main!(benches);
