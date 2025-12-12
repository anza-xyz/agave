use {
    agave_feature_set::{deprecate_legacy_vote_ixs, FeatureSet},
    bincode::serialize,
    criterion::{criterion_group, criterion_main, Criterion},
    solana_account::{self as account, create_account_for_test, Account, AccountSharedData},
    solana_clock::{Clock, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_hash::Hash,
    solana_instruction::{error::InstructionError, AccountMeta},
    solana_program_runtime::invoke_context::{
        mock_process_instruction, mock_process_instruction_with_feature_set,
    },
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_sdk_ids::{sysvar, vote::id},
    solana_slot_hashes::{SlotHashes, MAX_ENTRIES},
    solana_transaction_context::transaction_accounts::KeyedAccountSharedData,
    solana_vote_program::{
        vote_instruction::VoteInstruction,
        vote_processor::Entrypoint,
        vote_state::{
            create_v4_account_with_authorized, handler::VoteStateHandle, TowerSync, Vote,
            VoteAuthorize, VoteAuthorizeCheckedWithSeedArgs, VoteAuthorizeWithSeedArgs, VoteInit,
            VoteStateUpdate, VoteStateV3, VoteStateVersions, MAX_LOCKOUT_HISTORY,
        },
    },
};

fn create_default_rent_account() -> AccountSharedData {
    account::create_account_shared_data_for_test(&Rent::free())
}

fn create_default_clock_account() -> AccountSharedData {
    account::create_account_shared_data_for_test(&Clock::default())
}

fn create_accounts() -> (
    Slot,
    SlotHashes,
    Vec<KeyedAccountSharedData>,
    Vec<AccountMeta>,
) {
    // vote accounts are usually almost full of votes in normal operation
    let num_initial_votes = MAX_LOCKOUT_HISTORY as Slot;

    let clock = Clock::default();
    let mut slot_hashes = SlotHashes::new(&[]);
    for i in 0..MAX_ENTRIES {
        // slot hashes is full in normal operation
        slot_hashes.add(i as Slot, Hash::new_unique());
    }

    let vote_pubkey = Pubkey::new_unique();
    let authority_pubkey = Pubkey::new_unique();
    let vote_account = {
        let mut vote_state = VoteStateV3::new(
            &VoteInit {
                node_pubkey: authority_pubkey,
                authorized_voter: authority_pubkey,
                authorized_withdrawer: authority_pubkey,
                commission: 0,
            },
            &clock,
        );

        for next_vote_slot in 0..num_initial_votes {
            vote_state.process_next_vote_slot(next_vote_slot, 0, 0);
        }
        let mut vote_account_data: Vec<u8> = vec![0; VoteStateV3::size_of()];
        let versioned = VoteStateVersions::new_v3(vote_state);
        VoteStateV3::serialize(&versioned, &mut vote_account_data).unwrap();

        Account {
            lamports: 1,
            data: vote_account_data,
            owner: solana_vote_program::id(),
            executable: false,
            rent_epoch: 0,
        }
    };

    let transaction_accounts = vec![
        (solana_vote_program::id(), AccountSharedData::default()),
        (vote_pubkey, AccountSharedData::from(vote_account)),
        (
            sysvar::slot_hashes::id(),
            AccountSharedData::from(create_account_for_test(&slot_hashes)),
        ),
        (
            sysvar::clock::id(),
            AccountSharedData::from(create_account_for_test(&clock)),
        ),
        (authority_pubkey, AccountSharedData::default()),
    ];
    let instruction_account_metas = vec![
        AccountMeta {
            // `[WRITE]` Vote account to vote with
            pubkey: vote_pubkey,
            is_signer: false,
            is_writable: true,
        },
        AccountMeta {
            // `[]` Slot hashes sysvar
            pubkey: sysvar::slot_hashes::id(),
            is_signer: false,
            is_writable: false,
        },
        AccountMeta {
            // `[]` Clock sysvar
            pubkey: sysvar::clock::id(),
            is_signer: false,
            is_writable: false,
        },
        AccountMeta {
            // `[SIGNER]` Vote authority
            pubkey: authority_pubkey,
            is_signer: true,
            is_writable: false,
        },
    ];

    (
        num_initial_votes,
        slot_hashes,
        transaction_accounts,
        instruction_account_metas,
    )
}

fn create_test_account() -> (Pubkey, AccountSharedData) {
    let rent = Rent::default();
    let balance = rent.minimum_balance(VoteStateV3::size_of());
    let vote_pubkey = solana_pubkey::new_rand();
    (
        vote_pubkey,
        create_v4_account_with_authorized(
            &solana_pubkey::new_rand(),
            &vote_pubkey,
            &vote_pubkey,
            None,
            0,
            balance,
        ),
    )
}

fn create_test_account_with_authorized() -> (Pubkey, Pubkey, Pubkey, AccountSharedData) {
    let vote_pubkey = solana_pubkey::new_rand();
    let authorized_voter = solana_pubkey::new_rand();
    let authorized_withdrawer = solana_pubkey::new_rand();

    (
        vote_pubkey,
        authorized_voter,
        authorized_withdrawer,
        create_v4_account_with_authorized(
            &solana_pubkey::new_rand(),
            &authorized_voter,
            &authorized_withdrawer,
            None,
            0,
            100,
        ),
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
        None,
        instruction_data,
        transaction_accounts,
        instruction_accounts,
        expected_result,
        Entrypoint::vm,
        |_invoke_context| {},
        |_invoke_context| {},
    )
}

fn process_deprecated_instruction(
    instruction_data: &[u8],
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
    expected_result: Result<(), InstructionError>,
) -> Vec<AccountSharedData> {
    let mut deprecated_feature_set = FeatureSet::all_enabled();
    deprecated_feature_set.deactivate(&deprecate_legacy_vote_ixs::id());
    mock_process_instruction_with_feature_set(
        &id(),
        None,
        instruction_data,
        transaction_accounts,
        instruction_accounts,
        expected_result,
        Entrypoint::vm,
        |_invoke_context| {},
        |_invoke_context| {},
        &deprecated_feature_set.runtime_features(),
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

struct BenchVote {
    instruction_data: Vec<u8>,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl BenchVote {
    pub fn new() -> Self {
        let (num_initial_votes, slot_hashes, transaction_accounts, instruction_accounts) =
            create_accounts();

        let num_vote_slots = 4;
        let last_vote_slot = num_initial_votes
            .saturating_add(num_vote_slots)
            .saturating_sub(1);

        let last_vote_hash = slot_hashes
            .iter()
            .find(|(slot, _hash)| *slot == last_vote_slot)
            .unwrap()
            .1;

        let vote = Vote::new(
            (num_initial_votes..=last_vote_slot).collect(),
            last_vote_hash,
        );
        assert_eq!(vote.slots.len(), 4);
        let instruction_data = serialize(&VoteInstruction::Vote(vote)).unwrap();

        Self {
            instruction_data,
            transaction_accounts,
            instruction_accounts,
        }
    }

    pub fn run(&self) {
        let _accounts = process_deprecated_instruction(
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
            (vote_pubkey, vote_account.clone()),
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
                account::create_account_shared_data_for_test(&Clock::default()),
            ),
            (
                sysvar::epoch_schedule::id(),
                account::create_account_shared_data_for_test(&EpochSchedule::without_warmup()),
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

struct BenchVoteSwitch {
    instruction_data: Vec<u8>,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl BenchVoteSwitch {
    fn new() -> Self {
        let (num_initial_votes, slot_hashes, transaction_accounts, instruction_accounts) =
            create_accounts();

        let num_vote_slots = 4;
        let last_vote_slot = num_initial_votes
            .saturating_add(num_vote_slots)
            .saturating_sub(1);

        let last_vote_hash = slot_hashes
            .iter()
            .find(|(slot, _hash)| *slot == last_vote_slot)
            .unwrap()
            .1;

        let vote = Vote::new(
            (num_initial_votes..=last_vote_slot).collect(),
            last_vote_hash,
        );
        assert_eq!(vote.slots.len(), 4);
        let instruction_data =
            serialize(&VoteInstruction::VoteSwitch(vote, Hash::default())).unwrap();

        Self {
            instruction_data,
            transaction_accounts,
            instruction_accounts,
        }
    }
    fn run(&self) {
        let _accounts = process_deprecated_instruction(
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
        let vote_account = AccountSharedData::new(100, VoteStateV3::size_of(), &id());
        let clock_address = sysvar::clock::id();
        let clock_account = account::create_account_shared_data_for_test(&Clock::default());
        let default_authorized_pubkey = Pubkey::default();
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

        let instruction_data =
            serialize(&VoteInstruction::AuthorizeChecked(VoteAuthorize::Voter)).unwrap();
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

struct BenchUpdateVoteState {
    instruction_data: Vec<u8>,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl BenchUpdateVoteState {
    fn new(switch: bool) -> Self {
        let (num_initial_votes, slot_hashes, transaction_accounts, instruction_accounts) =
            create_accounts();

        let num_vote_slots = MAX_LOCKOUT_HISTORY as Slot;
        let last_vote_slot = num_initial_votes
            .saturating_add(num_vote_slots)
            .saturating_sub(1);
        let last_vote_hash = slot_hashes
            .iter()
            .find(|(slot, _hash)| *slot == last_vote_slot)
            .unwrap()
            .1;
        let slots_and_lockouts: Vec<(Slot, u32)> =
            ((num_initial_votes.saturating_add(1)..=last_vote_slot).zip((1u32..=31).rev()))
                .collect();
        let mut vote_state_update = VoteStateUpdate::from(slots_and_lockouts);
        vote_state_update.root = Some(num_initial_votes);
        vote_state_update.hash = last_vote_hash;

        let instruction_data = if switch {
            serialize(&VoteInstruction::UpdateVoteStateSwitch(
                vote_state_update,
                Hash::default(),
            ))
            .unwrap()
        } else {
            serialize(&VoteInstruction::UpdateVoteState(vote_state_update)).unwrap()
        };

        Self {
            instruction_data,
            transaction_accounts,
            instruction_accounts,
        }
    }

    fn run(&self) {
        let _accounts = process_deprecated_instruction(
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
        let vote_account = create_v4_account_with_authorized(
            &Pubkey::new_unique(),
            &authorized_voter,
            &authorized_withdrawer,
            None,
            0,
            100,
        );
        let clock_account = account::create_account_shared_data_for_test(&clock);
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

        let authorization_type = VoteAuthorize::Voter;
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
        let authorization_type: VoteAuthorize = VoteAuthorize::Voter;
        let vote_pubkey = Pubkey::new_unique();
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
        let vote_account = create_v4_account_with_authorized(
            &Pubkey::new_unique(),
            &authorized_voter,
            &authorized_withdrawer,
            None,
            0,
            100,
        );
        let new_authority_pubkey = Pubkey::new_unique();
        let clock = Clock {
            epoch: 1,
            leader_schedule_epoch: 2,
            ..Clock::default()
        };
        let clock_account = account::create_account_shared_data_for_test(&clock);
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

struct BenchCompactUpdateVoteState {
    instruction_data: Vec<u8>,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl BenchCompactUpdateVoteState {
    fn new(switch: bool) -> Self {
        let (vote_pubkey, vote_account) = create_test_account();
        let vote = Vote::new(vec![1], Hash::default());
        let vote_state_update = VoteStateUpdate::from(vec![(1, 1)]);
        let slot_hashes = SlotHashes::new(&[(*vote.slots.last().unwrap(), vote.hash)]);
        let slot_hashes_account = account::create_account_shared_data_for_test(&slot_hashes);
        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: true,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::slot_hashes::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
        ];

        let instruction_data = if switch {
            serialize(&VoteInstruction::CompactUpdateVoteStateSwitch(
                vote_state_update,
                Hash::default(),
            ))
            .unwrap()
        } else {
            serialize(&VoteInstruction::CompactUpdateVoteState(vote_state_update)).unwrap()
        };

        let transaction_accounts = vec![
            (vote_pubkey, vote_account.clone()),
            (sysvar::slot_hashes::id(), slot_hashes_account.clone()),
            (sysvar::clock::id(), create_default_clock_account()),
        ];

        Self {
            instruction_data,
            transaction_accounts,
            instruction_accounts,
        }
    }
    fn run(&self) {
        let _accounts = process_deprecated_instruction(
            &self.instruction_data,
            self.transaction_accounts.clone(),
            self.instruction_accounts.clone(),
            Ok(()),
        );
    }
}

struct BenchTowerSync {
    instruction_data: Vec<u8>,
    transaction_accounts: Vec<(Pubkey, AccountSharedData)>,
    instruction_accounts: Vec<AccountMeta>,
}

impl BenchTowerSync {
    fn new(switch: bool) -> Self {
        let (vote_pubkey, vote_account) = create_test_account();
        let vote = Vote::new(vec![1], Hash::default());
        let slot_hashes = SlotHashes::new(&[(*vote.slots.last().unwrap(), vote.hash)]);
        let slot_hashes_account = account::create_account_shared_data_for_test(&slot_hashes);
        let instruction_accounts = vec![
            AccountMeta {
                pubkey: vote_pubkey,
                is_signer: true,
                is_writable: true,
            },
            AccountMeta {
                pubkey: sysvar::slot_hashes::id(),
                is_signer: false,
                is_writable: false,
            },
            AccountMeta {
                pubkey: sysvar::clock::id(),
                is_signer: false,
                is_writable: false,
            },
        ];
        let tower_sync = TowerSync::from(vec![(1, 1)]);

        let instruction_data = if switch {
            serialize(&VoteInstruction::TowerSyncSwitch(
                tower_sync,
                Hash::default(),
            ))
            .unwrap()
        } else {
            serialize(&VoteInstruction::TowerSync(tower_sync)).unwrap()
        };

        let transaction_accounts = vec![
            (vote_pubkey, vote_account.clone()),
            (sysvar::slot_hashes::id(), slot_hashes_account.clone()),
            (sysvar::clock::id(), create_default_clock_account()),
        ];

        Self {
            instruction_data,
            transaction_accounts,
            instruction_accounts,
        }
    }
    fn run(&self) {
        let _accounts = process_deprecated_instruction(
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

fn bench_vote(c: &mut Criterion) {
    let test_setup = BenchVote::new();
    c.bench_function("vote_instruction_vote", |bencher| {
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

fn bench_vote_switch(c: &mut Criterion) {
    let test_setup = BenchVoteSwitch::new();
    c.bench_function("vote_vote_switch", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_authorize_checked(c: &mut Criterion) {
    let test_setup = BenchAuthorizeChecked::new();
    c.bench_function("vote_authorize_checked", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_update_vote_state(c: &mut Criterion) {
    let test_setup = BenchUpdateVoteState::new(false);
    c.bench_function("vote_update_vote_state", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_update_vote_state_switch(c: &mut Criterion) {
    let test_setup = BenchUpdateVoteState::new(true);
    c.bench_function("vote_update_vote_state_switch", |bencher| {
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

fn bench_compact_update_vote_state(c: &mut Criterion) {
    let test_setup = BenchCompactUpdateVoteState::new(false);
    c.bench_function("vote_compact_update_vote_state", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_compact_update_vote_state_switch(c: &mut Criterion) {
    let test_setup = BenchCompactUpdateVoteState::new(true);
    c.bench_function("vote_compact_update_vote_state_switch", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_tower_sync(c: &mut Criterion) {
    let test_setup = BenchTowerSync::new(false);
    c.bench_function("vote_tower_sync", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

fn bench_tower_sync_switch(c: &mut Criterion) {
    let test_setup = BenchTowerSync::new(true);
    c.bench_function("vote_tower_sync_switch", |bencher| {
        bencher.iter(|| test_setup.run())
    });
}

criterion_group!(
    benches,
    bench_initialize_account,
    bench_authorize,
    bench_vote,
    bench_withdraw,
    bench_update_validator_identity,
    bench_update_commission,
    bench_vote_switch,
    bench_authorize_checked,
    bench_update_vote_state,
    bench_update_vote_state_switch,
    bench_authorize_with_seed,
    bench_authorize_checked_with_seed,
    bench_compact_update_vote_state,
    bench_compact_update_vote_state_switch,
    bench_tower_sync,
    bench_tower_sync_switch,
);
criterion_main!(benches);
