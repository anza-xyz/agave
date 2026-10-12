use {
    bincode::serialize,
    criterion::{Criterion, criterion_group, criterion_main},
    solana_account::{AccountSharedData, state_traits::StateMutWincode as _},
    solana_clock::Clock,
    solana_epoch_schedule::EpochSchedule,
    solana_instruction::AccountMeta,
    solana_instruction_error::InstructionError,
    solana_program_runtime::{
        invoke_context::mock_process_instruction, solana_sbpf::program::BuiltinFunctionDefinition,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::{sysvar, vote::id},
    solana_sysvar_id::SysvarId,
    solana_vote_interface::state::BLS_PUBLIC_KEY_COMPRESSED_SIZE,
    solana_vote_program::{
        vote_instruction::VoteInstruction,
        vote_processor::Entrypoint,
        vote_state::{
            VoteAuthorize, VoteAuthorizeCheckedWithSeedArgs, VoteAuthorizeWithSeedArgs, VoteInit,
            VoteStateV3, VoterWithBLSArgs, create_bls_pubkey_and_proof_of_possession,
            create_v4_account_with_authorized,
        },
    },
};

fn create_sysvar_account<T>(value: &T) -> AccountSharedData
where
    T: wincode::SchemaWrite<solana_account::WincodeConfig, Src = T>
        + for<'de> wincode::SchemaRead<'de, solana_account::WincodeConfig, Dst = T>
        + SysvarId,
{
    let serialized_len =
        wincode::config::serialized_size(value, solana_account::WINCODE_CONFIG).unwrap() as usize;
    let canonical_data_len = match T::id() {
        sysvar::clock::ID => solana_clock::SIZE,
        sysvar::epoch_schedule::ID => solana_epoch_schedule::SIZE,
        sysvar::rent::ID => solana_rent::SIZE,
        sysvar::slot_hashes::ID => solana_slot_hashes::SIZE,
        id => panic!("unsupported sysvar: {id}"),
    };
    let required_data_len = canonical_data_len.max(serialized_len);
    AccountSharedData::new_data_with_space(1, value, required_data_len, &sysvar::id()).unwrap()
}

fn create_default_rent_account() -> AccountSharedData {
    create_sysvar_account(&Rent::free())
}

fn create_default_clock_account() -> AccountSharedData {
    create_sysvar_account(&Clock::default())
}

fn create_test_account() -> (Pubkey, AccountSharedData) {
    let rent = Rent::default();
    let balance = rent.minimum_balance(VoteStateV3::size_of());
    let node_pubkey = solana_pubkey::new_rand();
    let vote_pubkey = solana_pubkey::new_rand();
    (
        vote_pubkey,
        create_v4_account_with_authorized(
            &node_pubkey,
            &vote_pubkey,
            [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
            &vote_pubkey,
            0,
            &vote_pubkey,
            0,
            &node_pubkey,
            balance,
        ),
    )
}

fn create_test_account_with_authorized() -> (Pubkey, Pubkey, Pubkey, AccountSharedData) {
    let node_pubkey = solana_pubkey::new_rand();
    let vote_pubkey = solana_pubkey::new_rand();
    let authorized_voter = solana_pubkey::new_rand();
    let authorized_withdrawer = solana_pubkey::new_rand();

    (
        vote_pubkey,
        authorized_voter,
        authorized_withdrawer,
        create_v4_account_with_authorized(
            &node_pubkey,
            &authorized_voter,
            [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
            &authorized_withdrawer,
            0,
            &authorized_withdrawer,
            0,
            &node_pubkey,
            100,
        ),
    )
}

/// Build the `VoterWithBLS` voter-authorization form for `vote_pubkey`.
///
/// Once `bls_pubkey_management_in_vote_account` is enabled (as it is under the
/// mock harness' all-features-enabled set), the legacy `VoteAuthorize::Voter`
/// form is rejected for accounts that have a BLS key, so a fresh BLS pubkey and
/// proof-of-possession must be supplied.
fn voter_with_bls(vote_pubkey: &Pubkey) -> VoteAuthorize {
    let (bls_pubkey, bls_proof_of_possession) =
        create_bls_pubkey_and_proof_of_possession(vote_pubkey);
    VoteAuthorize::VoterWithBLS(VoterWithBLSArgs {
        bls_pubkey,
        bls_proof_of_possession,
    })
}

fn process_instruction(
    instruction_data: &[u8],
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
    expected_result: Result<(), InstructionError>,
) -> Vec<AccountSharedData> {
    mock_process_instruction(
        &id(),
        instruction_data,
        transaction_accounts,
        instruction_accounts,
        expected_result,
        Entrypoint::register,
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
        let clock_account = create_sysvar_account(&clock);
        let instruction_data = serialize(&VoteInstruction::Authorize(
            authorized_voter_pubkey,
            voter_with_bls(&vote_pubkey),
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
        let vote_account = AccountSharedData::new(100, VoteStateV3::size_of(), &id());
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

struct BenchWithdraw {
    instruction_data: Vec<u8>,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl BenchWithdraw {
    pub fn new() -> Self {
        let (vote_pubkey, vote_account) = create_test_account();
        let authorized_withdrawer_pubkey = solana_pubkey::new_rand();
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (sysvar::clock::id(), create_default_clock_account()),
            (sysvar::rent::id(), create_default_rent_account()),
            (authorized_withdrawer_pubkey, AccountSharedData::default()),
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
        let instruction_data = serialize(&VoteInstruction::Authorize(
            authorized_withdrawer_pubkey,
            VoteAuthorize::Withdrawer,
        ))
        .unwrap();

        Self {
            instruction_data,
            transaction_accounts,
            instruction_accounts,
        }
    }

    fn run(&self) {
        // should pass, withdraw using authorized_withdrawer to authorized_withdrawer's account
        let _accounts = process_instruction(
            &self.instruction_data,
            self.transaction_accounts.clone(),
            self.instruction_accounts.clone(),
            Ok(()),
        );
    }
}

struct BenchUpdateValidatorIdentity {
    instruction_data: Vec<u8>,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl BenchUpdateValidatorIdentity {
    pub fn new() -> Self {
        let (vote_pubkey, _authorized_voter, authorized_withdrawer, vote_account) =
            create_test_account_with_authorized();

        let node_pubkey = solana_pubkey::new_rand();
        let instruction_data = serialize(&VoteInstruction::UpdateValidatorIdentity).unwrap();
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (node_pubkey, AccountSharedData::default()),
            (authorized_withdrawer, AccountSharedData::default()),
        ];
        let instruction_accounts = vec![
            AccountMeta {
                // `[WRITE]` Vote account to be updated with the given authority public key
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                // `[SIGNER]` New validator identity (node_pubkey)
                pubkey: node_pubkey,
                is_signer: true,
                is_writable: false,
            },
            AccountMeta {
                // `[SIGNER]` Withdraw authority
                pubkey: authorized_withdrawer,
                is_signer: true,
                is_writable: false,
            },
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

struct BenchUpdateCommission {
    instruction_data: Vec<u8>,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl BenchUpdateCommission {
    fn new() -> Self {
        let (vote_pubkey, _authorized_voter, authorized_withdrawer, vote_account) =
            create_test_account_with_authorized();
        let instruction_data = serialize(&VoteInstruction::UpdateCommission(u8::MAX)).unwrap();
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (authorized_withdrawer, AccountSharedData::default()),
            // Add the sysvar accounts so they're in the cache for mock processing
            (
                sysvar::clock::id(),
                create_sysvar_account(&Clock::default()),
            ),
            (
                sysvar::epoch_schedule::id(),
                create_sysvar_account(&EpochSchedule::without_warmup()),
            ),
        ];
        let instruction_accounts = vec![
            AccountMeta {
                // `[WRITE]` Vote account to be updated
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                // `[SIGNER]` Withdraw authority
                pubkey: authorized_withdrawer,
                is_signer: true,
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

struct BenchAuthorizeChecked {
    instruction_data: Vec<u8>,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl BenchAuthorizeChecked {
    fn new() -> Self {
        let vote_pubkey = Pubkey::new_unique();
        let new_authorized_pubkey = Pubkey::new_unique();
        let default_authorized_pubkey = Pubkey::default();
        let node_pubkey = solana_pubkey::new_rand();
        // Initialized V4 vote account whose current voter/withdrawer authority is
        // `default_authorized_pubkey` (the signer below).
        let vote_account = create_v4_account_with_authorized(
            &node_pubkey,
            &default_authorized_pubkey,
            [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
            &default_authorized_pubkey,
            0,
            &default_authorized_pubkey,
            0,
            &node_pubkey,
            100,
        );
        let clock_address = sysvar::clock::id();
        let clock_account = create_sysvar_account(&Clock::default());
        let authorized_account = AccountSharedData::new(0, 0, &Pubkey::new_unique());
        let new_authorized_account = AccountSharedData::new(0, 0, &Pubkey::new_unique());
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (clock_address, clock_account),
            (default_authorized_pubkey, authorized_account),
            (new_authorized_pubkey, new_authorized_account),
        ];
        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                pubkey: clock_address,
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: default_authorized_pubkey,
                is_signer: true,
                is_writable: false,
            },
            AccountMeta {
                pubkey: new_authorized_pubkey,
                is_signer: true,
                is_writable: false,
            },
        ];

        let instruction_data = serialize(&VoteInstruction::AuthorizeChecked(voter_with_bls(
            &vote_pubkey,
        )))
        .unwrap();
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

struct BenchAuthorizeWithSeed {
    instruction_data: Vec<u8>,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl BenchAuthorizeWithSeed {
    fn new() -> Self {
        let vote_pubkey = Pubkey::new_unique();
        let voter_base_key = Pubkey::new_unique();
        let voter_owner = Pubkey::new_unique();
        let voter_seed = String::from("VOTER_SEED");
        let new_voter_pubkey = Pubkey::new_unique();
        let clock = Clock {
            epoch: 1,
            leader_schedule_epoch: 2,
            ..Clock::default()
        };
        let withdrawer_base_key = Pubkey::new_unique();
        let withdrawer_owner = Pubkey::new_unique();
        let withdrawer_seed = String::from("WITHDRAWER_SEED");

        let authorized_voter =
            Pubkey::create_with_seed(&voter_base_key, voter_seed.as_str(), &voter_owner).unwrap();
        let authorized_withdrawer = Pubkey::create_with_seed(
            &withdrawer_base_key,
            withdrawer_seed.as_str(),
            &withdrawer_owner,
        )
        .unwrap();
        let node_pubkey = Pubkey::new_unique();
        let vote_account = create_v4_account_with_authorized(
            &node_pubkey,
            &authorized_voter,
            [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
            &authorized_withdrawer,
            0,
            &authorized_withdrawer,
            0,
            &node_pubkey,
            100,
        );
        let clock_account = create_sysvar_account(&clock);
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (sysvar::clock::id(), clock_account),
            (voter_base_key, AccountSharedData::default()),
        ];
        let instruction_accounts = vec![
            AccountMeta {
                // `[Write]` Vote account to be updated
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                // `[]` Clock sysvar
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                // `[SIGNER]` Base key of current Voter or Withdrawer authority's derived key
                pubkey: voter_base_key,
                is_signer: true,
                is_writable: false,
            },
        ];

        let authorization_type = voter_with_bls(&vote_pubkey);
        let instruction_data = serialize(&VoteInstruction::AuthorizeWithSeed(
            VoteAuthorizeWithSeedArgs {
                authorization_type,
                current_authority_derived_key_owner: voter_owner,
                current_authority_derived_key_seed: voter_seed,
                new_authority: new_voter_pubkey,
            },
        ))
        .unwrap();
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

struct BenchAuthorizeCheckedWithSeed {
    instruction_data: Vec<u8>,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl BenchAuthorizeCheckedWithSeed {
    fn new() -> Self {
        let vote_pubkey = Pubkey::new_unique();
        let authorization_type = voter_with_bls(&vote_pubkey);
        let current_authority_base_key = Pubkey::new_unique();
        let current_authority_owner = Pubkey::new_unique();
        let current_authority_seed = String::from("VOTER_SEED");
        let withdrawer_base_key = Pubkey::new_unique();
        let withdrawer_owner = Pubkey::new_unique();
        let withdrawer_seed = String::from("WITHDRAWER_SEED");
        let authorized_voter = Pubkey::create_with_seed(
            &current_authority_base_key,
            current_authority_seed.as_str(),
            &current_authority_owner,
        )
        .unwrap();
        let authorized_withdrawer = Pubkey::create_with_seed(
            &withdrawer_base_key,
            withdrawer_seed.as_str(),
            &withdrawer_owner,
        )
        .unwrap();
        let node_pubkey = Pubkey::new_unique();
        let vote_account = create_v4_account_with_authorized(
            &node_pubkey,
            &authorized_voter,
            [0u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE],
            &authorized_withdrawer,
            0,
            &authorized_withdrawer,
            0,
            &node_pubkey,
            100,
        );
        let new_authority_pubkey = Pubkey::new_unique();
        let clock = Clock {
            epoch: 1,
            leader_schedule_epoch: 2,
            ..Clock::default()
        };
        let clock_account = create_sysvar_account(&clock);
        let transaction_accounts = vec![
            (vote_pubkey, vote_account),
            (sysvar::clock::id(), clock_account),
            (current_authority_base_key, AccountSharedData::default()),
            (new_authority_pubkey, AccountSharedData::default()),
        ];
        let instruction_accounts = vec![
            AccountMeta {
                // `[Write]` Vote account to be updated
                pubkey: vote_pubkey,
                is_signer: false,
                is_writable: true,
            },
            AccountMeta {
                // `[]` Clock sysvar
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                // `[SIGNER]` Base key of current Voter or Withdrawer authority's derived key
                pubkey: current_authority_base_key,
                is_signer: true,
                is_writable: false,
            },
            AccountMeta {
                // `[SIGNER]` New vote or withdraw authority
                pubkey: new_authority_pubkey,
                is_signer: true,
                is_writable: false,
            },
        ];
        let instruction_data = serialize(&VoteInstruction::AuthorizeCheckedWithSeed(
            VoteAuthorizeCheckedWithSeedArgs {
                authorization_type,
                current_authority_derived_key_owner: current_authority_owner,
                current_authority_derived_key_seed: current_authority_seed,
            },
        ))
        .unwrap();
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

fn bench_withdraw(c: &mut Criterion) {
    let test_setup = BenchWithdraw::new();
    c.bench_function("vote_instruction_withdraw", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_update_validator_identity(c: &mut Criterion) {
    let test_setup = BenchUpdateValidatorIdentity::new();
    c.bench_function("vote_update_validator_identity", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_update_commission(c: &mut Criterion) {
    let test_setup = BenchUpdateCommission::new();
    c.bench_function("vote_update_commission", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_authorize_checked(c: &mut Criterion) {
    let test_setup = BenchAuthorizeChecked::new();
    c.bench_function("vote_authorize_checked", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_authorize_with_seed(c: &mut Criterion) {
    let test_setup = BenchAuthorizeWithSeed::new();
    c.bench_function("vote_authorize_with_seed", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_authorize_checked_with_seed(c: &mut Criterion) {
    let test_setup = BenchAuthorizeCheckedWithSeed::new();
    c.bench_function("vote_authorize_checked_with_seed", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

criterion_group!(
    benches,
    bench_initialize_account,
    bench_authorize,
    bench_withdraw,
    bench_update_validator_identity,
    bench_update_commission,
    bench_authorize_checked,
    bench_authorize_with_seed,
    bench_authorize_checked_with_seed,
);
criterion_main!(benches);
