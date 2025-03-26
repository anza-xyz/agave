use {
    agave_feature_set::FeatureSet,
    bincode::serialize,
    criterion::{black_box, criterion_group, criterion_main, Criterion},
    solana_account::{create_account_shared_data_for_test, AccountSharedData, WritableAccount},
    solana_clock::{Clock, Epoch},
    solana_instruction::AccountMeta,
    solana_program_runtime::invoke_context::mock_process_instruction,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::sysvar::{clock, rent, stake_history},
    solana_stake_interface::{
        instruction::{
            self, AuthorizeCheckedWithSeedArgs, AuthorizeWithSeedArgs, LockupArgs,
            LockupCheckedArgs, StakeInstruction,
        },
        stake_flags::StakeFlags,
        state::{Authorized, Lockup, StakeAuthorize, StakeStateV2},
    },
    solana_stake_program::{
        stake_instruction,
        stake_state::{Delegation, Meta, Stake},
    },
    solana_sysvar::stake_history::StakeHistory,
    solana_vote_interface::state::{VoteState, VoteStateVersions},
    solana_vote_program::vote_state,
    std::sync::Arc,
};

const ACCOUNT_BALANCE: u64 = u64::MAX / 4; // enough lamports for tests

struct TestSetup {
    feature_set: Arc<FeatureSet>,
    stake_address: Pubkey,
    stake_account: AccountSharedData,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl TestSetup {
    fn new() -> Self {
        let stake_account = AccountSharedData::new(
            ACCOUNT_BALANCE,
            StakeStateV2::size_of(),
            &solana_stake_program::id(),
        );
        let stake_address = solana_pubkey::Pubkey::new_unique();
        Self {
            // some stake instructions are behind feature gate, enable all
            // feature gates to bench all instructions
            feature_set: Arc::new(FeatureSet::all_enabled()),
            stake_address,
            stake_account: stake_account.clone(),
            transaction_accounts: vec![(stake_address, stake_account)],
            instruction_accounts: vec![AccountMeta {
                pubkey: stake_address,
                is_signer: false,
                is_writable: true,
            }],
        }
    }

    fn add_account(&mut self, id: Pubkey, account: AccountSharedData) {
        self.transaction_accounts.push((id, account));
        self.instruction_accounts.push(AccountMeta {
            pubkey: id,
            is_signer: false,
            is_writable: true,
        });
    }

    fn add_account_signer(&mut self, id: Pubkey, account: AccountSharedData) {
        self.transaction_accounts.push((id, account));
        self.instruction_accounts.push(AccountMeta {
            pubkey: id,
            is_signer: true,
            is_writable: true,
        });
    }

    fn initialize_stake_account(&mut self) {
        let initialized_stake_account = AccountSharedData::new_data_with_space(
            ACCOUNT_BALANCE,
            &StakeStateV2::Initialized(Meta::auto(&self.stake_address)),
            StakeStateV2::size_of(),
            &solana_stake_program::id(),
        )
        .unwrap();

        self.stake_account = initialized_stake_account.clone();
        self.transaction_accounts[0] = (self.stake_address, initialized_stake_account);
        // also make stake address a signer
        self.instruction_accounts[0] = AccountMeta {
            pubkey: self.stake_address,
            is_signer: true,
            is_writable: true,
        };
    }

    fn initialize_stake_account_with_seed(&mut self, seed: &str, authorized_owner: &Pubkey) {
        self.stake_address =
            Pubkey::create_with_seed(authorized_owner, seed, authorized_owner).unwrap();
        self.initialize_stake_account();
    }

    // config withdraw authority, returns authorized withdrwer's pubkey
    fn config_withdraw_authority(&mut self) -> Pubkey {
        let withdraw_authority_address = Pubkey::new_unique();

        let instruction = instruction::authorize(
            &self.stake_address,
            &self.stake_address,
            &withdraw_authority_address,
            StakeAuthorize::Withdrawer,
            None,
        );

        let transaction_accounts = vec![
            (self.stake_address, self.stake_account.clone()),
            (
                clock::id(),
                create_account_shared_data_for_test(&Clock::default()),
            ),
            (withdraw_authority_address, AccountSharedData::default()),
        ];

        let accounts = mock_process_instruction(
            &solana_stake_program::id(),
            Vec::new(),
            &instruction.data,
            transaction_accounts,
            instruction.accounts.clone(),
            Ok(()),
            stake_instruction::Entrypoint::vm,
            |invoke_context| {
                invoke_context.mock_set_feature_set(Arc::clone(&self.feature_set));
            },
            |_invoke_context| {},
        );
        // update stake account
        self.transaction_accounts[0] = (self.stake_address, accounts[0].clone());

        withdraw_authority_address
    }

    fn delegate_stake(&mut self) {
        let vote_address = Pubkey::new_unique();

        let instruction =
            instruction::delegate_stake(&self.stake_address, &self.stake_address, &vote_address);

        let transaction_accounts = vec![
            (self.stake_address, self.stake_account.clone()),
            (
                vote_address,
                vote_state::create_account(&vote_address, &Pubkey::new_unique(), 0, 100),
            ),
            (
                clock::id(),
                create_account_shared_data_for_test(&Clock::default()),
            ),
            (
                stake_history::id(),
                create_account_shared_data_for_test(&StakeHistory::default()),
            ),
        ];

        let accounts = mock_process_instruction(
            &solana_stake_program::id(),
            Vec::new(),
            &instruction.data,
            transaction_accounts,
            instruction.accounts.clone(),
            Ok(()),
            stake_instruction::Entrypoint::vm,
            |invoke_context| {
                invoke_context.mock_set_feature_set(Arc::clone(&self.feature_set));
            },
            |_invoke_context| {},
        );
        self.stake_account = accounts[0].clone();
        self.stake_account.set_lamports(ACCOUNT_BALANCE * 2);
        self.transaction_accounts[0] = (self.stake_address, self.stake_account.clone());
    }

    fn run(&self, instruction_data: &[u8]) {
        mock_process_instruction(
            &solana_stake_program::id(),
            Vec::new(),
            instruction_data,
            self.transaction_accounts.clone(),
            self.instruction_accounts.clone(),
            Ok(()), //expected_result,
            stake_instruction::Entrypoint::vm,
            |invoke_context| {
                invoke_context.mock_set_feature_set(Arc::clone(&self.feature_set));
            },
            |_invoke_context| {},
        );
    }
}

fn bench_initialize(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.add_account(
        solana_sdk_ids::sysvar::rent::id(),
        create_account_shared_data_for_test(&Rent::default()),
    );

    let instruction_data = serialize(&StakeInstruction::Initialize(
        Authorized::auto(&test_setup.stake_address),
        Lockup::default(),
    ))
    .unwrap();
    c.bench_function("initialize", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_initialize_checked(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.add_account(
        solana_sdk_ids::sysvar::rent::id(),
        create_account_shared_data_for_test(&Rent::default()),
    );
    // add staker account
    test_setup.add_account(Pubkey::new_unique(), AccountSharedData::default());
    // add withdrawer account
    test_setup.add_account_signer(Pubkey::new_unique(), AccountSharedData::default());

    let instruction_data = serialize(&StakeInstruction::InitializeChecked).unwrap();

    c.bench_function("initialize_checked", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_authorize_staker(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account();
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );
    let authority_address = Pubkey::new_unique();
    test_setup.add_account(authority_address, AccountSharedData::default());

    let instruction_data = serialize(&StakeInstruction::Authorize(
        authority_address,
        StakeAuthorize::Staker,
    ))
    .unwrap();

    c.bench_function("authorize_staker", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_authorize_withdrawer(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account();
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );
    // add authority address
    let authority_address = Pubkey::new_unique();
    test_setup.add_account(authority_address, AccountSharedData::default());

    let instruction_data = serialize(&StakeInstruction::Authorize(
        authority_address,
        StakeAuthorize::Withdrawer,
    ))
    .unwrap();

    c.bench_function("authorize_withdrawer", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_authorize_staker_with_seed(c: &mut Criterion) {
    let seed = "test test";
    let authorize_address = Pubkey::new_unique();

    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account_with_seed(seed, &authorize_address);
    test_setup.add_account_signer(authorize_address, AccountSharedData::default());
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );

    let instruction_data = serialize(&StakeInstruction::AuthorizeWithSeed(
        AuthorizeWithSeedArgs {
            new_authorized_pubkey: Pubkey::new_unique(),
            stake_authorize: StakeAuthorize::Staker,
            authority_seed: seed.to_string(),
            authority_owner: authorize_address,
        },
    ))
    .unwrap();

    c.bench_function("authorize_staker_with_seed", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_authorize_withdrawer_with_seed(c: &mut Criterion) {
    let seed = "test test";
    let authorize_address = Pubkey::new_unique();

    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account_with_seed(seed, &authorize_address);
    test_setup.add_account_signer(authorize_address, AccountSharedData::default());
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );

    let instruction_data = serialize(&StakeInstruction::AuthorizeWithSeed(
        AuthorizeWithSeedArgs {
            new_authorized_pubkey: Pubkey::new_unique(),
            stake_authorize: StakeAuthorize::Withdrawer,
            authority_seed: seed.to_string(),
            authority_owner: authorize_address,
        },
    ))
    .unwrap();

    c.bench_function("authorize_withdrawer_with_seed", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_authorize_staker_checked(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account();
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );
    // add authorized address as signer
    test_setup.add_account_signer(Pubkey::new_unique(), AccountSharedData::default());
    // add staker account as signer
    test_setup.add_account_signer(Pubkey::new_unique(), AccountSharedData::default());

    let instruction_data =
        serialize(&StakeInstruction::AuthorizeChecked(StakeAuthorize::Staker)).unwrap();

    c.bench_function("authorize_staker_checked", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_authorize_withdrawer_checked(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account();
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );
    // add authorized address as signer
    test_setup.add_account_signer(Pubkey::new_unique(), AccountSharedData::default());
    // add staker account as signer
    test_setup.add_account_signer(Pubkey::new_unique(), AccountSharedData::default());

    let instruction_data = serialize(&StakeInstruction::AuthorizeChecked(
        StakeAuthorize::Withdrawer,
    ))
    .unwrap();

    c.bench_function("authorize_withdrawer_checked", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_authorize_staker_checked_with_seed(c: &mut Criterion) {
    let seed = "test test";
    let authorize_address = Pubkey::new_unique();

    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account_with_seed(seed, &authorize_address);
    // add authorized address as signer
    test_setup.add_account_signer(authorize_address, AccountSharedData::default());
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );
    // add new authorize account as signer
    test_setup.add_account_signer(Pubkey::new_unique(), AccountSharedData::default());

    let instruction_data = serialize(&StakeInstruction::AuthorizeCheckedWithSeed(
        AuthorizeCheckedWithSeedArgs {
            stake_authorize: StakeAuthorize::Staker,
            authority_seed: seed.to_string(),
            authority_owner: authorize_address,
        },
    ))
    .unwrap();

    c.bench_function("authorize_staker_checked_with_seed", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_authorize_withdrawer_checked_with_seed(c: &mut Criterion) {
    let seed = "test test";
    let authorize_address = Pubkey::new_unique();

    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account_with_seed(seed, &authorize_address);
    // add authorized address as signer
    test_setup.add_account_signer(authorize_address, AccountSharedData::default());
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );
    // add new authorize account as signer
    test_setup.add_account_signer(Pubkey::new_unique(), AccountSharedData::default());

    let instruction_data = serialize(&StakeInstruction::AuthorizeCheckedWithSeed(
        AuthorizeCheckedWithSeedArgs {
            stake_authorize: StakeAuthorize::Withdrawer,
            authority_seed: seed.to_string(),
            authority_owner: authorize_address,
        },
    ))
    .unwrap();

    c.bench_function("authorize_withdrawer_checked_with_seed", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_set_lockup(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account();
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );

    let instruction_data = serialize(&StakeInstruction::SetLockup(LockupArgs {
        unix_timestamp: None,
        epoch: Some(1),
        custodian: None,
    }))
    .unwrap();

    c.bench_function("set_lockup", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_set_lockup_checked(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account();
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );

    let instruction_data = serialize(&StakeInstruction::SetLockupChecked(LockupCheckedArgs {
        unix_timestamp: None,
        epoch: Some(1),
    }))
    .unwrap();

    c.bench_function("set_lockup_checked", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_withdraw(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account();
    let withdraw_authority_address = test_setup.config_withdraw_authority();

    // withdraw to pubkey
    test_setup.add_account(Pubkey::new_unique(), AccountSharedData::default());
    // clock
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );
    // stake history
    test_setup.add_account(
        stake_history::id(),
        create_account_shared_data_for_test(&StakeHistory::default()),
    );
    // withdrawer pubkey
    test_setup.add_account_signer(withdraw_authority_address, AccountSharedData::default());

    let instruction_data = serialize(&StakeInstruction::Withdraw(1)).unwrap();

    c.bench_function("withdraw", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_delegate_stake(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account();

    let vote_address = Pubkey::new_unique();
    let vote_account = vote_state::create_account(&vote_address, &Pubkey::new_unique(), 0, 100);
    test_setup.add_account(vote_address, vote_account);
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );
    test_setup.add_account(
        stake_history::id(),
        create_account_shared_data_for_test(&StakeHistory::default()),
    );
    // dummy config account to pass check
    test_setup.add_account(Pubkey::new_unique(), AccountSharedData::default());
    let instruction_data = serialize(&StakeInstruction::DelegateStake).unwrap();

    c.bench_function("delegate_stake", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_deactivate(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account();
    test_setup.delegate_stake();

    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );

    let instruction_data = serialize(&StakeInstruction::Deactivate).unwrap();

    c.bench_function("deactivate", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_split(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account();

    let split_to_address = Pubkey::new_unique();
    let split_to_account = AccountSharedData::new_data_with_space(
        0,
        &StakeStateV2::Uninitialized,
        StakeStateV2::size_of(),
        &solana_stake_program::id(),
    )
    .unwrap();

    test_setup.add_account(split_to_address, split_to_account);
    test_setup.add_account(
        rent::id(),
        create_account_shared_data_for_test(&Rent {
            lamports_per_byte_year: 0,
            ..Rent::default()
        }),
    );
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );
    test_setup.add_account(
        stake_history::id(),
        create_account_shared_data_for_test(&StakeHistory::default()),
    );

    let instruction_data = serialize(&StakeInstruction::Split(1)).unwrap();

    c.bench_function("split", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_merge(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account();

    let merge_from_address = Pubkey::new_unique();
    // merge from account has same authority as stake account for simplicity,
    // it also has lamports 0 to avoid `ArithmeticOverflow` to current stake account
    let merge_from_account = AccountSharedData::new_data_with_space(
        1,
        &StakeStateV2::Initialized(Meta::auto(&test_setup.stake_address)),
        StakeStateV2::size_of(),
        &solana_stake_program::id(),
    )
    .unwrap();

    test_setup.add_account(merge_from_address, merge_from_account);
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock::default()),
    );
    test_setup.add_account(
        stake_history::id(),
        create_account_shared_data_for_test(&StakeHistory::default()),
    );

    let instruction_data = serialize(&StakeInstruction::Merge).unwrap();

    c.bench_function("merge", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_get_minimum_delegation(c: &mut Criterion) {
    let test_setup = TestSetup::new();
    let instruction_data = serialize(&StakeInstruction::GetMinimumDelegation).unwrap();

    c.bench_function("get_minimum_delegation", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_deactivate_delinquent(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();

    // reference vote account has been consistently voting
    let mut vote_state = VoteState::default();
    for epoch in 0..=solana_stake_interface::MINIMUM_DELINQUENT_EPOCHS_FOR_DEACTIVATION {
        vote_state.increment_credits(epoch as Epoch, 1);
    }
    let reference_vote_address = Pubkey::new_unique();
    let reference_vote_account = AccountSharedData::new_data_with_space(
        1,
        &VoteStateVersions::new_current(vote_state),
        VoteState::size_of(),
        &solana_sdk_ids::vote::id(),
    )
    .unwrap();

    let vote_address = Pubkey::new_unique();
    let vote_account = vote_state::create_account(&vote_address, &Pubkey::new_unique(), 0, 100);
    test_setup.stake_account = AccountSharedData::new_data_with_space(
        1,
        &StakeStateV2::Stake(
            Meta::default(),
            Stake {
                delegation: Delegation::new(&vote_address, 1, 1),
                credits_observed: VoteState::default().credits(),
            },
            StakeFlags::empty(),
        ),
        StakeStateV2::size_of(),
        &solana_stake_program::id(),
    )
    .unwrap();
    test_setup.transaction_accounts[0] =
        (test_setup.stake_address, test_setup.stake_account.clone());

    test_setup.add_account(vote_address, vote_account);
    test_setup.add_account(reference_vote_address, reference_vote_account);
    test_setup.add_account(
        clock::id(),
        create_account_shared_data_for_test(&Clock {
            epoch: solana_stake_interface::MINIMUM_DELINQUENT_EPOCHS_FOR_DEACTIVATION as u64,
            ..Clock::default()
        }),
    );

    let instruction_data = serialize(&StakeInstruction::DeactivateDelinquent).unwrap();

    c.bench_function("deactivate_delinquent", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_move_stake(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account();
    test_setup.delegate_stake();

    let destination_stake_address = Pubkey::new_unique();
    let destination_stake_account = test_setup.transaction_accounts[0].1.clone();
    test_setup.add_account(destination_stake_address, destination_stake_account);
    test_setup.add_account_signer(test_setup.stake_address, AccountSharedData::default());
    test_setup.add_account(
        clock::id(),
        // advance epoch to fully activate source account
        create_account_shared_data_for_test(&Clock {
            epoch: 1_u64,
            ..Clock::default()
        }),
    );
    test_setup.add_account(
        stake_history::id(),
        create_account_shared_data_for_test(&StakeHistory::default()),
    );

    let instruction_data = serialize(&StakeInstruction::MoveStake(1)).unwrap();

    c.bench_function("move_stake", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

fn bench_move_lamports(c: &mut Criterion) {
    let mut test_setup = TestSetup::new();
    test_setup.initialize_stake_account();
    test_setup.delegate_stake();

    let destination_stake_address = Pubkey::new_unique();
    let destination_stake_account = test_setup.transaction_accounts[0].1.clone();
    test_setup.add_account(destination_stake_address, destination_stake_account);
    test_setup.add_account_signer(test_setup.stake_address, AccountSharedData::default());
    test_setup.add_account(
        clock::id(),
        // advance epoch to fully activate source account
        create_account_shared_data_for_test(&Clock {
            epoch: 1_u64,
            ..Clock::default()
        }),
    );
    test_setup.add_account(
        stake_history::id(),
        create_account_shared_data_for_test(&StakeHistory::default()),
    );

    let instruction_data = serialize(&StakeInstruction::MoveLamports(1)).unwrap();

    c.bench_function("move_lamports", |bencher| {
        bencher.iter(|| test_setup.run(black_box(&instruction_data)))
    });
}

criterion_group!(
    benches,
    bench_initialize,
    bench_initialize_checked,
    bench_authorize_staker,
    bench_authorize_withdrawer,
    bench_authorize_staker_with_seed,
    bench_authorize_withdrawer_with_seed,
    bench_authorize_staker_checked,
    bench_authorize_withdrawer_checked,
    bench_authorize_staker_checked_with_seed,
    bench_authorize_withdrawer_checked_with_seed,
    bench_set_lockup,
    bench_set_lockup_checked,
    bench_withdraw,
    bench_delegate_stake,
    bench_deactivate,
    bench_split,
    bench_merge,
    bench_get_minimum_delegation,
    bench_deactivate_delinquent,
    bench_move_stake,
    bench_move_lamports,
);
criterion_main!(benches);
