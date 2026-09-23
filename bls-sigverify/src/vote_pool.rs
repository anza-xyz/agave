use {
    crate::{
        bls_vote_sigverify::batch::Batch, rewards::rewards_wants_vote,
        unverified_votes_batch::UnverifiedVotePayload,
    },
    agave_votor_messages::{
        certificate::{
            FAST_FINALIZE_CERT_THRESHOLD, FINALIZE_CERT_THRESHOLD, NOTAR_CERT_THRESHOLD,
            NOTAR_FALLBACK_CERT_THRESHOLD, SKIP_CERT_THRESHOLD,
        },
        consensus_message::{Block, BlockId},
        fraction::Fraction,
        migration::GENESIS_VOTE_THRESHOLD,
        reward_certificate::NUM_SLOTS_FOR_REWARD,
        vote::Vote,
        wire::VotePayloadToSign,
    },
    solana_clock::Slot,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, epoch_stakes::BLSPubkeyToRankMap},
    std::{
        collections::{HashMap, HashSet, hash_map::Entry},
        num::Saturating,
    },
};

const MAX_NOTAR_FALLBACK_ENTRIES: usize = 3;
const SAFE_TO_SKIP: Fraction = Fraction::from_percentage(40);
const SAFE_TO_NOTAR_NOTAR_ONLY: Fraction = Fraction::from_percentage(40);
const SAFE_TO_NOTAR_COMBINED_NOTAR_AND_SKIP: Fraction = Fraction::from_percentage(60);
const SAFE_TO_NOTAR_COMBINED_NOTAR_ONLY: Fraction = Fraction::from_percentage(20);

pub(crate) enum VotePoolError {
    InvalidVote(Pubkey),
    DuplicateVote,
}

#[derive(Default)]
pub(crate) struct VotePool {
    entries: HashMap<Slot, SlotEntry>,
}

impl VotePool {
    pub(crate) fn add_vote(
        &mut self,
        my_pubkey: &Pubkey,
        root_bank: &Bank,
        leader_schedule: &LeaderScheduleCache,
        rank_map: &BLSPubkeyToRankMap,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        my_stake: Saturating<u64>,
        unverified_payload: UnverifiedVotePayload,
    ) -> Result<(), VotePoolError> {
        let vote_msg = &unverified_payload.vote_message;
        let vote = &vote_msg.vote;
        match self.entries.entry(vote.slot()) {
            Entry::Occupied(mut e) => e.get_mut().add_vote(
                my_pubkey,
                root_bank,
                leader_schedule,
                rank_map,
                votes,
                unverified_payload,
            ),
            Entry::Vacant(e) => {
                let mut slot_entry = SlotEntry::new(vote.slot(), vote_msg.shred_version, my_stake);
                let res = slot_entry.add_vote(
                    my_pubkey,
                    root_bank,
                    leader_schedule,
                    rank_map,
                    votes,
                    unverified_payload,
                );
                if res.is_ok() {
                    e.insert(slot_entry);
                }
                res
            }
        }
    }

    pub(crate) fn update_verified(&mut self, votes: &mut HashMap<VotePayloadToSign, Batch>) {
        for (vote_payload_to_sign, batch) in votes.drain() {
            let Some(entry) = self.entries.get_mut(&vote_payload_to_sign.slot()) else {
                continue;
            };
            entry.update_verified(vote_payload_to_sign, batch.verified_stake());
        }
    }

    pub(crate) fn prune(&mut self, root_slot: Slot) {
        // To support rewards, we need to keep older notar and skip votes.
        // Simpler to keep all votes for older slots.
        let slot_to_keep = root_slot.saturating_sub(NUM_SLOTS_FOR_REWARD);
        self.entries.retain(|slot, _| slot >= &slot_to_keep);
    }
}

struct SlotEntry {
    my_stake: Saturating<u64>,
    skip: SkipState,
    finalize: VoteEntry,
    genesis: HashMap<BlockId, VoteEntry>,
    genesis_votes: HashSet<u16>,
    notar: HashMap<BlockId, VoteEntry>,
    notar_votes: HashSet<u16>,
    notar_fallback: HashMap<BlockId, VoteEntry>,
    // mapping from rank to blocks the validator has voted for.
    notar_fallback_votes: HashMap<u16, Vec<BlockId>>,
}

impl SlotEntry {
    fn new(slot: Slot, shred_version: u16, my_stake: Saturating<u64>) -> Self {
        let finalize_vote_payload_to_sign =
            VotePayloadToSign::new_from_vote(Vote::new_finalization_vote(slot), shred_version);
        Self {
            my_stake,
            skip: SkipState::new(slot, shred_version, my_stake),
            finalize: VoteEntry::new(finalize_vote_payload_to_sign, my_stake),
            genesis: HashMap::new(),
            genesis_votes: HashSet::new(),
            notar: HashMap::new(),
            notar_votes: HashSet::new(),
            notar_fallback: HashMap::new(),
            notar_fallback_votes: HashMap::new(),
        }
    }

    fn add_vote(
        &mut self,
        my_pubkey: &Pubkey,
        root_bank: &Bank,
        leader_schedule: &LeaderScheduleCache,
        rank_map: &BLSPubkeyToRankMap,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        unverified_payload: UnverifiedVotePayload,
    ) -> Result<(), VotePoolError> {
        let rank = unverified_payload.rank;
        let vote_slot = unverified_payload.vote_message.vote.slot();
        let shred_version = unverified_payload.vote_message.shred_version;

        match &unverified_payload.vote_message.vote {
            Vote::Finalize(_) => {
                if self.skip.skip.ranks.contains(&rank)
                    || self.skip.skip_fallback.ranks.contains(&rank)
                    || self.notar_fallback_votes.contains_key(&rank)
                    || self.genesis_votes.contains(&rank)
                {
                    return Err(VotePoolError::InvalidVote(
                        unverified_payload.sender_identity_pubkey,
                    ));
                }
                if self.finalize.ranks.contains(&rank) {
                    return Err(VotePoolError::DuplicateVote);
                }
                self.finalize
                    .try_build_finalize_batch(rank_map, votes, unverified_payload)?;
            }

            Vote::Skip(_) => {
                if self.notar_votes.contains(&rank)
                    || self.finalize.ranks.contains(&rank)
                    || self.skip.skip_fallback.ranks.contains(&rank)
                    || self.genesis_votes.contains(&rank)
                {
                    return Err(VotePoolError::InvalidVote(
                        unverified_payload.sender_identity_pubkey,
                    ));
                }
                if self.skip.skip.ranks.contains(&rank) {
                    return Err(VotePoolError::DuplicateVote);
                }
                self.skip.skip.add_unverified_vote(unverified_payload);
                let vote = Vote::new_skip_vote(vote_slot);
                if rewards_wants_vote(my_pubkey, leader_schedule, root_bank.slot(), &vote) {
                    self.skip.skip.do_add_to_votes(votes, rank_map);
                }
                self.skip.try_build_batch(votes, rank_map);
                self.check_all_safe_to_notar(vote_slot, rank_map, votes);
            }

            Vote::SkipFallback(_) => {
                if self.finalize.ranks.contains(&rank)
                    || self.skip.skip.ranks.contains(&rank)
                    || self.genesis_votes.contains(&rank)
                {
                    return Err(VotePoolError::InvalidVote(
                        unverified_payload.sender_identity_pubkey,
                    ));
                }
                if self.skip.skip_fallback.ranks.contains(&rank) {
                    return Err(VotePoolError::DuplicateVote);
                }
                self.skip
                    .skip_fallback
                    .add_unverified_vote(unverified_payload);
                self.skip.try_build_batch(votes, rank_map);
            }

            Vote::Genesis(genesis) => {
                if self.skip.skip.ranks.contains(&rank)
                    || self.skip.skip_fallback.ranks.contains(&rank)
                    || self.finalize.ranks.contains(&rank)
                    || self.notar_votes.contains(&rank)
                    || self.notar_fallback_votes.contains_key(&rank)
                {
                    return Err(VotePoolError::InvalidVote(
                        unverified_payload.sender_identity_pubkey,
                    ));
                }
                if !self.genesis_votes.insert(rank) {
                    if let Some(entry) = self.genesis.get(&genesis.block.block_id)
                        && entry.ranks.contains(&rank)
                    {
                        return Err(VotePoolError::DuplicateVote);
                    }
                    return Err(VotePoolError::InvalidVote(
                        unverified_payload.sender_identity_pubkey,
                    ));
                }
                let vote_payload_to_sign =
                    VotePayloadToSign::new_from_vote(Vote::Genesis(*genesis), shred_version);
                let vote_entry = self
                    .genesis
                    .entry(genesis.block.block_id)
                    .or_insert_with(|| VoteEntry::new(vote_payload_to_sign, self.my_stake));
                vote_entry.add_unverified_vote(unverified_payload);
                vote_entry.try_add_to_votes(votes, rank_map, GENESIS_VOTE_THRESHOLD);
            }

            Vote::Notarize(notar) => {
                if self.skip.skip.ranks.contains(&rank) || self.genesis_votes.contains(&rank) {
                    return Err(VotePoolError::InvalidVote(
                        unverified_payload.sender_identity_pubkey,
                    ));
                }
                if let Some(entry) = self.notar_fallback.get(&notar.block.block_id)
                    && entry.ranks.contains(&rank)
                {
                    return Err(VotePoolError::InvalidVote(
                        unverified_payload.sender_identity_pubkey,
                    ));
                }
                if !self.notar_votes.insert(rank) {
                    if let Some(notar_entry) = self.notar.get(&notar.block.block_id)
                        && notar_entry.ranks.contains(&rank)
                    {
                        return Err(VotePoolError::DuplicateVote);
                    }
                    return Err(VotePoolError::InvalidVote(
                        unverified_payload.sender_identity_pubkey,
                    ));
                }
                let vote_payload_to_sign =
                    VotePayloadToSign::new_from_vote(Vote::Notarize(*notar), shred_version);
                let block = notar.block;
                let entry = self
                    .notar
                    .entry(block.block_id)
                    .or_insert_with(|| VoteEntry::new(vote_payload_to_sign, self.my_stake));
                entry.add_unverified_vote(unverified_payload);
                let vote = Vote::new_notarization_vote(block);
                if rewards_wants_vote(my_pubkey, leader_schedule, root_bank.slot(), &vote) {
                    entry.do_add_to_votes(votes, rank_map);
                }
                self.handle_notar_and_nf_state(block, rank_map, votes);
                self.handle_safe_to_notar(block, rank_map, votes);
            }

            Vote::NotarizeFallback(nf) => {
                let block = nf.block;
                if let Some(block_ids) = self.notar_fallback_votes.get(&rank)
                    && block_ids.contains(&block.block_id)
                {
                    return Err(VotePoolError::DuplicateVote);
                }
                if self.finalize.ranks.contains(&rank) || self.genesis_votes.contains(&rank) {
                    return Err(VotePoolError::InvalidVote(
                        unverified_payload.sender_identity_pubkey,
                    ));
                }
                if let Some(block_ids) = self.notar_fallback_votes.get(&rank)
                    && block_ids.len() >= MAX_NOTAR_FALLBACK_ENTRIES
                {
                    return Err(VotePoolError::InvalidVote(
                        unverified_payload.sender_identity_pubkey,
                    ));
                }
                if let Some(entry) = self.notar.get(&block.block_id)
                    && entry.ranks.contains(&rank)
                {
                    return Err(VotePoolError::InvalidVote(
                        unverified_payload.sender_identity_pubkey,
                    ));
                }
                let vote_payload_to_sign =
                    VotePayloadToSign::new_from_vote(Vote::NotarizeFallback(*nf), shred_version);
                let entry = self
                    .notar_fallback
                    .entry(block.block_id)
                    .or_insert_with(|| VoteEntry::new(vote_payload_to_sign, self.my_stake));
                entry.add_unverified_vote(unverified_payload);
                self.notar_fallback_votes
                    .entry(rank)
                    .or_default()
                    .push(block.block_id);
                self.handle_notar_and_nf_state(block, rank_map, votes);
            }
        }
        self.handle_safe_to_skip(rank_map, votes);
        Ok(())
    }

    fn update_verified(&mut self, vote_payload_to_sign: VotePayloadToSign, verified_stake: u64) {
        match vote_payload_to_sign {
            VotePayloadToSign::Notar {
                block,
                shred_version: _,
            } => {
                if let Some(entry) = self.notar.get_mut(&block.block_id) {
                    entry.update_verified(verified_stake);
                }
            }
            VotePayloadToSign::NotarFallback {
                block,
                shred_version: _,
            } => {
                if let Some(entry) = self.notar_fallback.get_mut(&block.block_id) {
                    entry.update_verified(verified_stake);
                }
            }
            VotePayloadToSign::Skip { .. } => {
                self.skip.skip.update_verified(verified_stake);
            }
            VotePayloadToSign::SkipFallback { .. } => {
                self.skip.skip_fallback.update_verified(verified_stake);
            }
            VotePayloadToSign::Finalize { .. } => {
                self.finalize.update_verified(verified_stake);
            }
            VotePayloadToSign::Genesis {
                block,
                shred_version: _,
            } => {
                if let Some(entry) = self.genesis.get_mut(&block.block_id) {
                    entry.update_verified(verified_stake);
                }
            }
        }
    }

    fn handle_notar_and_nf_state(
        &mut self,
        block: Block,
        rank_map: &BLSPubkeyToRankMap,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
    ) {
        match (
            self.notar.get_mut(&block.block_id),
            self.notar_fallback.get_mut(&block.block_id),
        ) {
            (None, None) => (),
            (Some(entry), None) => {
                handle_notar_state(rank_map, votes, entry);
            }
            (None, Some(entry)) => {
                entry.try_add_to_votes(votes, rank_map, NOTAR_FALLBACK_CERT_THRESHOLD);
            }

            (Some(notar_entry), Some(nf_entry)) => {
                handle_notar_state(rank_map, votes, notar_entry);
                let observed_fraction = Fraction::new(
                    (notar_entry.observed_stake() + nf_entry.observed_stake() + self.my_stake).0,
                    rank_map.total_stake(),
                );
                let already_verified = notar_entry.already_verified + nf_entry.already_verified;
                let already_verified = Fraction::new(already_verified.0, rank_map.total_stake());
                if already_verified < NOTAR_FALLBACK_CERT_THRESHOLD
                    && observed_fraction >= NOTAR_FALLBACK_CERT_THRESHOLD
                {
                    notar_entry.do_add_to_votes(votes, rank_map);
                    nf_entry.do_add_to_votes(votes, rank_map);
                }
            }
        }
    }

    fn check_all_safe_to_notar(
        &mut self,
        slot: Slot,
        rank_map: &BLSPubkeyToRankMap,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
    ) {
        let block_ids = self.notar.keys().cloned().collect::<Vec<_>>();
        for block_id in block_ids {
            let block = Block { block_id, slot };
            self.handle_safe_to_notar(block, rank_map, votes);
        }
    }

    fn handle_safe_to_notar(
        &mut self,
        block: Block,
        rank_map: &BLSPubkeyToRankMap,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
    ) {
        let Some(notar_entry) = self.notar.get_mut(&block.block_id) else {
            return;
        };
        notar_entry.try_add_to_votes(votes, rank_map, SAFE_TO_NOTAR_NOTAR_ONLY);

        let current_combined = Fraction::new(
            (notar_entry.observed_stake() + self.skip.skip.observed_stake() + self.my_stake).0,
            rank_map.total_stake(),
        );
        let already_verified_combined =
            notar_entry.already_verified + self.skip.skip.already_verified;
        let already_verified_combined =
            Fraction::new(already_verified_combined.0, rank_map.total_stake());

        let already_verified_notar =
            Fraction::new(notar_entry.already_verified.0, rank_map.total_stake());
        let current_notar = Fraction::new(
            (notar_entry.observed_stake() + self.my_stake).0,
            rank_map.total_stake(),
        );

        if (already_verified_combined < SAFE_TO_NOTAR_COMBINED_NOTAR_AND_SKIP
            || already_verified_notar < SAFE_TO_NOTAR_COMBINED_NOTAR_ONLY)
            && current_combined >= SAFE_TO_NOTAR_COMBINED_NOTAR_AND_SKIP
            && current_notar >= SAFE_TO_NOTAR_COMBINED_NOTAR_ONLY
        {
            notar_entry.do_add_to_votes(votes, rank_map);
            self.skip.skip.do_add_to_votes(votes, rank_map);
        }
    }

    fn handle_safe_to_skip(
        &mut self,
        rank_map: &BLSPubkeyToRankMap,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
    ) {
        let mut already_verified_max_notar = Saturating(0);
        let mut current_max_notar = self.my_stake;
        let mut already_verified_sum_notar = Saturating(0);
        let mut current_sum_notar = self.my_stake;
        for notar in self.notar.values() {
            let current_notar = notar.observed_stake();
            current_sum_notar += current_notar;
            current_max_notar = current_max_notar.max(current_notar);
            already_verified_sum_notar += notar.already_verified;
            already_verified_max_notar = already_verified_max_notar.max(notar.already_verified);
        }
        let already_verified = self.skip.skip.already_verified + already_verified_sum_notar
            - already_verified_max_notar;
        let current = self.skip.skip.observed_stake() + current_sum_notar - current_max_notar;

        let already_verified = Fraction::new(already_verified.0, rank_map.total_stake());
        let current = Fraction::new(current.0, rank_map.total_stake());
        if already_verified >= SAFE_TO_SKIP || current < SAFE_TO_SKIP {
            return;
        }
        self.skip.skip.do_add_to_votes(votes, rank_map);
        for notar in self.notar.values_mut() {
            notar.do_add_to_votes(votes, rank_map);
        }
    }
}

struct VoteEntry {
    vote_payload_to_sign: VotePayloadToSign,
    /// Stake of the node
    my_stake: Saturating<u64>,
    /// Stake that has already been verified.
    already_verified: Saturating<u64>,
    /// Stake that is in in the process of being verified.
    inflight_stake: Saturating<u64>,
    /// validatores included in the current batch.
    ranks: HashSet<u16>,
    /// the current batch of votes waiting to be verified
    batch: Vec<UnverifiedVotePayload>,
    /// the current bactch of sender vote account pubkeys waiting to be verified.
    sender_vote_account_pubkeys: Vec<Pubkey>,
    /// stake of the current batch.
    batch_stake: Saturating<u64>,
}

impl VoteEntry {
    fn new(vote_payload_to_sign: VotePayloadToSign, my_stake: Saturating<u64>) -> Self {
        Self {
            vote_payload_to_sign,
            my_stake,
            already_verified: Saturating(0),
            inflight_stake: Saturating(0),
            ranks: HashSet::new(),
            batch: Vec::new(),
            sender_vote_account_pubkeys: Vec::new(),
            batch_stake: Saturating(0),
        }
    }

    fn add_unverified_vote(&mut self, unverified_payload: UnverifiedVotePayload) {
        assert!(self.ranks.insert(unverified_payload.rank));
        self.sender_vote_account_pubkeys
            .push(unverified_payload.sender_vote_account_pubkey);
        self.batch_stake += unverified_payload.stake.get();
        self.batch.push(unverified_payload);
    }

    fn try_add_to_votes(
        &mut self,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        rank_map: &BLSPubkeyToRankMap,
        threshold: Fraction,
    ) {
        let already_verified_fraction =
            Fraction::new(self.already_verified.0, rank_map.total_stake());
        let observed_fraction = Fraction::new(
            (self.observed_stake() + self.my_stake).0,
            rank_map.total_stake(),
        );
        if already_verified_fraction >= threshold || observed_fraction < threshold {
            return;
        }
        self.do_add_to_votes(votes, rank_map);
    }

    fn do_add_to_votes(
        &mut self,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        rank_map: &BLSPubkeyToRankMap,
    ) {
        if self.batch.is_empty() {
            return;
        }
        let Self {
            my_stake: _,
            already_verified: _,
            inflight_stake,
            ranks: _,
            batch,
            sender_vote_account_pubkeys,
            batch_stake,
            vote_payload_to_sign,
        } = self;
        let payloads = std::mem::take(batch);
        let sender_vote_account_pubkeys = std::mem::take(sender_vote_account_pubkeys);
        *inflight_stake += *batch_stake;
        *batch_stake = Saturating(0);
        match votes.entry(*vote_payload_to_sign) {
            Entry::Vacant(e) => {
                let batch = Batch::new(
                    *vote_payload_to_sign,
                    payloads,
                    sender_vote_account_pubkeys,
                    rank_map.len(),
                );
                e.insert(batch);
            }
            Entry::Occupied(mut e) => {
                let batch = e.get_mut();
                batch.push_list(payloads, sender_vote_account_pubkeys);
            }
        }
    }

    fn try_build_finalize_batch(
        &mut self,
        rank_map: &BLSPubkeyToRankMap,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        unverified_payload: UnverifiedVotePayload,
    ) -> Result<(), VotePoolError> {
        self.add_unverified_vote(unverified_payload);
        self.try_add_to_votes(votes, rank_map, FINALIZE_CERT_THRESHOLD);
        Ok(())
    }

    fn update_verified(&mut self, verified: u64) {
        self.inflight_stake = Saturating(0);
        self.already_verified += verified;
    }

    fn observed_stake(&self) -> Saturating<u64> {
        self.already_verified + self.inflight_stake + self.batch_stake
    }
}

struct SkipState {
    my_stake: Saturating<u64>,
    skip: VoteEntry,
    skip_fallback: VoteEntry,
}

impl SkipState {
    fn new(slot: Slot, shred_version: u16, my_stake: Saturating<u64>) -> Self {
        let skip_vote_payload_to_sign =
            VotePayloadToSign::new_from_vote(Vote::new_skip_vote(slot), shred_version);
        let sf_vote_payload_to_sign =
            VotePayloadToSign::new_from_vote(Vote::new_skip_fallback_vote(slot), shred_version);
        Self {
            my_stake,
            skip: VoteEntry::new(skip_vote_payload_to_sign, my_stake),
            skip_fallback: VoteEntry::new(sf_vote_payload_to_sign, my_stake),
        }
    }

    fn try_build_batch(
        &mut self,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        rank_map: &BLSPubkeyToRankMap,
    ) {
        let already_verified = self.skip.already_verified + self.skip_fallback.already_verified;
        let observed_fraction = Fraction::new(
            (self.my_stake + self.skip.observed_stake() + self.skip_fallback.observed_stake()).0,
            rank_map.total_stake(),
        );
        let already_verified = Fraction::new(already_verified.0, rank_map.total_stake());
        if already_verified >= SKIP_CERT_THRESHOLD || observed_fraction < SKIP_CERT_THRESHOLD {
            return;
        }
        self.skip.do_add_to_votes(votes, rank_map);
        self.skip_fallback.do_add_to_votes(votes, rank_map);
    }
}

fn handle_notar_state(
    rank_map: &BLSPubkeyToRankMap,
    votes: &mut HashMap<VotePayloadToSign, Batch>,
    vote_entry: &mut VoteEntry,
) {
    vote_entry.try_add_to_votes(votes, rank_map, NOTAR_CERT_THRESHOLD);
    vote_entry.try_add_to_votes(votes, rank_map, FAST_FINALIZE_CERT_THRESHOLD);
}
