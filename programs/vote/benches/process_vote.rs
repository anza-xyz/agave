use {
    agave_feature_set::{FeatureSet, deprecate_legacy_vote_ixs},
    criterion::{BatchSize, Criterion, criterion_group, criterion_main},
    solana_account::{Account, AccountSharedData, create_account_for_test},
    solana_clock::{Clock, Slot},
    solana_hash::Hash,
    solana_instruction::AccountMeta,
    solana_program_runtime::{
        invoke_context::{mock_process_instruction, mock_process_instruction_with_feature_set},
        solana_sbpf::program::BuiltinFunctionDefinition,
    },
    solana_pubkey::Pubkey,
    solana_sdk_ids::sysvar,
    solana_slot_hashes::{MAX_ENTRIES, SlotHashes},
    solana_transaction_context::transaction_accounts::KeyedAccountSharedData,
    solana_vote_program::{
        vote_instruction::VoteInstruction,
        vote_state::{
            MAX_LOCKOUT_HISTORY, TowerSync, Vote, VoteInitV2, VoteStateUpdate, VoteStateV4,
            VoteStateVersions, handler::VoteStateHandler,
        },
    },
    std::time::Duration,
};

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
    let node_pubkey = Pubkey::new_unique();
    let authority_pubkey = Pubkey::new_unique();
    let vote_account = {
        let mut vote_state = VoteStateHandler::new_v4(VoteStateV4::new(
            &VoteInitV2 {
                node_pubkey,
                authorized_voter: authority_pubkey,
                authorized_withdrawer: authority_pubkey,
                ..Default::default()
            },
            &vote_pubkey,
            &node_pubkey,
            &clock,
        ));

        for next_vote_slot in 0..num_initial_votes {
            vote_state.process_next_vote_slot(next_vote_slot, 0, 0);
        }
        let mut vote_account_data: Vec<u8> = vec![0; VoteStateV4::size_of()];
        let versioned = VoteStateVersions::new_v4(vote_state.unwrap_v4());
        VoteStateV4::serialize(&versioned, &mut vote_account_data).unwrap();

        Account {
            lamports: 1,
            data: vote_account_data,
            owner: solana_vote_program::id(),
            executable: false,
            rent_epoch: 0,
        }
    };

    let transaction_accounts = vec![
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
    let mut instruction_account_metas = (0..4)
        .map(|index_in_callee| AccountMeta {
            pubkey: transaction_accounts[index_in_callee].0,
            is_signer: false,
            is_writable: false,
        })
        .collect::<Vec<AccountMeta>>();
    instruction_account_metas[0].is_writable = true;
    instruction_account_metas[3].is_signer = true;

    (
        num_initial_votes,
        slot_hashes,
        transaction_accounts,
        instruction_account_metas,
    )
}

fn bench_process_deprecated_vote_instruction(
    c: &mut Criterion,
    name: &str,
    transaction_accounts: Vec<KeyedAccountSharedData>,
    instruction_account_metas: Vec<AccountMeta>,
    instruction_data: Vec<u8>,
) {
    let mut deprecated_feature_set = FeatureSet::all_enabled();
    deprecated_feature_set.deactivate(&deprecate_legacy_vote_ixs::id());
    let feature_set = deprecated_feature_set.runtime_features();
    c.bench_function(name, |bencher| {
        // Processing a vote mutates the vote-account state, so hand each
        // iteration a fresh clone of the accounts/metas via the (untimed)
        // batched setup closure.
        bencher.iter_batched(
            || {
                (
                    transaction_accounts.clone(),
                    instruction_account_metas.clone(),
                )
            },
            |(transaction_accounts, instruction_account_metas)| {
                mock_process_instruction_with_feature_set(
                    &solana_vote_program::id(),
                    &instruction_data,
                    transaction_accounts,
                    instruction_account_metas,
                    Ok(()),
                    solana_vote_program::vote_processor::Entrypoint::register,
                    |_invoke_context| {},
                    |_invoke_context| {},
                    &feature_set,
                );
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_process_vote_instruction(
    c: &mut Criterion,
    name: &str,
    transaction_accounts: Vec<KeyedAccountSharedData>,
    instruction_account_metas: Vec<AccountMeta>,
    instruction_data: Vec<u8>,
) {
    c.bench_function(name, |bencher| {
        // Processing a vote mutates the vote-account state, so hand each
        // iteration a fresh clone of the accounts/metas via the (untimed)
        // batched setup closure.
        bencher.iter_batched(
            || {
                (
                    transaction_accounts.clone(),
                    instruction_account_metas.clone(),
                )
            },
            |(transaction_accounts, instruction_account_metas)| {
                mock_process_instruction(
                    &solana_vote_program::id(),
                    &instruction_data,
                    transaction_accounts,
                    instruction_account_metas,
                    Ok(()),
                    solana_vote_program::vote_processor::Entrypoint::register,
                    |_invoke_context| {},
                    |_invoke_context| {},
                );
            },
            BatchSize::SmallInput,
        )
    });
}

fn bench_process_vote(c: &mut Criterion) {
    let (num_initial_votes, slot_hashes, transaction_accounts, instruction_account_metas) =
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
    let instruction_data = bincode::serialize(&VoteInstruction::Vote(vote)).unwrap();

    bench_process_deprecated_vote_instruction(
        c,
        "process_vote",
        transaction_accounts,
        instruction_account_metas,
        instruction_data,
    );
}

fn bench_process_vote_state_update(c: &mut Criterion) {
    let (num_initial_votes, slot_hashes, transaction_accounts, instruction_account_metas) =
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
        ((num_initial_votes.saturating_add(1)..=last_vote_slot).zip((1u32..=31).rev())).collect();
    let mut vote_state_update = VoteStateUpdate::from(slots_and_lockouts);
    vote_state_update.root = Some(num_initial_votes);
    vote_state_update.hash = last_vote_hash;
    let instruction_data =
        bincode::serialize(&VoteInstruction::UpdateVoteState(vote_state_update)).unwrap();

    bench_process_deprecated_vote_instruction(
        c,
        "process_vote_state_update",
        transaction_accounts,
        instruction_account_metas,
        instruction_data,
    );
}

fn bench_process_tower_sync(c: &mut Criterion) {
    let (num_initial_votes, slot_hashes, transaction_accounts, instruction_account_metas) =
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
        ((num_initial_votes.saturating_add(1)..=last_vote_slot).zip((1u32..=31).rev())).collect();
    let mut tower_sync = TowerSync::from(slots_and_lockouts);
    tower_sync.root = Some(num_initial_votes);
    tower_sync.hash = last_vote_hash;
    tower_sync.block_id = Hash::new_unique();
    let instruction_data = bincode::serialize(&VoteInstruction::TowerSync(tower_sync)).unwrap();

    bench_process_vote_instruction(
        c,
        "process_tower_sync",
        transaction_accounts,
        instruction_account_metas,
        instruction_data,
    );
}

criterion_group! {
    name = benches;
    // Trim criterion's defaults so the suite runs in a couple of seconds.
    // These are cheap, low-variance CPU-bound benchmarks, so short windows and
    // few samples already give tight confidence intervals.
    config = Criterion::default()
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_millis(400))
        .sample_size(10)
        .without_plots();
    targets = bench_process_vote, bench_process_vote_state_update, bench_process_tower_sync,
}
criterion_main!(benches);
