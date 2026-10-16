use {
    crate::{
        bls_sigverifier::BAN_TIMEOUT, bls_vote_sigverify::batch::Batch,
        rewards::rewards_wants_vote, unverified_votes_batch::UnverifiedVotePayload,
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
    agave_votor_transport::endpoint::BanSender,
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
const BATCH_THRESHOLD: usize = 16;
const SAFE_TO_SKIP: Fraction = Fraction::from_percentage(40);
const SAFE_TO_NOTAR_NOTAR_ONLY: Fraction = Fraction::from_percentage(40);
const SAFE_TO_NOTAR_COMBINED_NOTAR_AND_SKIP: Fraction = Fraction::from_percentage(60);
const SAFE_TO_NOTAR_COMBINED_NOTAR_ONLY: Fraction = Fraction::from_percentage(20);

#[derive(Debug)]
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

    pub(crate) fn ban_pubkeys(&mut self, ban_sender: &BanSender, pubkeys_to_ban: HashSet<Pubkey>) {
        if pubkeys_to_ban.is_empty() {
            return;
        }
        for entry in self.entries.values_mut() {
            entry.ban_pubkeys(&pubkeys_to_ban);
        }
        for pubkey in pubkeys_to_ban {
            ban_sender.ban(pubkey, BAN_TIMEOUT);
        }
    }

    pub(crate) fn update_verified(
        &mut self,
        ban_sender: &BanSender,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
    ) {
        let mut total_pubkeys_to_ban = HashSet::new();
        for (vote_payload_to_sign, batch) in votes.drain() {
            let Some(entry) = self.entries.get_mut(&vote_payload_to_sign.slot()) else {
                continue;
            };
            let (verified_stake, pubkeys_to_ban) = batch.verified_stake_and_pubkeys_to_ban();
            entry.update_verified(vote_payload_to_sign, verified_stake);
            total_pubkeys_to_ban.extend(pubkeys_to_ban);
        }
        self.ban_pubkeys(ban_sender, total_pubkeys_to_ban);
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
                    .try_build_finalize_batch(rank_map, votes, unverified_payload);
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
                self.skip
                    .skip
                    .add_unverified_vote(unverified_payload, votes, rank_map);
                let vote = Vote::new_skip_vote(vote_slot);
                if rewards_wants_vote(my_pubkey, leader_schedule, root_bank.slot(), &vote) {
                    self.skip.skip.do_add_to_votes(votes, rank_map);
                }
                self.skip.try_build_batch(votes, rank_map);
                self.check_all_safe_to_notar(rank_map, votes);
                self.handle_safe_to_skip(rank_map, votes);
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
                    .add_unverified_vote(unverified_payload, votes, rank_map);
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
                vote_entry.add_unverified_vote(unverified_payload, votes, rank_map);
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
                let notar_entry = self
                    .notar
                    .entry(block.block_id)
                    .or_insert_with(|| VoteEntry::new(vote_payload_to_sign, self.my_stake));
                notar_entry.add_unverified_vote(unverified_payload, votes, rank_map);
                let vote = Vote::new_notarization_vote(block);
                if rewards_wants_vote(my_pubkey, leader_schedule, root_bank.slot(), &vote) {
                    notar_entry.do_add_to_votes(votes, rank_map);
                }
                Self::handle_safe_to_notar(
                    notar_entry,
                    &mut self.skip.skip,
                    self.my_stake,
                    rank_map,
                    votes,
                );
                self.handle_notar_and_nf_state(block, rank_map, votes);
                self.handle_safe_to_skip(rank_map, votes);
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
                entry.add_unverified_vote(unverified_payload, votes, rank_map);
                self.notar_fallback_votes
                    .entry(rank)
                    .or_default()
                    .push(block.block_id);
                self.handle_notar_and_nf_state(block, rank_map, votes);
            }
        }
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

    fn ban_pubkeys(&mut self, pubkeys_to_ban: &HashSet<Pubkey>) {
        self.skip.skip.ban_pubkeys(pubkeys_to_ban);
        self.skip.skip_fallback.ban_pubkeys(pubkeys_to_ban);
        self.finalize.ban_pubkeys(pubkeys_to_ban);
        for entry in self.genesis.values_mut() {
            entry.ban_pubkeys(pubkeys_to_ban);
        }
        for entry in self.notar.values_mut() {
            entry.ban_pubkeys(pubkeys_to_ban);
        }
        for entry in self.notar_fallback.values_mut() {
            entry.ban_pubkeys(pubkeys_to_ban);
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
                if notar_entry.batch.is_empty() && nf_entry.batch.is_empty() {
                    return;
                }
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
        rank_map: &BLSPubkeyToRankMap,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
    ) {
        for notar_entry in self.notar.values_mut() {
            Self::handle_safe_to_notar(
                notar_entry,
                &mut self.skip.skip,
                self.my_stake,
                rank_map,
                votes,
            );
        }
    }

    fn handle_safe_to_notar(
        notar_entry: &mut VoteEntry,
        skip_entry: &mut VoteEntry,
        my_stake: Saturating<u64>,
        rank_map: &BLSPubkeyToRankMap,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
    ) {
        notar_entry.try_add_to_votes(votes, rank_map, SAFE_TO_NOTAR_NOTAR_ONLY);
        if notar_entry.batch.is_empty() && skip_entry.batch.is_empty() {
            return;
        }

        let current_combined = Fraction::new(
            (notar_entry.observed_stake() + skip_entry.observed_stake() + my_stake).0,
            rank_map.total_stake(),
        );
        let already_verified_combined = notar_entry.already_verified + skip_entry.already_verified;
        let already_verified_combined =
            Fraction::new(already_verified_combined.0, rank_map.total_stake());

        let already_verified_notar =
            Fraction::new(notar_entry.already_verified.0, rank_map.total_stake());
        let current_notar = Fraction::new(
            (notar_entry.observed_stake() + my_stake).0,
            rank_map.total_stake(),
        );

        if (already_verified_combined < SAFE_TO_NOTAR_COMBINED_NOTAR_AND_SKIP
            || already_verified_notar < SAFE_TO_NOTAR_COMBINED_NOTAR_ONLY)
            && current_combined >= SAFE_TO_NOTAR_COMBINED_NOTAR_AND_SKIP
            && current_notar >= SAFE_TO_NOTAR_COMBINED_NOTAR_ONLY
        {
            notar_entry.do_add_to_votes(votes, rank_map);
            skip_entry.do_add_to_votes(votes, rank_map);
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
        let mut has_pending = !self.skip.skip.batch.is_empty();
        for notar in self.notar.values() {
            let current_notar = notar.observed_stake();
            current_sum_notar += current_notar;
            current_max_notar = current_max_notar.max(current_notar);
            already_verified_sum_notar += notar.already_verified;
            already_verified_max_notar = already_verified_max_notar.max(notar.already_verified);
            has_pending |= !notar.batch.is_empty();
        }
        if !has_pending {
            return;
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
            batch_stake: Saturating(0),
        }
    }

    fn add_unverified_vote(
        &mut self,
        unverified_payload: UnverifiedVotePayload,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        rank_map: &BLSPubkeyToRankMap,
    ) {
        assert!(self.ranks.insert(unverified_payload.rank));
        self.batch_stake += unverified_payload.stake.get();
        self.batch.push(unverified_payload);
        if self.batch.len() >= BATCH_THRESHOLD {
            self.do_add_to_votes(votes, rank_map);
        }
    }

    fn try_add_to_votes(
        &mut self,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        rank_map: &BLSPubkeyToRankMap,
        threshold: Fraction,
    ) {
        if self.batch.is_empty() {
            return;
        }
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
            batch_stake,
            vote_payload_to_sign,
        } = self;
        *inflight_stake += *batch_stake;
        *batch_stake = Saturating(0);
        match votes.entry(*vote_payload_to_sign) {
            Entry::Vacant(e) => {
                let payloads = std::mem::take(batch);
                let batch = Batch::new(*vote_payload_to_sign, payloads, rank_map.len());
                e.insert(batch);
            }
            Entry::Occupied(mut e) => {
                e.get_mut().push(batch.drain(..));
            }
        }
    }

    fn try_build_finalize_batch(
        &mut self,
        rank_map: &BLSPubkeyToRankMap,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        unverified_payload: UnverifiedVotePayload,
    ) {
        self.add_unverified_vote(unverified_payload, votes, rank_map);
        self.try_add_to_votes(votes, rank_map, FINALIZE_CERT_THRESHOLD);
    }

    fn update_verified(&mut self, verified: u64) {
        self.inflight_stake = Saturating(0);
        self.already_verified += verified;
    }

    fn ban_pubkeys(&mut self, pubkeys_to_ban: &HashSet<Pubkey>) {
        if pubkeys_to_ban.is_empty() {
            return;
        }
        self.batch.retain(|payload| {
            if pubkeys_to_ban.contains(&payload.sender_identity_pubkey) {
                self.batch_stake -= payload.stake.get();
                false
            } else {
                true
            }
        });
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
        if self.skip.batch.is_empty() && self.skip_fallback.batch.is_empty() {
            return;
        }
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

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{bls_sigverifier::SigVerifierChannels, bls_vote_sigverify::verify_and_send_votes},
        agave_votor_messages::unverified_vote_message::UnverifiedVoteMessage,
        agave_votor_transport::endpoint::stub_ban_channel_for_tests,
        crossbeam_channel::unbounded,
        rayon::ThreadPoolBuilder,
        solana_runtime::genesis_utils::{
            ValidatorVoteKeypairs, create_genesis_config_with_vote_accounts,
        },
        solana_signer::Signer,
        solana_streamer::evicting_sender::EvictingSender,
        std::sync::Arc,
    };

    struct TestContext {
        shred_version: u16,
        bank: Bank,
        leader_schedule: LeaderScheduleCache,
        validators: Vec<ValidatorVoteKeypairs>,
        rank_map: Arc<BLSPubkeyToRankMap>,
        ranks: Vec<u16>,
        my_pubkey: Pubkey,
        my_stake: Saturating<u64>,
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
            let my_stake = Saturating(my_rank.map_or(0, |(_, entry)| entry.stake.get()));
            let ranks = (0..rank_map.len() as u16)
                .filter(|rank| my_rank.is_none_or(|(my_rank, _)| my_rank != *rank))
                .collect();
            assert_eq!(
                rank_map.total_stake().get(),
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
            let entry = self.rank_map.get_pubkey_stake_entry(rank as usize).unwrap();
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
        }

        fn entry(&self, vote: Vote) -> &VoteEntry {
            let entry = self.pool.entries.get(&vote.slot()).unwrap();
            match vote {
                Vote::Skip(_) => &entry.skip.skip,
                Vote::SkipFallback(_) => &entry.skip.skip_fallback,
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
            stats.votes_to_sig_verify.0
        }
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
                assert_eq!(ctx.entry(vote).batch_stake.0, 0);
                assert_eq!(ctx.entry(vote).inflight_stake.0, 160);
                assert_eq!(ctx.verify(), 16);
                assert_eq!(ctx.entry(vote).inflight_stake.0, 0);
                assert_eq!(ctx.entry(vote).already_verified.0, (start + 16) as u64 * 10);
            }
            ctx.add(vote, 32, true);
            ctx.assert_queued(&[]);
            assert_eq!(ctx.entry(vote).batch.len(), 1);
            assert_eq!(ctx.entry(vote).batch_stake.0, 10);
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
        assert_eq!(ctx.entry(vote).batch_stake.0, 0);
        assert_eq!(ctx.entry(vote).inflight_stake.0, 320);
        assert_eq!(ctx.verify(), 32);
        assert_eq!(ctx.entry(vote).already_verified.0, 320);
        assert_eq!(ctx.entry(vote).inflight_stake.0, 0);
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
        assert_eq!(entry.batch_stake.0, 50);

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
        assert_eq!(entry.batch_stake.0, 20);
        assert_eq!(entry.observed_stake().0, 20);

        entry.ban_pubkeys(&identities.into_iter().collect());
        assert!(entry.batch.is_empty());
        assert_eq!(entry.batch_stake.0, 0);
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
            assert_eq!(
                ctx.entry(vote).already_verified.0,
                required_votes as u64 * 10
            );
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
            assert_eq!(ctx.entry(vote).already_verified.0, end as u64 * 10);
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
        assert_eq!(ctx.entry(skip).already_verified.0, 40);
        assert_eq!(ctx.entry(fallback).already_verified.0, 20);
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
            assert_eq!(ctx.entry(vote).already_verified.0, valid_votes as u64 * 10);
            assert_eq!(ctx.entry(vote).inflight_stake.0, 0);
            assert_eq!(ctx.entry(vote).observed_stake().0, valid_votes as u64 * 10);
            for voter in 6..9 {
                ctx.add(vote, voter, true);
                if valid_votes == 3 && voter == 8 {
                    ctx.assert_queued(&[vote]);
                    assert_eq!(ctx.verify(), 3);
                    assert_eq!(ctx.entry(vote).already_verified.0, 60);
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
        assert_eq!(ctx.entry(vote).already_verified.0, 0);
        // Leave the event batch unverified. Later arrivals append to it and must
        // include its stake when checking the notar and fast-finalize thresholds.
        for voter in 4..8 {
            ctx.add(vote, voter, true);
            assert_eq!(ctx.entry(vote).inflight_stake.0, (voter as u64 + 1) * 10);
            assert_eq!(ctx.entry(vote).batch_stake.0, 0);
        }
        ctx.assert_queued(&[vote]);
        assert_eq!(ctx.verify(), 8);
        assert_eq!(ctx.entry(vote).inflight_stake.0, 0);
        assert_eq!(ctx.entry(vote).already_verified.0, 80);
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
            assert_eq!(ctx.entry(reward_vote).already_verified.0, 10);
            for voter in 1..4 {
                ctx.add(fallback, voter, true);
                ctx.assert_queued(&[]);
            }
            // 10% own stake + 10% verified for rewards + 40% fallback = 60%.
            ctx.add(fallback, 4, true);
            ctx.assert_queued(&[fallback]);
            assert_eq!(ctx.verify(), 4);
            assert_eq!(ctx.entry(reward_vote).already_verified.0, 10);
            assert_eq!(ctx.entry(fallback).already_verified.0, 40);
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
            assert_eq!(ctx.entry(reward_vote).already_verified.0, 0);
            assert_eq!(ctx.entry(reward_vote).inflight_stake.0, 10);
            for voter in 1..4 {
                ctx.add(fallback, voter, true);
                ctx.assert_queued(&[reward_vote]);
            }
            ctx.add(fallback, 4, true);
            ctx.assert_queued(&[reward_vote, fallback]);
            assert_eq!(ctx.verify(), 5);
            assert_eq!(ctx.entry(reward_vote).already_verified.0, 10);
            assert_eq!(ctx.entry(fallback).already_verified.0, 40);
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
            assert_eq!(ctx.entry(reward_vote).already_verified.0, 0);
            assert_eq!(ctx.entry(reward_vote).inflight_stake.0, 0);
            for voter in 1..5 {
                ctx.add(fallback, voter, true);
                ctx.assert_queued(&[]);
            }
            ctx.add(fallback, 5, true);
            ctx.assert_queued(&[fallback]);
            assert_eq!(ctx.verify(), 5);
            assert_eq!(ctx.entry(fallback).already_verified.0, 50);
        }
    }

    #[test]
    fn test_combined_certificates_queue_both_vote_types() {
        let slot = 1;
        let block = Block::new_unique(slot);
        for (vote, fallback) in [
            (
                Vote::new_skip_vote(slot),
                Vote::new_skip_fallback_vote(slot),
            ),
            (
                Vote::new_notarization_vote(block),
                Vote::new_notarization_fallback_vote(block),
            ),
        ] {
            let mut ctx = TestContext::new(false, slot);
            for voter in 0..3 {
                ctx.add(vote, voter, true);
                ctx.assert_queued(&[]);
            }
            for voter in 3..5 {
                ctx.add(fallback, voter, true);
                ctx.assert_queued(&[]);
            }
            ctx.add(fallback, 5, true);
            ctx.assert_queued(&[vote, fallback]);
            assert_eq!(ctx.verify(), 6);
            assert_eq!(ctx.entry(vote).already_verified.0, 30);
            assert_eq!(ctx.entry(fallback).already_verified.0, 30);
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
            assert_eq!(ctx.entry(notar).already_verified.0, 40);
            assert_eq!(ctx.entry(skip).already_verified.0, 20);
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
            assert_eq!(ctx.entry(largest).already_verified.0, 40);
            assert_eq!(ctx.entry(other).already_verified.0, 30);
            assert_eq!(ctx.entry(skip).already_verified.0, 10);
        }
    }
}
