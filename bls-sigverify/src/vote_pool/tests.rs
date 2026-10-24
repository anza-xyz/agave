#![allow(clippy::arithmetic_side_effects)]

use {
    super::*,
    crate::{
        bls_sigverifier::SigVerifierChannels,
        bls_vote_sigverify::{batch::Batch, verify_and_send_votes},
        rewards::rewards_wants_vote,
        unverified_votes_batch::{Rank, UnverifiedVotePayload},
    },
    agave_votor_messages::{
        consensus_message::Block, reward_certificate::NUM_SLOTS_FOR_REWARD,
        unverified_vote_message::UnverifiedVoteMessage, vote::Vote, wire::VotePayloadToSign,
    },
    agave_votor_transport::endpoint::stub_ban_channel_for_tests,
    crossbeam_channel::unbounded,
    rayon::ThreadPoolBuilder,
    solana_clock::Slot,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::{
        bank::Bank,
        epoch_stakes::BLSPubkeyToRankMap,
        genesis_utils::{ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts},
    },
    solana_signer::Signer,
    solana_streamer::evicting_sender::EvictingSender,
    std::{
        collections::{BTreeSet, HashMap, HashSet},
        sync::Arc,
    },
};

struct TestContext {
    shred_version: u16,
    bank: Bank,
    leader_schedule: LeaderScheduleCache,
    validators: Vec<ValidatorVoteKeypairs>,
    rank_map: Arc<BLSPubkeyToRankMap>,
    ranks: Vec<Rank>,
    my_pubkey: Pubkey,
    my_stake: Stake,
    pool: VotePool,
    votes: HashMap<VotePayloadToSign, Batch>,
}

impl TestContext {
    fn new(for_rewards: bool, slot: Slot) -> Self {
        Self::new_with_num_validators(for_rewards, slot, 10)
    }

    fn new_with_num_validators(for_rewards: bool, slot: Slot, num_validators: usize) -> Self {
        let validators = (0..num_validators)
            .map(|_| ValidatorVoteKeypairs::new_rand())
            .collect::<Vec<_>>();
        let genesis = create_genesis_config_with_vote_accounts(
            1_000_000_000,
            &validators,
            vec![10; validators.len()],
        );
        let bank = Bank::new_for_tests(&genesis.genesis_config);
        let leader_schedule = LeaderScheduleCache::new_from_bank(&bank);
        let rank_map = bank.get_rank_map(slot).unwrap().clone();
        let my_pubkey = if for_rewards {
            leader_schedule
                .slot_leader_at(slot.saturating_add(NUM_SLOTS_FOR_REWARD), Some(&bank))
                .unwrap()
                .id
        } else {
            Pubkey::new_unique()
        };
        let my_rank = rank_map.get_ranked_entry_for_node(&my_pubkey);
        let my_stake = my_rank.map_or(Stake(0), |(_, entry)| Stake::from(entry.stake));
        let ranks = (0..rank_map.len() as u16)
            .filter(|rank| my_rank.is_none_or(|(my_rank, _)| my_rank != *rank))
            .map(Rank)
            .collect();
        assert_eq!(
            rank_map.total_stake().get().get(),
            10u64.saturating_mul(num_validators as u64)
        );
        assert_eq!(
            rewards_wants_vote(
                &my_pubkey,
                &leader_schedule,
                bank.slot(),
                &Vote::new_skip_vote(slot),
            ),
            for_rewards,
        );
        Self {
            shred_version: 42,
            bank,
            leader_schedule,
            validators,
            rank_map,
            ranks,
            my_pubkey,
            my_stake,
            pool: VotePool::default(),
            votes: HashMap::new(),
        }
    }

    fn add(&mut self, vote: Vote, voter: usize, valid: bool) {
        let rank = self.ranks[voter];
        let entry = self
            .rank_map
            .get_pubkey_stake_entry(rank.0 as usize)
            .unwrap();
        let validator = self
            .validators
            .iter()
            .find(|validator| validator.node_keypair.pubkey() == entry.node_pubkey)
            .unwrap();
        // Signing a different shred version gives a well-formed invalid signature.
        let signed_payload = VotePayloadToSign::new_from_vote(
            vote,
            self.shred_version.saturating_add(u16::from(!valid)),
        );
        let payload = UnverifiedVotePayload {
            vote_message: UnverifiedVoteMessage {
                vote,
                shred_version: self.shred_version,
                signature: validator
                    .bls_keypair
                    .sign(&wincode::serialize(&signed_payload).unwrap())
                    .into(),
            },
            sender_bls_pubkey: entry.bls_pubkey,
            sender_identity_pubkey: entry.node_pubkey,
            sender_vote_account_pubkey: entry.vote_account_pubkey,
            rank,
            stake: entry.stake,
        };
        self.pool
            .add_vote(
                &self.my_pubkey,
                &self.bank,
                &self.leader_schedule,
                &self.rank_map,
                &mut self.votes,
                self.my_stake,
                payload,
            )
            .unwrap();
        self.assert_notar_indexes();
        self.assert_pending_vote_index();
    }

    fn assert_notar_indexes(&self) {
        for slot in self.pool.entries.values() {
            let expected_pending = slot
                .notar
                .iter()
                .filter(|(_, entry)| !entry.batch.is_empty())
                .map(|(block_id, entry)| (entry.observed_stake(), *block_id))
                .collect::<BTreeSet<_>>();
            assert_eq!(slot.notar_stake_state.pending_entries, expected_pending);
            for &(stake, block_id) in &slot.notar_stake_state.safe_to_notar_candidates {
                let entry = &slot.notar[&block_id];
                assert_eq!(stake, entry.observed_stake());
                assert!(!safe_to_notar_is_combined_threshold_met(
                    entry.verified_stake,
                    slot.skip.verified_stake,
                    slot.total_stake,
                ));
                assert_eq!(
                    slot.notar_stake_state
                        .pending_entries
                        .contains(&(stake, block_id)),
                    !entry.batch.is_empty()
                );
            }
            for (block_id, entry) in &slot.notar {
                // Bans may remove candidates whose observed stake drops to zero.
                if entry.observed_stake().0 != 0
                    && !safe_to_notar_is_combined_threshold_met(
                        entry.verified_stake,
                        slot.skip.verified_stake,
                        slot.total_stake,
                    )
                {
                    assert!(
                        slot.notar_stake_state
                            .safe_to_notar_candidates
                            .contains(&(entry.observed_stake(), *block_id))
                    );
                }
            }
        }
    }

    fn entry(&self, vote: Vote) -> &VoteEntry {
        let entry = self.pool.entries.get(&vote.slot()).unwrap();
        match vote {
            Vote::Skip(_) => &entry.skip,
            Vote::SkipFallback(_) => &entry.skip_fallback,
            Vote::Finalize(_) => &entry.finalize,
            Vote::Notarize(vote) => &entry.notar[&vote.block.block_id],
            Vote::NotarizeFallback(vote) => &entry.notar_fallback[&vote.block.block_id],
            Vote::Genesis(vote) => &entry.genesis[&vote.block.block_id],
        }
    }

    fn assert_queued(&self, expected: &[Vote]) {
        let expected = expected
            .iter()
            .map(|vote| VotePayloadToSign::new_from_vote(*vote, self.shred_version))
            .collect();
        assert_eq!(self.votes.keys().copied().collect::<HashSet<_>>(), expected);
    }

    fn assert_pending_vote_index(&self) {
        assert_pending_vote_index(self.pool.entries.values(), &self.pool.pending_votes);
    }

    fn verify(&mut self) -> usize {
        let (ban_sender, _ban_receiver) = stub_ban_channel_for_tests(1024);
        let thread_pool = ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        let (_, packet_receiver) = unbounded();
        let (_, certificate_receiver) = unbounded();
        let (repair_sender, _repair_receiver) = EvictingSender::new_bounded(1024);
        let (reward_sender, _reward_receiver) = unbounded();
        let (pool_sender, _pool_receiver) = unbounded();
        let (metrics_sender, _metrics_receiver) = unbounded();
        let channels = SigVerifierChannels::new(
            packet_receiver,
            certificate_receiver,
            repair_sender,
            reward_sender,
            pool_sender,
            metrics_sender,
        );
        let stats = verify_and_send_votes(
            &mut self.votes,
            &self.bank,
            &self.my_pubkey,
            &self.leader_schedule,
            &thread_pool,
            &channels,
        )
        .unwrap();
        self.pool.update_verified(&ban_sender, &mut self.votes);
        assert!(self.votes.is_empty());
        self.assert_notar_indexes();
        self.assert_pending_vote_index();
        stats.votes_to_sig_verify.0
    }
}

fn pending_vote_index<'a>(slots: impl IntoIterator<Item = &'a SlotEntry>) -> PendingVoteIndex {
    let mut index = PendingVoteIndex::default();
    for slot in slots {
        for entry in slot.vote_entries() {
            for payload in &entry.batch {
                index.insert(payload.sender_identity_pubkey, entry.vote_payload_to_sign);
            }
        }
    }
    index
}

fn assert_pending_vote_index<'a>(
    slots: impl IntoIterator<Item = &'a SlotEntry>,
    index: &PendingVoteIndex,
) {
    assert_eq!(index.entries, pending_vote_index(slots).entries);
}

// These lookup tests construct stake distributions directly. Their payloads are
// only moved between batches, never signature-verified.
fn insert_notar_for_lookup(
    slot: &mut SlotEntry,
    payload: &UnverifiedVotePayload,
    verified: Stake,
    pending: Stake,
) -> Block {
    let block = Block::new_unique(1);
    let vote = Vote::new_notarization_vote(block);
    let key = VotePayloadToSign::new_from_vote(vote, payload.vote_message.shred_version);
    let mut entry = VoteEntry::new(key, slot.my_stake, slot.total_stake, slot.max_validators);
    entry.verified_stake = verified;
    if pending.0 != 0 {
        let mut payload = payload.clone();
        payload.vote_message.vote = vote;
        payload.stake = NonZeroStake::new(pending.0).unwrap();
        entry.batch.push(payload);
        entry.batch_stake = pending;
    }
    let observed = entry.observed_stake();
    slot.notar_stake_state
        .total_notar_stake_verified
        .increment(verified);
    slot.notar_stake_state
        .total_notar_stake_observed
        .increment(observed);
    slot.notar_stake_state
        .stakes_verified
        .insert((verified, block.block_id));
    slot.notar_stake_state
        .stakes_observed
        .insert((observed, block.block_id));
    if !entry.batch.is_empty() {
        slot.notar_stake_state
            .pending_entries
            .insert((observed, block.block_id));
    }
    if !safe_to_notar_is_combined_threshold_met(
        verified,
        slot.skip.verified_stake,
        slot.total_stake,
    ) {
        slot.notar_stake_state
            .safe_to_notar_candidates
            .insert((observed, block.block_id));
    }
    slot.notar.insert(block.block_id, entry);
    block
}

#[test]
fn test_combined_lookup_multiple_candidates_and_tied_stakes() {
    let mut ctx = TestContext::new(false, 1);
    let sample_vote = Vote::new_notarization_vote(Block::new_unique(1));
    ctx.add(sample_vote, 0, true);
    let payload = ctx.entry(sample_vote).batch[0].clone();
    let mut slot = SlotEntry::new(
        1,
        ctx.shred_version,
        Stake(15),
        NonZeroStake::new(100).unwrap(),
        10,
    );
    let completed = insert_notar_for_lookup(&mut slot, &payload, Stake(20), Stake(0));
    let qualifying = [
        insert_notar_for_lookup(&mut slot, &payload, Stake(0), Stake(10)),
        insert_notar_for_lookup(&mut slot, &payload, Stake(0), Stake(5)),
        insert_notar_for_lookup(&mut slot, &payload, Stake(0), Stake(5)),
    ];
    let below = insert_notar_for_lookup(&mut slot, &payload, Stake(0), Stake(4));
    // The largest candidate completes after verified skip stake increases.
    slot.update_verified(slot.skip.vote_payload_to_sign, Stake(40));
    assert!(
        !slot
            .notar_stake_state
            .safe_to_notar_candidates
            .contains(&(Stake(20), completed.block_id))
    );
    let mut skip_payload = payload;
    skip_payload.vote_message.vote = Vote::new_skip_vote(1);
    skip_payload.stake = NonZeroStake::new(1).unwrap();
    let mut votes = HashMap::new();
    let mut pending_votes = pending_vote_index([&slot]);
    slot.skip
        .add_unverified_vote(skip_payload, &mut votes, &mut pending_votes);
    slot.check_safe_to_notar_after_skip_vote(&mut votes, &mut pending_votes);
    assert_pending_vote_index([&slot], &pending_votes);

    let expected = qualifying
        .into_iter()
        .map(|block| {
            VotePayloadToSign::new_from_vote(Vote::new_notarization_vote(block), ctx.shred_version)
        })
        .chain([slot.skip.vote_payload_to_sign])
        .collect::<HashSet<_>>();
    assert_eq!(votes.keys().copied().collect::<HashSet<_>>(), expected);
    assert!(
        !slot
            .notar_stake_state
            .safe_to_notar_candidates
            .contains(&(Stake(20), completed.block_id))
    );
    assert_eq!(
        slot.notar_stake_state.pending_entries,
        BTreeSet::from([(Stake(4), below.block_id)])
    );
    for block in qualifying {
        let entry = &slot.notar[&block.block_id];
        assert!(entry.batch.is_empty());
        assert!(entry.inflight_stake.0 > 0);
    }
    assert_eq!(slot.notar[&below.block_id].batch_stake, Stake(4));

    // Repeating the lookup neither queues more work nor revisits drained batches.
    votes.clear();
    slot.check_safe_to_notar_after_skip_vote(&mut votes, &mut pending_votes);
    assert_pending_vote_index([&slot], &pending_votes);
    assert!(votes.is_empty());
}

#[test]
fn test_combined_lookup_visits_do_not_scale_with_block_count() {
    let mut ctx = TestContext::new(false, 1);
    let sample_vote = Vote::new_notarization_vote(Block::new_unique(1));
    ctx.add(sample_vote, 0, true);
    let payload = ctx.entry(sample_vote).batch[0].clone();
    const SKIP_VOTES: usize = 512;
    for blocks in [64, 2048] {
        let mut slot = SlotEntry::new(
            1,
            ctx.shred_version,
            Stake(0),
            NonZeroStake::new(10_000).unwrap(),
            10_000,
        );
        for _ in 0..blocks {
            insert_notar_for_lookup(&mut slot, &payload, Stake(0), Stake(1));
        }
        let mut votes = HashMap::new();
        let mut pending_votes = pending_vote_index([&slot]);
        for rank in 0..SKIP_VOTES {
            let mut skip_payload = payload.clone();
            skip_payload.vote_message.vote = Vote::new_skip_vote(1);
            skip_payload.rank = Rank(rank as u16);
            skip_payload.stake = NonZeroStake::new(1).unwrap();
            slot.skip
                .add_unverified_vote(skip_payload, &mut votes, &mut pending_votes);
            slot.check_safe_to_notar_after_skip_vote(&mut votes, &mut pending_votes);
            assert_pending_vote_index([&slot], &pending_votes);
        }
        // One witness and one pending boundary are inspected per skip arrival,
        // regardless of the number of below-threshold notar blocks.
        assert_eq!(
            slot.notar_stake_state.safe_to_notar_candidates.len(),
            blocks
        );
        assert_eq!(slot.notar_stake_state.pending_entries.len(), blocks);
        assert_eq!(
            votes.keys().copied().collect::<HashSet<_>>(),
            HashSet::from([slot.skip.vote_payload_to_sign])
        );
    }
}

#[test]
fn test_combined_lookup_reorders_candidates_after_ban() {
    let mut ctx = TestContext::new(false, 1);
    let first = Block::new_unique(1);
    let second = Block::new_unique(1);
    for voter in 0..3 {
        ctx.add(Vote::new_notarization_vote(first), voter, true);
    }
    for voter in 3..5 {
        ctx.add(Vote::new_notarization_vote(second), voter, true);
    }
    let slot = ctx.pool.entries.get_mut(&1).unwrap();
    let banned: HashSet<_> = slot.notar[&first.block_id]
        .batch
        .iter()
        .take(2)
        .map(|payload| payload.sender_identity_pubkey)
        .collect();
    let (ban_sender, _ban_receiver) = stub_ban_channel_for_tests(1024);
    ctx.pool.ban_pubkeys(&ban_sender, banned);
    ctx.assert_pending_vote_index();
    let slot = ctx.pool.entries.get_mut(&1).unwrap();
    assert_eq!(
        slot.notar_stake_state.safe_to_notar_candidates,
        BTreeSet::from([(Stake(10), first.block_id), (Stake(20), second.block_id),])
    );
    assert_eq!(
        slot.notar_stake_state.pending_entries,
        slot.notar_stake_state.safe_to_notar_candidates
    );

    // Isolate the combined lookup from safe-to-skip, which could also flush
    // these votes. Only the new largest block has the required 20% notar stake.
    slot.update_verified(slot.skip.vote_payload_to_sign, Stake(40));
    slot.check_safe_to_notar_after_skip_vote(&mut ctx.votes, &mut ctx.pool.pending_votes);
    ctx.assert_queued(&[Vote::new_notarization_vote(second)]);
    let slot = &ctx.pool.entries[&1];
    assert_eq!(
        slot.notar_stake_state.pending_entries,
        BTreeSet::from([(Stake(10), first.block_id)])
    );
    assert_eq!(slot.notar[&first.block_id].batch_stake, Stake(10));
    assert!(slot.notar[&second.block_id].batch.is_empty());
    ctx.assert_notar_indexes();
}

#[test]
fn test_completed_notar_stays_pending_until_safe_to_skip() {
    let mut ctx = TestContext::new(false, 1);
    let sample_vote = Vote::new_notarization_vote(Block::new_unique(1));
    ctx.add(sample_vote, 0, true);
    let payload = ctx.entry(sample_vote).batch[0].clone();
    let mut slot = SlotEntry::new(
        1,
        ctx.shred_version,
        Stake(0),
        NonZeroStake::new(100).unwrap(),
        10,
    );
    let block = insert_notar_for_lookup(&mut slot, &payload, Stake(40), Stake(10));
    slot.update_verified(slot.skip.vote_payload_to_sign, Stake(20));
    assert!(slot.notar_stake_state.safe_to_notar_candidates.is_empty());
    let mut skip_payload = payload;
    skip_payload.vote_message.vote = Vote::new_skip_vote(1);
    skip_payload.stake = NonZeroStake::new(20).unwrap();
    let mut votes = HashMap::new();
    let mut pending_votes = pending_vote_index([&slot]);
    slot.skip
        .add_unverified_vote(skip_payload, &mut votes, &mut pending_votes);

    // Skip verification retired the candidate at 60% combined stake, but its
    // unverified batch must remain pending for other thresholds.
    let pending = BTreeSet::from([(Stake(50), block.block_id)]);
    slot.check_safe_to_notar_after_skip_vote(&mut votes, &mut pending_votes);
    assert_pending_vote_index([&slot], &pending_votes);
    assert!(slot.notar_stake_state.safe_to_notar_candidates.is_empty());
    assert_eq!(slot.notar_stake_state.pending_entries, pending);
    assert_eq!(slot.notar[&block.block_id].batch_stake, Stake(10));
    assert!(votes.is_empty());
    for _ in 0..16 {
        slot.check_safe_to_notar_after_skip_vote(&mut votes, &mut pending_votes);
        assert_pending_vote_index([&slot], &pending_votes);
    }
    assert_eq!(slot.notar_stake_state.pending_entries, pending);
    assert!(votes.is_empty());

    // Only 20% skip stake is verified, while 40% is observed. Safe-to-skip
    // must still flush both the new skip votes and the completed notar block.
    slot.handle_safe_to_skip(&mut votes, &mut pending_votes);
    assert_pending_vote_index([&slot], &pending_votes);
    assert!(slot.notar_stake_state.pending_entries.is_empty());
    assert!(slot.notar[&block.block_id].batch.is_empty());
    assert_eq!(slot.notar[&block.block_id].inflight_stake.0, 10);
    assert_eq!(slot.skip.inflight_stake.0, 20);
    assert_eq!(
        votes.keys().copied().collect::<HashSet<_>>(),
        HashSet::from([
            slot.notar[&block.block_id].vote_payload_to_sign,
            slot.skip.vote_payload_to_sign,
        ])
    );
}

#[test]
fn test_combined_lookup_reactivates_drained_notar_batch() {
    let mut ctx = TestContext::new(false, 1);
    let block = Block::new_unique(1);
    let notar = Vote::new_notarization_vote(block);
    let skip = Vote::new_skip_vote(1);
    for voter in 0..4 {
        ctx.add(notar, voter, true);
    }
    assert_eq!(ctx.verify(), 4);
    assert!(
        ctx.pool.entries[&1]
            .notar_stake_state
            .pending_entries
            .is_empty()
    );

    ctx.add(notar, 4, true);
    ctx.assert_queued(&[]);
    assert_eq!(
        ctx.pool.entries[&1].notar_stake_state.pending_entries,
        BTreeSet::from([(Stake(50), block.block_id)])
    );
    ctx.add(skip, 5, true);
    ctx.assert_queued(&[notar, skip]);
    assert!(
        ctx.pool.entries[&1]
            .notar_stake_state
            .pending_entries
            .is_empty()
    );
    assert_eq!(ctx.verify(), 2);
    assert_eq!(ctx.entry(notar).verified_stake.0, 50);
    assert_eq!(ctx.entry(skip).verified_stake.0, 10);
}

#[test]
fn test_indexed_safe_to_notar_matches_scan() {
    let mut ctx = TestContext::new(false, 1);
    let block = Block::new_unique(1);
    let vote = Vote::new_notarization_vote(block);
    ctx.add(vote, 0, true);
    let payload = ctx.entry(vote).batch[0].clone();
    let notar_key = VotePayloadToSign::new_from_vote(vote, ctx.shred_version);

    // Compare the indexed lookup with the original per-block conditions, including
    // rounding, own stake, empty batches, inflight votes and completed candidates.
    for total in [100, 101, u64::MAX] {
        let fraction = |percent: u64| (u128::from(total) * u128::from(percent) / 100) as u64;
        let total_stake = NonZeroStake::new(total).unwrap();
        for my_stake in [0, fraction(20), fraction(50)] {
            for verified in [0, fraction(20), fraction(60)] {
                for inflight in [0, fraction(10)] {
                    for pending in [0, 1, fraction(10)] {
                        let observed = Stake(verified).add(Stake(inflight)).add(Stake(pending));
                        // Nonempty batches at the standalone 40% threshold have
                        // already been flushed by the notar arrival path.
                        if pending != 0
                            && total_stake.fraction(observed.add(Stake(my_stake)))
                                >= SAFE_TO_NOTAR_NOTAR_ONLY
                        {
                            continue;
                        }
                        for verified_skip in [0, fraction(40), fraction(60)] {
                            for pending_skip in [0, 1, fraction(10)] {
                                let make_slot = || {
                                    let mut slot = SlotEntry::new(
                                        1,
                                        ctx.shred_version,
                                        Stake(my_stake),
                                        total_stake,
                                        10,
                                    );
                                    let mut entry =
                                        VoteEntry::new(notar_key, Stake(my_stake), total_stake, 10);
                                    entry.verified_stake = Stake(verified);
                                    entry.inflight_stake = Stake(inflight);
                                    entry.batch_stake = Stake(pending);
                                    if pending != 0 {
                                        entry.batch.push(payload.clone());
                                    }
                                    if !entry.batch.is_empty() {
                                        slot.notar_stake_state
                                            .pending_entries
                                            .insert((entry.observed_stake(), block.block_id));
                                    }
                                    slot.skip.verified_stake = Stake(verified_skip);
                                    slot.skip.batch_stake = Stake(pending_skip);
                                    if pending_skip != 0 {
                                        slot.skip.batch.push(payload.clone());
                                    }
                                    if !safe_to_notar_is_combined_threshold_met(
                                        entry.verified_stake,
                                        slot.skip.verified_stake,
                                        slot.total_stake,
                                    ) {
                                        slot.notar_stake_state
                                            .safe_to_notar_candidates
                                            .insert((entry.observed_stake(), block.block_id));
                                    }
                                    slot.notar.insert(block.block_id, entry);
                                    slot
                                };
                                let mut indexed = make_slot();
                                let mut scanned = make_slot();
                                let mut indexed_votes = HashMap::new();
                                let mut scanned_votes = HashMap::new();
                                let mut indexed_pending = pending_vote_index([&indexed]);
                                let mut scanned_pending = pending_vote_index([&scanned]);
                                indexed.check_safe_to_notar_after_skip_vote(
                                    &mut indexed_votes,
                                    &mut indexed_pending,
                                );
                                SlotEntry::check_safe_to_notar_after_notar_vote(
                                    scanned.notar.get_mut(&block.block_id).unwrap(),
                                    &mut scanned.skip,
                                    Stake(my_stake),
                                    total_stake,
                                    &mut scanned_votes,
                                    &mut scanned_pending,
                                );
                                assert_pending_vote_index([&indexed], &indexed_pending);
                                assert_pending_vote_index([&scanned], &scanned_pending);
                                assert_eq!(indexed_pending.entries, scanned_pending.entries);
                                assert_eq!(
                                    indexed_votes.keys().collect::<HashSet<_>>(),
                                    scanned_votes.keys().collect::<HashSet<_>>()
                                );
                                assert_eq!(
                                    indexed.notar[&block.block_id].batch_stake,
                                    scanned.notar[&block.block_id].batch_stake
                                );
                                assert_eq!(indexed.skip.batch_stake, scanned.skip.batch_stake);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn test_empty_notar_batch_witnesses_skip_then_retires() {
    let mut ctx = TestContext::new(false, 1);
    let block = Block::new_unique(1);
    let notar = Vote::new_notarization_vote(block);
    let skip = Vote::new_skip_vote(1);
    for voter in 0..4 {
        ctx.add(notar, voter, true);
    }
    assert_eq!(ctx.verify(), 4);
    assert!(ctx.entry(notar).batch.is_empty());
    ctx.add(skip, 4, true);
    ctx.assert_queued(&[]);
    ctx.add(skip, 5, true);
    ctx.assert_queued(&[skip]);
    assert_eq!(ctx.verify(), 2);

    // Skip verification retires the witness immediately.
    assert!(
        ctx.pool.entries[&1]
            .notar_stake_state
            .safe_to_notar_candidates
            .is_empty()
    );
    ctx.add(skip, 6, true);
    ctx.assert_queued(&[]);
    let slot = &ctx.pool.entries[&1];
    assert!(slot.notar_stake_state.safe_to_notar_candidates.is_empty());
    assert!(slot.notar_stake_state.pending_entries.is_empty());

    // New unverified notar votes and bans must not reactivate a completed block.
    ctx.add(notar, 7, true);
    ctx.assert_queued(&[]);
    let slot = ctx.pool.entries.get_mut(&1).unwrap();
    let banned = HashSet::from([slot.notar[&block.block_id].batch[0].sender_identity_pubkey]);
    let (ban_sender, _ban_receiver) = stub_ban_channel_for_tests(1024);
    ctx.pool.ban_pubkeys(&ban_sender, banned);
    ctx.assert_pending_vote_index();
    let slot = ctx.pool.entries.get_mut(&1).unwrap();
    assert!(slot.notar_stake_state.safe_to_notar_candidates.is_empty());
    ctx.assert_notar_indexes();
}

#[test]
fn test_notar_stake_indexes_after_verification() {
    for valid_votes in [0, 3, 4] {
        let mut ctx = TestContext::new(false, 1);
        let block = Block::new_unique(1);
        let vote = Vote::new_notarization_vote(block);
        for voter in 0..4 {
            ctx.add(vote, voter, voter < valid_votes);
        }
        assert_eq!(ctx.verify(), 4);
        let stake = Stake(valid_votes as u64 * 10);
        let slot = &ctx.pool.entries[&1];
        assert_eq!(slot.notar_stake_state.total_notar_stake_observed, stake);
        assert_eq!(slot.notar_stake_state.total_notar_stake_verified, stake);
        assert_eq!(
            slot.notar_stake_state.stakes_observed,
            BTreeSet::from([(stake, block.block_id)]),
        );
        assert_eq!(
            slot.notar_stake_state.stakes_verified,
            BTreeSet::from([(stake, block.block_id)]),
        );

        ctx.add(vote, 4, true);
        let slot = &ctx.pool.entries[&1];
        assert_eq!(
            slot.notar_stake_state.total_notar_stake_observed,
            stake.add(Stake(10))
        );
        assert_eq!(
            slot.notar_stake_state.stakes_observed,
            BTreeSet::from([(Stake(stake.0 + 10), block.block_id)]),
        );
    }
}

#[test]
fn test_notar_stake_indexes_after_ban() {
    let mut ctx = TestContext::new(false, 1);
    let first = Block::new_unique(1);
    let second = Block::new_unique(1);
    for voter in 0..3 {
        ctx.add(Vote::new_notarization_vote(first), voter, true);
    }
    for voter in 3..5 {
        ctx.add(Vote::new_notarization_vote(second), voter, true);
    }
    let slot = ctx.pool.entries.get_mut(&1).unwrap();
    let banned: HashSet<_> = slot.notar[&first.block_id]
        .batch
        .iter()
        .take(2)
        .map(|payload| payload.sender_identity_pubkey)
        .collect();
    let (ban_sender, _ban_receiver) = stub_ban_channel_for_tests(1024);
    ctx.pool.ban_pubkeys(&ban_sender, banned.clone());
    ctx.pool.ban_pubkeys(&ban_sender, banned);
    ctx.assert_pending_vote_index();
    let slot = ctx.pool.entries.get_mut(&1).unwrap();
    assert_eq!(slot.notar_stake_state.total_notar_stake_observed, Stake(30));
    assert_eq!(
        slot.notar_stake_state.stakes_observed,
        BTreeSet::from([(Stake(10), first.block_id), (Stake(20), second.block_id),]),
    );
    assert_eq!(slot.notar_stake_state.total_notar_stake_verified.0, 0);
    assert!(slot.notar_stake_state.stakes_verified.is_empty());
}

#[test]
fn test_safe_to_skip_includes_own_stake() {
    let mut ctx = TestContext::new(false, 1);
    let first = Vote::new_notarization_vote(Block::new_unique(1));
    let second = Vote::new_notarization_vote(Block::new_unique(1));
    let skip = Vote::new_skip_vote(1);
    for voter in 0..3 {
        ctx.add(first, voter, true);
    }
    for voter in 3..5 {
        ctx.add(second, voter, true);
    }
    ctx.add(skip, 5, true);
    ctx.assert_queued(&[]);

    // Isolate the safe-to-skip calculation: 10 own + 50 notar - 30 max
    // + 10 skip reaches 40% with no notar stake verified yet.
    let slot = ctx.pool.entries.get_mut(&1).unwrap();
    slot.my_stake = Stake(10);
    slot.handle_safe_to_skip(&mut ctx.votes, &mut ctx.pool.pending_votes);
    ctx.assert_queued(&[first, second, skip]);
}

#[test]
fn test_batch_size_limit_for_all_vote_types() {
    let slot = 1;
    let block = Block::new_unique(slot);
    for vote in [
        Vote::new_finalization_vote(slot),
        Vote::new_skip_vote(slot),
        Vote::new_skip_fallback_vote(slot),
        Vote::new_notarization_vote(block),
        Vote::new_notarization_fallback_vote(block),
        Vote::new_genesis_vote(block),
    ] {
        // Two batches represent 32% stake, below every applicable threshold.
        let mut ctx = TestContext::new_with_num_validators(false, slot, 100);
        for start in [0, 16] {
            for voter in start..start + 15 {
                ctx.add(vote, voter, true);
                ctx.assert_queued(&[]);
                assert_eq!(ctx.entry(vote).batch.len(), voter - start + 1);
            }
            ctx.add(vote, start + 15, true);
            ctx.assert_queued(&[vote]);
            assert!(ctx.entry(vote).batch.is_empty());
            assert_eq!(ctx.entry(vote).batch_stake, Stake(0));
            assert_eq!(ctx.entry(vote).inflight_stake.0, 160);
            assert_eq!(ctx.verify(), 16);
            assert_eq!(ctx.entry(vote).inflight_stake.0, 0);
            assert_eq!(ctx.entry(vote).verified_stake.0, (start + 16) as u64 * 10);
        }
        ctx.add(vote, 32, true);
        ctx.assert_queued(&[]);
        assert_eq!(ctx.entry(vote).batch.len(), 1);
        assert_eq!(ctx.entry(vote).batch_stake, Stake(10));
    }
}

#[test]
fn test_batch_size_limit_appends_to_inflight_batch() {
    let slot = 1;
    let vote = Vote::new_finalization_vote(slot);
    let mut ctx = TestContext::new_with_num_validators(false, slot, 100);
    for voter in 0..16 {
        ctx.add(vote, voter, true);
    }
    ctx.assert_queued(&[vote]);
    assert_eq!(ctx.entry(vote).inflight_stake.0, 160);

    // Leave the first batch queued while the next 16 votes accumulate.
    for voter in 16..31 {
        ctx.add(vote, voter, true);
        assert_eq!(ctx.entry(vote).batch.len(), voter - 15);
        assert_eq!(ctx.entry(vote).inflight_stake.0, 160);
    }
    ctx.add(vote, 31, true);
    ctx.assert_queued(&[vote]);
    assert!(ctx.entry(vote).batch.is_empty());
    assert_eq!(ctx.entry(vote).batch_stake, Stake(0));
    assert_eq!(ctx.entry(vote).inflight_stake.0, 320);
    assert_eq!(ctx.verify(), 32);
    assert_eq!(ctx.entry(vote).verified_stake.0, 320);
    assert_eq!(ctx.entry(vote).inflight_stake.0, 0);
}

#[test]
fn test_ban_index_targets_pending_votes_across_slots_and_vote_types() {
    let mut ctx = TestContext::new_with_num_validators(false, 1, 40);
    let block = Block::new_unique(1);
    let votes = [
        Vote::new_notarization_vote(block),
        Vote::new_notarization_fallback_vote(Block::new_unique(1)),
        Vote::new_skip_fallback_vote(1),
        Vote::new_finalization_vote(2),
        Vote::new_skip_vote(3),
        Vote::new_genesis_vote(Block::new_unique(4)),
    ];
    for vote in votes {
        ctx.add(vote, 0, true);
        ctx.add(vote, 1, true);
        ctx.add(vote, 2, true);
        ctx.assert_pending_vote_index();
    }
    let banned = ctx.entry(votes[0]).batch[0].sender_identity_pubkey;
    let also_banned = ctx.entry(votes[0]).batch[1].sender_identity_pubkey;
    let survivor = ctx.entry(votes[0]).batch[2].sender_identity_pubkey;
    let (ban_sender, _ban_receiver) = stub_ban_channel_for_tests(1024);

    // A sender with no buffered votes must not affect any entry or the index.
    ctx.pool
        .ban_pubkeys(&ban_sender, HashSet::from([Pubkey::new_unique()]));
    ctx.assert_pending_vote_index();
    for vote in votes {
        assert_eq!(ctx.entry(vote).batch.len(), 3);
    }

    ctx.pool
        .ban_pubkeys(&ban_sender, HashSet::from([banned, also_banned]));
    ctx.assert_pending_vote_index();
    ctx.assert_notar_indexes();
    assert!(!ctx.pool.pending_votes.entries.contains_key(&banned));
    for vote in votes {
        let entry = ctx.entry(vote);
        assert_eq!(entry.batch.len(), 1);
        assert_eq!(entry.batch[0].sender_identity_pubkey, survivor);
        assert_eq!(entry.batch_stake, Stake(10));
        assert_eq!(entry.observed_stake().0, 10);
    }
    // Repeated bans should remove each payload and its stake exactly once.
    ctx.pool
        .ban_pubkeys(&ban_sender, HashSet::from([banned, also_banned, survivor]));
    ctx.assert_pending_vote_index();
    ctx.assert_notar_indexes();
    for vote in votes {
        assert!(ctx.entry(vote).batch.is_empty());
        assert_eq!(ctx.entry(vote).batch_stake, Stake(0));
    }
}

#[test]
fn test_pending_vote_index_is_cleared_on_flush_and_prune() {
    let mut ctx = TestContext::new_with_num_validators(false, 1, 40);
    let vote = Vote::new_finalization_vote(1);
    for voter in 0..BATCH_THRESHOLD {
        ctx.add(vote, voter, true);
        ctx.assert_pending_vote_index();
    }
    ctx.assert_queued(&[vote]);
    assert!(ctx.pool.pending_votes.entries.is_empty());
    assert_eq!(ctx.verify(), BATCH_THRESHOLD);

    // The same sender can have references in both a pruned and a retained slot.
    let old_vote = Vote::new_finalization_vote(2);
    let retained_vote = Vote::new_finalization_vote(3);
    ctx.add(old_vote, 0, true);
    ctx.add(retained_vote, 0, true);
    ctx.assert_pending_vote_index();
    ctx.pool.prune(3 + NUM_SLOTS_FOR_REWARD);
    ctx.assert_pending_vote_index();
    assert!(!ctx.pool.entries.contains_key(&1));
    assert!(!ctx.pool.entries.contains_key(&2));
    assert!(ctx.pool.entries.contains_key(&3));
    assert_eq!(ctx.pool.pending_votes.entries.len(), 1);
    let (ban_sender, _ban_receiver) = stub_ban_channel_for_tests(1024);
    let sender = ctx.entry(retained_vote).batch[0].sender_identity_pubkey;
    ctx.pool.ban_pubkeys(&ban_sender, HashSet::from([sender]));
    ctx.assert_pending_vote_index();
    assert!(ctx.entry(retained_vote).batch.is_empty());
}

#[test]
fn test_update_banned_removes_pending_votes_and_account_pubkeys() {
    let slot = 1;
    let vote = Vote::new_finalization_vote(slot);
    let mut ctx = TestContext::new(false, slot);
    for voter in 0..5 {
        ctx.add(vote, voter, true);
    }
    ctx.assert_queued(&[]);
    let entry = &mut ctx.pool.entries.get_mut(&slot).unwrap().finalize;
    let identities: Vec<_> = entry
        .batch
        .iter()
        .map(|payload| payload.sender_identity_pubkey)
        .collect();

    entry.ban_pubkeys(&HashSet::new());
    entry.ban_pubkeys(&HashSet::from([Pubkey::new_unique()]));
    assert_eq!(entry.batch.len(), 5);
    assert_eq!(entry.batch_stake, Stake(50));

    let banned = HashSet::from([identities[0], identities[2], identities[4], identities[2]]);
    entry.ban_pubkeys(&banned);
    entry.ban_pubkeys(&banned);
    assert_eq!(
        entry
            .batch
            .iter()
            .map(|payload| payload.sender_identity_pubkey)
            .collect::<Vec<_>>(),
        vec![identities[1], identities[3]],
    );
    assert_eq!(entry.batch_stake, Stake(20));
    assert_eq!(entry.observed_stake().0, 20);

    entry.ban_pubkeys(&identities.into_iter().collect());
    assert!(entry.batch.is_empty());
    assert_eq!(entry.batch_stake, Stake(0));
}

#[test]
fn test_certificate_thresholds() {
    let slot = 1;
    let block = Block::new_unique(slot);
    for (vote, required_votes) in [
        (Vote::new_finalization_vote(slot), 6),
        (Vote::new_skip_fallback_vote(slot), 6),
        (Vote::new_notarization_fallback_vote(block), 6),
        (Vote::new_genesis_vote(block), 9),
    ] {
        let mut ctx = TestContext::new(false, slot);
        for voter in 0..required_votes - 1 {
            ctx.add(vote, voter, true);
            ctx.assert_queued(&[]);
        }
        ctx.add(vote, required_votes - 1, true);
        ctx.assert_queued(&[vote]);
        assert_eq!(ctx.verify(), required_votes);
        assert_eq!(ctx.entry(vote).verified_stake.0, required_votes as u64 * 10);
        ctx.add(vote, required_votes, true);
        ctx.assert_queued(&[]);
    }
}

#[test]
fn test_notar_event_then_notar_cert_then_fast_finalize() {
    let slot = 1;
    let mut ctx = TestContext::new(false, slot);
    let vote = Vote::new_notarization_vote(Block::new_unique(slot));
    for (start, end) in [(0, 4), (4, 6), (6, 8)] {
        for voter in start..end - 1 {
            ctx.add(vote, voter, true);
            ctx.assert_queued(&[]);
        }
        ctx.add(vote, end - 1, true);
        ctx.assert_queued(&[vote]);
        assert_eq!(ctx.verify(), end - start);
        assert_eq!(ctx.entry(vote).verified_stake.0, end as u64 * 10);
    }
    ctx.add(vote, 8, true);
    ctx.assert_queued(&[]);
}

#[test]
fn test_safe_to_skip_then_skip_certificate() {
    let slot = 1;
    let mut ctx = TestContext::new(false, slot);
    let skip = Vote::new_skip_vote(slot);
    let fallback = Vote::new_skip_fallback_vote(slot);
    for voter in 0..3 {
        ctx.add(skip, voter, true);
        ctx.assert_queued(&[]);
    }
    ctx.add(skip, 3, true);
    ctx.assert_queued(&[skip]);
    assert_eq!(ctx.verify(), 4);
    ctx.add(fallback, 4, true);
    ctx.assert_queued(&[]);
    ctx.add(fallback, 5, true);
    ctx.assert_queued(&[fallback]);
    assert_eq!(ctx.verify(), 2);
    assert_eq!(ctx.entry(skip).verified_stake.0, 40);
    assert_eq!(ctx.entry(fallback).verified_stake.0, 20);
}

#[test]
fn test_combined_safe_to_notar_both_arrival_orders() {
    let slot = 1;
    for skip_first in [false, true] {
        let mut ctx = TestContext::new(false, slot);
        let notar = Vote::new_notarization_vote(Block::new_unique(slot));
        let skip = Vote::new_skip_vote(slot);
        // Both the 20% notar and 60% combined conditions must hold.
        if skip_first {
            for voter in 0..5 {
                ctx.add(skip, voter, true);
            }
            assert_eq!(ctx.verify(), 5);
            ctx.add(notar, 5, true);
            ctx.assert_queued(&[]);
            ctx.add(notar, 6, true);
            ctx.assert_queued(&[notar]);
            assert_eq!(ctx.verify(), 2);
        } else {
            for voter in 0..3 {
                ctx.add(notar, voter, true);
                ctx.assert_queued(&[]);
            }
            for voter in 3..5 {
                ctx.add(skip, voter, true);
                ctx.assert_queued(&[]);
            }
            ctx.add(skip, 5, true);
            ctx.assert_queued(&[notar, skip]);
            assert_eq!(ctx.verify(), 6);
        }
    }
}

#[test]
fn test_safe_to_skip_excludes_largest_notar_block() {
    let slot = 1;
    let mut ctx = TestContext::new(false, slot);
    let largest = Vote::new_notarization_vote(Block::new_unique(slot));
    let other = Vote::new_notarization_vote(Block::new_unique(slot));
    let skip = Vote::new_skip_vote(slot);
    for voter in 0..3 {
        ctx.add(largest, voter, true);
    }
    for voter in 3..5 {
        ctx.add(other, voter, true);
    }
    ctx.add(skip, 5, true);
    ctx.assert_queued(&[]);
    ctx.add(skip, 6, true);
    ctx.assert_queued(&[largest, other, skip]);
    assert_eq!(ctx.verify(), 7);
}

#[test]
fn test_notar_fallback_certificate_after_notar_event() {
    let slot = 1;
    let mut ctx = TestContext::new(false, slot);
    let block = Block::new_unique(slot);
    let notar = Vote::new_notarization_vote(block);
    let fallback = Vote::new_notarization_fallback_vote(block);
    for voter in 0..4 {
        ctx.add(notar, voter, true);
    }
    assert_eq!(ctx.verify(), 4);
    ctx.add(fallback, 4, true);
    ctx.assert_queued(&[]);
    ctx.add(fallback, 5, true);
    ctx.assert_queued(&[fallback]);
    assert_eq!(ctx.verify(), 2);
}

#[test]
fn test_failed_verification_reduces_observed_stake_and_retries_threshold() {
    let slot = 1;
    for valid_votes in [0, 3] {
        let mut ctx = TestContext::new(false, slot);
        let vote = Vote::new_finalization_vote(slot);
        for voter in 0..6 {
            ctx.add(vote, voter, voter < valid_votes);
        }
        assert_eq!(ctx.entry(vote).inflight_stake.0, 60);
        assert_eq!(ctx.verify(), 6);
        assert_eq!(ctx.entry(vote).verified_stake.0, valid_votes as u64 * 10);
        assert_eq!(ctx.entry(vote).inflight_stake.0, 0);
        assert_eq!(ctx.entry(vote).observed_stake().0, valid_votes as u64 * 10);
        for voter in 6..9 {
            ctx.add(vote, voter, true);
            if valid_votes == 3 && voter == 8 {
                ctx.assert_queued(&[vote]);
                assert_eq!(ctx.verify(), 3);
                assert_eq!(ctx.entry(vote).verified_stake.0, 60);
            } else {
                ctx.assert_queued(&[]);
            }
        }
    }
}

#[test]
fn test_inflight_stake_counts_toward_larger_thresholds() {
    let slot = 1;
    let mut ctx = TestContext::new(false, slot);
    let vote = Vote::new_notarization_vote(Block::new_unique(slot));
    for voter in 0..4 {
        ctx.add(vote, voter, true);
    }
    assert_eq!(ctx.entry(vote).inflight_stake.0, 40);
    assert_eq!(ctx.entry(vote).verified_stake.0, 0);
    // Leave the event batch unverified. Later arrivals append to it and must
    // include its stake when checking the notar and fast-finalize thresholds.
    for voter in 4..8 {
        ctx.add(vote, voter, true);
        assert_eq!(ctx.entry(vote).inflight_stake.0, (voter as u64 + 1) * 10);
        assert_eq!(ctx.entry(vote).batch_stake, Stake(0));
    }
    ctx.assert_queued(&[vote]);
    assert_eq!(ctx.verify(), 8);
    assert_eq!(ctx.entry(vote).inflight_stake.0, 0);
    assert_eq!(ctx.entry(vote).verified_stake.0, 80);
}

#[test]
fn test_rewards_verification_counts_toward_combined_certificates() {
    let slot = 1;
    let block = Block::new_unique(slot);
    for (reward_vote, fallback) in [
        (
            Vote::new_skip_vote(slot),
            Vote::new_skip_fallback_vote(slot),
        ),
        (
            Vote::new_notarization_vote(block),
            Vote::new_notarization_fallback_vote(block),
        ),
    ] {
        let mut ctx = TestContext::new(true, slot);
        assert_eq!(ctx.my_stake.0, 10);
        ctx.add(reward_vote, 0, true);
        ctx.assert_queued(&[reward_vote]);
        assert_eq!(ctx.verify(), 1);
        assert_eq!(ctx.entry(reward_vote).verified_stake.0, 10);
        for voter in 1..4 {
            ctx.add(fallback, voter, true);
            ctx.assert_queued(&[]);
        }
        // 10% own stake + 10% verified for rewards + 40% fallback = 60%.
        ctx.add(fallback, 4, true);
        ctx.assert_queued(&[fallback]);
        assert_eq!(ctx.verify(), 4);
        assert_eq!(ctx.entry(reward_vote).verified_stake.0, 10);
        assert_eq!(ctx.entry(fallback).verified_stake.0, 40);
    }
}

#[test]
fn test_rewards_inflight_stake_counts_toward_combined_certificates() {
    let slot = 1;
    let block = Block::new_unique(slot);
    for (reward_vote, fallback) in [
        (
            Vote::new_skip_vote(slot),
            Vote::new_skip_fallback_vote(slot),
        ),
        (
            Vote::new_notarization_vote(block),
            Vote::new_notarization_fallback_vote(block),
        ),
    ] {
        let mut ctx = TestContext::new(true, slot);
        ctx.add(reward_vote, 0, true);
        assert_eq!(ctx.entry(reward_vote).verified_stake.0, 0);
        assert_eq!(ctx.entry(reward_vote).inflight_stake.0, 10);
        for voter in 1..4 {
            ctx.add(fallback, voter, true);
            ctx.assert_queued(&[reward_vote]);
        }
        ctx.add(fallback, 4, true);
        ctx.assert_queued(&[reward_vote, fallback]);
        assert_eq!(ctx.verify(), 5);
        assert_eq!(ctx.entry(reward_vote).verified_stake.0, 10);
        assert_eq!(ctx.entry(fallback).verified_stake.0, 40);
        assert_eq!(ctx.entry(reward_vote).inflight_stake.0, 0);
        assert_eq!(ctx.entry(fallback).inflight_stake.0, 0);
    }
}

#[test]
fn test_failed_rewards_verification_does_not_count_toward_thresholds() {
    let slot = 1;
    let block = Block::new_unique(slot);
    for (reward_vote, fallback) in [
        (
            Vote::new_skip_vote(slot),
            Vote::new_skip_fallback_vote(slot),
        ),
        (
            Vote::new_notarization_vote(block),
            Vote::new_notarization_fallback_vote(block),
        ),
    ] {
        let mut ctx = TestContext::new(true, slot);
        ctx.add(reward_vote, 0, false);
        ctx.assert_queued(&[reward_vote]);
        assert_eq!(ctx.verify(), 1);
        assert_eq!(ctx.entry(reward_vote).verified_stake.0, 0);
        assert_eq!(ctx.entry(reward_vote).inflight_stake.0, 0);
        for voter in 1..5 {
            ctx.add(fallback, voter, true);
            ctx.assert_queued(&[]);
        }
        ctx.add(fallback, 5, true);
        ctx.assert_queued(&[fallback]);
        assert_eq!(ctx.verify(), 5);
        assert_eq!(ctx.entry(fallback).verified_stake.0, 50);
    }
}

#[test]
fn test_combined_certificates_queue_both_vote_types() {
    let slot = 1;
    let block = Block::new_unique(slot);
    for ((vote, fallback), fallback_first) in [
        (
            Vote::new_skip_vote(slot),
            Vote::new_skip_fallback_vote(slot),
        ),
        (
            Vote::new_notarization_vote(block),
            Vote::new_notarization_fallback_vote(block),
        ),
    ]
    .into_iter()
    .flat_map(|pair| [false, true].map(|fallback_first| (pair, fallback_first)))
    {
        let mut ctx = TestContext::new(false, slot);
        let (first, second) = if fallback_first {
            (fallback, vote)
        } else {
            (vote, fallback)
        };
        for voter in 0..3 {
            ctx.add(first, voter, true);
            ctx.assert_queued(&[]);
        }
        for voter in 3..5 {
            ctx.add(second, voter, true);
            ctx.assert_queued(&[]);
        }
        ctx.add(second, 5, true);
        ctx.assert_queued(&[vote, fallback]);
        assert_eq!(ctx.verify(), 6);
        assert_eq!(ctx.entry(vote).verified_stake.0, 30);
        assert_eq!(ctx.entry(fallback).verified_stake.0, 30);
        ctx.add(fallback, 6, true);
        ctx.assert_queued(&[]);
    }
}

#[test]
fn test_combined_safe_to_notar_counts_verified_and_inflight_stake() {
    let slot = 1;
    for verify_first in [false, true] {
        let mut ctx = TestContext::new(false, slot);
        let notar = Vote::new_notarization_vote(Block::new_unique(slot));
        let skip = Vote::new_skip_vote(slot);
        for voter in 0..4 {
            ctx.add(notar, voter, true);
        }
        if verify_first {
            assert_eq!(ctx.verify(), 4);
        }
        ctx.add(skip, 4, true);
        if verify_first {
            ctx.assert_queued(&[]);
        } else {
            ctx.assert_queued(&[notar]);
        }
        ctx.add(skip, 5, true);
        if verify_first {
            ctx.assert_queued(&[skip]);
            assert_eq!(ctx.verify(), 2);
        } else {
            ctx.assert_queued(&[notar, skip]);
            assert_eq!(ctx.verify(), 6);
        }
        assert_eq!(ctx.entry(notar).verified_stake.0, 40);
        assert_eq!(ctx.entry(skip).verified_stake.0, 20);
    }
}

#[test]
fn test_safe_to_skip_counts_verified_and_inflight_notar_stake() {
    let slot = 1;
    for verify_first in [false, true] {
        let mut ctx = TestContext::new(false, slot);
        let largest = Vote::new_notarization_vote(Block::new_unique(slot));
        let other = Vote::new_notarization_vote(Block::new_unique(slot));
        let skip = Vote::new_skip_vote(slot);
        for voter in 0..4 {
            ctx.add(largest, voter, true);
        }
        if verify_first {
            assert_eq!(ctx.verify(), 4);
        }
        for voter in 4..7 {
            ctx.add(other, voter, true);
        }
        if verify_first {
            ctx.assert_queued(&[]);
        } else {
            ctx.assert_queued(&[largest]);
        }
        // Excluding the largest block leaves 30% notar + 10% skip.
        ctx.add(skip, 7, true);
        if verify_first {
            ctx.assert_queued(&[other, skip]);
            assert_eq!(ctx.verify(), 4);
        } else {
            ctx.assert_queued(&[largest, other, skip]);
            assert_eq!(ctx.verify(), 8);
        }
        assert_eq!(ctx.entry(largest).verified_stake.0, 40);
        assert_eq!(ctx.entry(other).verified_stake.0, 30);
        assert_eq!(ctx.entry(skip).verified_stake.0, 10);
    }
}
