use {
    crate::{
        bls_sigverifier::BAN_TIMEOUT,
        bls_vote_sigverify::batch::Batch,
        rewards::rewards_wants_vote,
        unverified_votes_batch::{Rank, UnverifiedVotePayload},
    },
    agave_votor_messages::{
        certificate::{
            FAST_FINALIZE_CERT_THRESHOLD, FINALIZE_CERT_THRESHOLD, NOTAR_CERT_THRESHOLD,
            NOTAR_FALLBACK_CERT_THRESHOLD, SKIP_CERT_THRESHOLD,
        },
        consensus_message::BlockId,
        fraction::Fraction,
        migration::GENESIS_VOTE_THRESHOLD,
        reward_certificate::NUM_SLOTS_FOR_REWARD,
        stake::{NonZeroStake, Stake},
        vote::Vote,
        wire::VotePayloadToSign,
    },
    agave_votor_transport::endpoint::BanSender,
    solana_clock::Slot,
    solana_ledger::leader_schedule_cache::LeaderScheduleCache,
    solana_pubkey::Pubkey,
    solana_runtime::{bank::Bank, epoch_stakes::BLSPubkeyToRankMap},
    std::collections::{BTreeSet, HashMap, HashSet, hash_map::Entry},
};

#[cfg(test)]
mod tests;

const MAX_NOTAR_FALLBACK_ENTRIES: usize = 3;
const BATCH_THRESHOLD: usize = 16;
const SAFE_TO_SKIP: Fraction = Fraction::from_percentage(40);
const SAFE_TO_NOTAR_NOTAR_ONLY: Fraction = Fraction::from_percentage(40);
const SAFE_TO_NOTAR_COMBINED_PERCENT: u64 = 60;
const SAFE_TO_NOTAR_COMBINED_NOTAR_PERCENT: u64 = 20;
const SAFE_TO_NOTAR_COMBINED_NOTAR_AND_SKIP: Fraction =
    Fraction::from_percentage(SAFE_TO_NOTAR_COMBINED_PERCENT);
const SAFE_TO_NOTAR_COMBINED_NOTAR_ONLY: Fraction =
    Fraction::from_percentage(SAFE_TO_NOTAR_COMBINED_NOTAR_PERCENT);

#[derive(Debug)]
pub(crate) enum VotePoolError {
    InvalidVote(Pubkey),
    DuplicateVote,
}

/// A simple index to look up which senders have votes waiting to be verified.
#[derive(Default)]
struct PendingVoteIndex {
    entries: HashMap<Pubkey, HashSet<VotePayloadToSign>>,
}

impl PendingVoteIndex {
    fn insert(&mut self, sender: Pubkey, vote: VotePayloadToSign) {
        self.entries.entry(sender).or_default().insert(vote);
    }

    fn remove(&mut self, sender: &Pubkey, vote: &VotePayloadToSign) {
        if let Entry::Occupied(mut entry) = self.entries.entry(*sender) {
            entry.get_mut().remove(vote);
            if entry.get().is_empty() {
                entry.remove();
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct VotePool {
    entries: HashMap<Slot, SlotEntry>,
    pending_votes: PendingVoteIndex,
}

impl VotePool {
    pub(crate) fn add_vote(
        &mut self,
        my_pubkey: &Pubkey,
        root_bank: &Bank,
        leader_schedule: &LeaderScheduleCache,
        rank_map: &BLSPubkeyToRankMap,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        my_stake: Stake,
        unverified_payload: UnverifiedVotePayload,
    ) -> Result<(), VotePoolError> {
        let vote_msg = &unverified_payload.vote_message;
        let vote = &vote_msg.vote;
        match self.entries.entry(vote.slot()) {
            Entry::Occupied(mut e) => e.get_mut().add_vote(
                my_pubkey,
                root_bank,
                leader_schedule,
                votes,
                &mut self.pending_votes,
                unverified_payload,
            ),
            Entry::Vacant(e) => {
                let total_stake = rank_map.total_stake();
                let mut slot_entry = SlotEntry::new(
                    vote.slot(),
                    vote_msg.shred_version,
                    my_stake,
                    total_stake,
                    rank_map.len(),
                );
                let res = slot_entry.add_vote(
                    my_pubkey,
                    root_bank,
                    leader_schedule,
                    votes,
                    &mut self.pending_votes,
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
        let mut affected_votes = HashSet::new();
        for pubkey in &pubkeys_to_ban {
            if let Some(votes) = self.pending_votes.entries.remove(pubkey) {
                affected_votes.extend(votes);
            }
        }
        for vote in affected_votes {
            if let Some(entry) = self.entries.get_mut(&vote.slot()) {
                entry.ban_vote(vote, &pubkeys_to_ban);
            }
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
        self.entries.retain(|slot, entry| {
            if *slot >= slot_to_keep {
                return true;
            }
            for vote_entry in entry.vote_entries() {
                for payload in &vote_entry.batch {
                    self.pending_votes.remove(
                        &payload.sender_identity_pubkey,
                        &vote_entry.vote_payload_to_sign,
                    );
                }
            }
            false
        });
    }
}

/// Computes percent of total stake
fn percent(total_stake: NonZeroStake, percent: u64) -> Stake {
    let total_stake = u128::from(total_stake.get().get());
    let percent = u128::from(percent);
    let mult = total_stake.saturating_mul(percent);
    let res = mult.div_ceil(100);
    Stake(u64::try_from(res).expect("should fit in 64bits"))
}

struct SlotEntry {
    max_validators: usize,
    total_stake: NonZeroStake,
    my_stake: Stake,
    skip: VoteEntry,
    skip_fallback: VoteEntry,
    finalize: VoteEntry,
    genesis: HashMap<BlockId, VoteEntry>,
    genesis_votes: HashSet<Rank>,
    notar: HashMap<BlockId, VoteEntry>,
    notar_votes: HashSet<Rank>,
    notar_fallback: HashMap<BlockId, VoteEntry>,
    notar_fallback_votes: HashMap<Rank, Vec<BlockId>>,
    notar_stake_state: NotarStakeState,
}

impl SlotEntry {
    fn new(
        slot: Slot,
        shred_version: u16,
        my_stake: Stake,
        total_stake: NonZeroStake,
        max_validators: usize,
    ) -> Self {
        let finalize_vote_payload_to_sign =
            VotePayloadToSign::new_from_vote(Vote::new_finalization_vote(slot), shred_version);
        let skip_vote_payload_to_sign =
            VotePayloadToSign::new_from_vote(Vote::new_skip_vote(slot), shred_version);
        let sf_vote_payload_to_sign =
            VotePayloadToSign::new_from_vote(Vote::new_skip_fallback_vote(slot), shred_version);
        Self {
            max_validators,
            total_stake,
            my_stake,
            skip: VoteEntry::new(
                skip_vote_payload_to_sign,
                my_stake,
                total_stake,
                max_validators,
            ),
            skip_fallback: VoteEntry::new(
                sf_vote_payload_to_sign,
                my_stake,
                total_stake,
                max_validators,
            ),
            finalize: VoteEntry::new(
                finalize_vote_payload_to_sign,
                my_stake,
                total_stake,
                max_validators,
            ),
            genesis: HashMap::new(),
            genesis_votes: HashSet::new(),
            notar: HashMap::new(),
            notar_votes: HashSet::new(),
            notar_fallback: HashMap::new(),
            notar_fallback_votes: HashMap::new(),
            notar_stake_state: NotarStakeState::new(total_stake),
        }
    }

    fn add_vote(
        &mut self,
        my_pubkey: &Pubkey,
        root_bank: &Bank,
        leader_schedule: &LeaderScheduleCache,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        pending_votes: &mut PendingVoteIndex,
        unverified_payload: UnverifiedVotePayload,
    ) -> Result<(), VotePoolError> {
        let rank = unverified_payload.rank;
        let vote_slot = unverified_payload.vote_message.vote.slot();
        let shred_version = unverified_payload.vote_message.shred_version;

        match &unverified_payload.vote_message.vote {
            Vote::Genesis(genesis) => {
                if self.skip.ranks.contains(&rank)
                    || self.skip_fallback.ranks.contains(&rank)
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
                    .or_insert_with(|| {
                        VoteEntry::new(
                            vote_payload_to_sign,
                            self.my_stake,
                            self.total_stake,
                            self.max_validators,
                        )
                    });
                vote_entry.add_unverified_vote(unverified_payload, votes, pending_votes);
                vote_entry.try_add_to_votes(votes, pending_votes, GENESIS_VOTE_THRESHOLD);
            }

            Vote::Finalize(_) => {
                if self.skip.ranks.contains(&rank)
                    || self.skip_fallback.ranks.contains(&rank)
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
                    .add_unverified_vote(unverified_payload, votes, pending_votes);
                self.finalize
                    .try_add_to_votes(votes, pending_votes, FINALIZE_CERT_THRESHOLD);
            }

            Vote::SkipFallback(_) => {
                if self.finalize.ranks.contains(&rank)
                    || self.skip.ranks.contains(&rank)
                    || self.genesis_votes.contains(&rank)
                {
                    return Err(VotePoolError::InvalidVote(
                        unverified_payload.sender_identity_pubkey,
                    ));
                }
                if self.skip_fallback.ranks.contains(&rank) {
                    return Err(VotePoolError::DuplicateVote);
                }
                self.skip_fallback
                    .add_unverified_vote(unverified_payload, votes, pending_votes);
                self.try_build_skip_batch(votes, pending_votes);
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
                let nf_entry = self
                    .notar_fallback
                    .entry(block.block_id)
                    .or_insert_with(|| {
                        VoteEntry::new(
                            vote_payload_to_sign,
                            self.my_stake,
                            self.total_stake,
                            self.max_validators,
                        )
                    });
                nf_entry.add_unverified_vote(unverified_payload, votes, pending_votes);
                self.notar_fallback_votes
                    .entry(rank)
                    .or_default()
                    .push(block.block_id);
                let mut notar_entry = self.notar.get_mut(&block.block_id);
                Self::handle_notar_and_nf_state(
                    self.my_stake,
                    self.total_stake,
                    notar_entry.as_deref_mut(),
                    Some(nf_entry),
                    votes,
                    pending_votes,
                );
                if let Some(notar_entry) = notar_entry {
                    self.notar_stake_state.add_unverified_nf_vote(
                        block.block_id,
                        notar_entry.observed_stake(),
                        notar_entry.batch.is_empty(),
                    );
                }
            }

            Vote::Skip(_) => {
                if self.notar_votes.contains(&rank)
                    || self.finalize.ranks.contains(&rank)
                    || self.skip_fallback.ranks.contains(&rank)
                    || self.genesis_votes.contains(&rank)
                {
                    return Err(VotePoolError::InvalidVote(
                        unverified_payload.sender_identity_pubkey,
                    ));
                }
                if self.skip.ranks.contains(&rank) {
                    return Err(VotePoolError::DuplicateVote);
                }

                self.skip
                    .add_unverified_vote(unverified_payload, votes, pending_votes);
                let vote = Vote::new_skip_vote(vote_slot);
                if rewards_wants_vote(my_pubkey, leader_schedule, root_bank.slot(), &vote) {
                    self.skip.do_add_to_votes(votes, pending_votes);
                }
                self.try_build_skip_batch(votes, pending_votes);
                self.check_safe_to_notar_after_skip_vote(votes, pending_votes);
                self.handle_safe_to_skip(votes, pending_votes);
            }

            Vote::Notarize(notar) => {
                if self.skip.ranks.contains(&rank) || self.genesis_votes.contains(&rank) {
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
                let notar_entry = self.notar.entry(block.block_id).or_insert_with(|| {
                    VoteEntry::new(
                        vote_payload_to_sign,
                        self.my_stake,
                        self.total_stake,
                        self.max_validators,
                    )
                });

                let prev_observed_notar_stake = notar_entry.observed_stake();
                let additional_stake = unverified_payload.stake;

                notar_entry.add_unverified_vote(unverified_payload, votes, pending_votes);
                let vote = Vote::new_notarization_vote(block);
                if rewards_wants_vote(my_pubkey, leader_schedule, root_bank.slot(), &vote) {
                    notar_entry.do_add_to_votes(votes, pending_votes);
                }
                Self::check_safe_to_notar_after_notar_vote(
                    notar_entry,
                    &mut self.skip,
                    self.my_stake,
                    self.total_stake,
                    votes,
                    pending_votes,
                );
                Self::handle_notar_and_nf_state(
                    self.my_stake,
                    self.total_stake,
                    Some(notar_entry),
                    self.notar_fallback.get_mut(&block.block_id),
                    votes,
                    pending_votes,
                );
                self.notar_stake_state.add_unverified_notar_vote(
                    block.block_id,
                    prev_observed_notar_stake,
                    additional_stake,
                    notar_entry.batch.is_empty(),
                );
                self.handle_safe_to_skip(votes, pending_votes);
            }
        }
        Ok(())
    }

    fn update_verified(&mut self, vote_payload_to_sign: VotePayloadToSign, verified_stake: Stake) {
        match vote_payload_to_sign {
            VotePayloadToSign::Notar {
                block,
                shred_version: _,
            } => {
                if let Some(notar_entry) = self.notar.get_mut(&block.block_id) {
                    let prev_verified_stake = notar_entry.verified_stake;
                    let prev_observed_stake = notar_entry.observed_stake();
                    notar_entry.update_verified(verified_stake);
                    let observed_stake = notar_entry.observed_stake();
                    let stake_not_verified = prev_observed_stake.sub(observed_stake);
                    self.notar_stake_state.notar_stake_verified(
                        block.block_id,
                        prev_observed_stake,
                        prev_verified_stake,
                        verified_stake,
                        stake_not_verified,
                        self.skip.verified_stake,
                    );
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
                self.skip.update_verified(verified_stake);
                self.notar_stake_state
                    .skip_stake_verified(self.skip.verified_stake, &self.notar);
            }
            VotePayloadToSign::SkipFallback { .. } => {
                self.skip_fallback.update_verified(verified_stake);
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

    fn vote_entries(&self) -> impl Iterator<Item = &VoteEntry> {
        [&self.skip, &self.skip_fallback, &self.finalize]
            .into_iter()
            .chain(self.genesis.values())
            .chain(self.notar.values())
            .chain(self.notar_fallback.values())
    }

    fn ban_vote(&mut self, vote: VotePayloadToSign, pubkeys_to_ban: &HashSet<Pubkey>) {
        let entry = match vote {
            VotePayloadToSign::Notar { block, .. } => {
                if let Some(notar_entry) = self.notar.get_mut(&block.block_id) {
                    let prev_observed_stake = notar_entry.observed_stake();
                    let stake_removed = notar_entry.ban_pubkeys(pubkeys_to_ban);
                    self.notar_stake_state.validator_banned(
                        block.block_id,
                        prev_observed_stake,
                        stake_removed,
                        notar_entry.batch.is_empty(),
                    );
                }
                None
            }
            VotePayloadToSign::Skip { .. } => Some(&mut self.skip),
            VotePayloadToSign::SkipFallback { .. } => Some(&mut self.skip_fallback),
            VotePayloadToSign::Finalize { .. } => Some(&mut self.finalize),
            VotePayloadToSign::Genesis { block, .. } => self.genesis.get_mut(&block.block_id),
            VotePayloadToSign::NotarFallback { block, .. } => {
                self.notar_fallback.get_mut(&block.block_id)
            }
        };
        if let Some(entry) = entry {
            entry.ban_pubkeys(pubkeys_to_ban);
        }
    }

    fn handle_notar_and_nf_state(
        my_stake: Stake,
        total_stake: NonZeroStake,
        notar_entry: Option<&mut VoteEntry>,
        nf_entry: Option<&mut VoteEntry>,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        pending_votes: &mut PendingVoteIndex,
    ) {
        match (notar_entry, nf_entry) {
            (None, None) => (),
            (Some(notar_entry), None) => {
                handle_notar_state(votes, pending_votes, notar_entry);
            }
            (None, Some(nf_entry)) => {
                nf_entry.try_add_to_votes(votes, pending_votes, NOTAR_FALLBACK_CERT_THRESHOLD);
            }

            (Some(notar_entry), Some(nf_entry)) => {
                handle_notar_state(votes, pending_votes, notar_entry);
                let observed_stake = total_stake.fraction(
                    notar_entry
                        .observed_stake()
                        .add(nf_entry.observed_stake())
                        .add(my_stake),
                );
                let verified_stake = notar_entry.verified_stake.add(nf_entry.verified_stake);
                let verified_stake = total_stake.fraction(verified_stake);
                if verified_stake < NOTAR_FALLBACK_CERT_THRESHOLD
                    && observed_stake >= NOTAR_FALLBACK_CERT_THRESHOLD
                {
                    notar_entry.do_add_to_votes(votes, pending_votes);
                    nf_entry.do_add_to_votes(votes, pending_votes);
                }
            }
        }
    }

    fn try_build_skip_batch(
        &mut self,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        pending_votes: &mut PendingVoteIndex,
    ) {
        let verified = self
            .skip
            .verified_stake
            .add(self.skip_fallback.verified_stake);
        let verified = self.total_stake.fraction(verified);
        let observed = self.total_stake.fraction(
            self.my_stake
                .add(self.skip.observed_stake())
                .add(self.skip_fallback.observed_stake()),
        );
        if verified < SKIP_CERT_THRESHOLD && observed >= SKIP_CERT_THRESHOLD {
            self.skip.do_add_to_votes(votes, pending_votes);
            self.skip_fallback.do_add_to_votes(votes, pending_votes);
        }
    }

    /// Checks if safe-to-notar condition is met after receiving a skip vote.
    /// Do not need to check just the notar threshold, just the combined skip+notar threshold.
    fn check_safe_to_notar_after_skip_vote(
        &mut self,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        pending_votes: &mut PendingVoteIndex,
    ) {
        // Minimum notar stake required is max(20%, 60% - skip stake).
        // Including my_stake in the calculation as we do not see votes from self.
        let min_notar_stake = percent(self.total_stake, SAFE_TO_NOTAR_COMBINED_NOTAR_PERCENT)
            .sub(self.my_stake)
            .max(
                percent(self.total_stake, SAFE_TO_NOTAR_COMBINED_PERCENT)
                    .sub(self.skip.observed_stake())
                    .sub(self.my_stake),
            );

        // Checks if skip votes should be verified.
        if let Some(notar_stake) = self
            .notar_stake_state
            .highest_safe_to_notar_candidate_stake()
            && notar_stake >= min_notar_stake
        {
            self.skip.do_add_to_votes(votes, pending_votes);
        }

        // Checks if any of the notar blocks are ready to be verified.
        // To meet the criteria below, blocks must have at least 20% stake so we have to check at most 5 blocks.
        let mut flushed = Vec::new();
        for &(stake, block_id) in self.notar_stake_state.pending_entries.iter().rev() {
            if stake < min_notar_stake {
                break;
            }
            let notar_entry = self
                .notar
                .get_mut(&block_id)
                .expect("pending entries should be present");
            // minimum notar stake requirement was met.  Do not add if the verified stake already met the skip+notar threshold.
            if !safe_to_notar_is_combined_threshold_met(
                notar_entry.verified_stake,
                self.skip.verified_stake,
                self.total_stake,
            ) {
                notar_entry.do_add_to_votes(votes, pending_votes);
                flushed.push((stake, block_id));
            }
        }
        for key in flushed {
            self.notar_stake_state.pending_entries.remove(&key);
        }
    }

    /// Checks if the safe-to-notar condition is met after receiving a notar vote.
    /// Need to check both just the notar threshold and the combined skip+notar theshold.
    fn check_safe_to_notar_after_notar_vote(
        notar_entry: &mut VoteEntry,
        skip_entry: &mut VoteEntry,
        my_stake: Stake,
        total_stake: NonZeroStake,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        pending_votes: &mut PendingVoteIndex,
    ) {
        // Checks just the notar threshold.
        notar_entry.try_add_to_votes(votes, pending_votes, SAFE_TO_NOTAR_NOTAR_ONLY);

        // Checks the notar + skip threshold.  Handles cases where notar votes arrive after skip.
        let current_combined = total_stake.fraction(
            notar_entry
                .observed_stake()
                .add(skip_entry.observed_stake())
                .add(my_stake),
        );
        let verified_combined = notar_entry.verified_stake.add(skip_entry.verified_stake);
        let verified_combined = total_stake.fraction(verified_combined);

        let verified_notar = total_stake.fraction(notar_entry.verified_stake);
        let current_notar = total_stake.fraction(notar_entry.observed_stake().add(my_stake));

        if (verified_combined < SAFE_TO_NOTAR_COMBINED_NOTAR_AND_SKIP
            || verified_notar < SAFE_TO_NOTAR_COMBINED_NOTAR_ONLY)
            && current_combined >= SAFE_TO_NOTAR_COMBINED_NOTAR_AND_SKIP
            && current_notar >= SAFE_TO_NOTAR_COMBINED_NOTAR_ONLY
        {
            notar_entry.do_add_to_votes(votes, pending_votes);
            skip_entry.do_add_to_votes(votes, pending_votes);
        }
    }

    fn handle_safe_to_skip(
        &mut self,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        pending_votes: &mut PendingVoteIndex,
    ) {
        let verified_max_notar = self
            .notar_stake_state
            .highest_verified_stake()
            .unwrap_or(Stake(0));
        let current_max_notar = self
            .notar_stake_state
            .highest_observed_stake()
            .unwrap_or(Stake(0));
        let verified = self
            .skip
            .verified_stake
            .add(self.notar_stake_state.total_notar_stake_verified)
            .sub(verified_max_notar);
        let current = self
            .skip
            .observed_stake()
            .add(self.my_stake)
            .add(self.notar_stake_state.total_notar_stake_observed)
            .sub(current_max_notar.max(self.my_stake));

        let verified = self.total_stake.fraction(verified);
        let current = self.total_stake.fraction(current);
        if verified >= SAFE_TO_SKIP || current < SAFE_TO_SKIP {
            return;
        }
        self.skip.do_add_to_votes(votes, pending_votes);
        while let Some((_, block_id)) = self.notar_stake_state.pending_entries.pop_first() {
            self.notar
                .get_mut(&block_id)
                .expect("pending entries should be present")
                .do_add_to_votes(votes, pending_votes);
        }
    }
}

struct VoteEntry {
    max_validators: usize,
    total_stake: NonZeroStake,
    vote_payload_to_sign: VotePayloadToSign,
    /// Stake of the node
    my_stake: Stake,
    /// Stake that has already been verified.
    verified_stake: Stake,
    /// Stake that is in in the process of being verified.
    inflight_stake: Stake,
    /// validatores included in the current batch.
    ranks: HashSet<Rank>,
    /// the current batch of votes waiting to be verified
    batch: Vec<UnverifiedVotePayload>,
    /// stake of the current batch.
    batch_stake: Stake,
}

impl VoteEntry {
    fn new(
        vote_payload_to_sign: VotePayloadToSign,
        my_stake: Stake,
        total_stake: NonZeroStake,
        max_validators: usize,
    ) -> Self {
        Self {
            max_validators,
            total_stake,
            vote_payload_to_sign,
            my_stake,
            verified_stake: Stake(0),
            inflight_stake: Stake(0),
            ranks: HashSet::new(),
            batch: Vec::new(),
            batch_stake: Stake(0),
        }
    }

    fn add_unverified_vote(
        &mut self,
        unverified_payload: UnverifiedVotePayload,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        pending_votes: &mut PendingVoteIndex,
    ) {
        assert!(self.ranks.insert(unverified_payload.rank));
        pending_votes.insert(
            unverified_payload.sender_identity_pubkey,
            self.vote_payload_to_sign,
        );
        self.batch_stake
            .increment(Stake::from(unverified_payload.stake));
        self.batch.push(unverified_payload);
        if self.batch.len() >= BATCH_THRESHOLD {
            self.do_add_to_votes(votes, pending_votes);
        }
    }

    fn try_add_to_votes(
        &mut self,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        pending_votes: &mut PendingVoteIndex,
        threshold: Fraction,
    ) {
        if self.batch.is_empty() {
            return;
        }
        let verified = self.total_stake.fraction(self.verified_stake);
        let observed = self
            .total_stake
            .fraction(self.observed_stake().add(self.my_stake));
        if verified >= threshold || observed < threshold {
            return;
        }
        self.do_add_to_votes(votes, pending_votes);
    }

    fn do_add_to_votes(
        &mut self,
        votes: &mut HashMap<VotePayloadToSign, Batch>,
        pending_votes: &mut PendingVoteIndex,
    ) {
        if self.batch.is_empty() {
            return;
        }
        self.inflight_stake.increment(self.batch_stake);
        self.batch_stake = Stake(0);
        for payload in &self.batch {
            pending_votes.remove(&payload.sender_identity_pubkey, &self.vote_payload_to_sign);
        }
        match votes.entry(self.vote_payload_to_sign) {
            Entry::Vacant(e) => {
                let payloads = std::mem::take(&mut self.batch);
                let batch = Batch::new(self.vote_payload_to_sign, payloads, self.max_validators);
                e.insert(batch);
            }
            Entry::Occupied(mut e) => {
                e.get_mut().push(self.batch.drain(..));
            }
        }
    }

    fn update_verified(&mut self, verified: Stake) {
        self.inflight_stake = Stake(0);
        self.verified_stake.increment(verified);
    }

    fn ban_pubkeys(&mut self, pubkeys_to_ban: &HashSet<Pubkey>) -> Stake {
        let mut stake_removed = Stake(0);
        self.batch.retain(|payload| {
            if pubkeys_to_ban.contains(&payload.sender_identity_pubkey) {
                self.batch_stake.decrement(Stake::from(payload.stake));
                stake_removed.increment(Stake::from(payload.stake));
                false
            } else {
                true
            }
        });
        stake_removed
    }

    fn observed_stake(&self) -> Stake {
        self.verified_stake
            .add(self.inflight_stake)
            .add(self.batch_stake)
    }
}

struct NotarStakeState {
    total_stake: NonZeroStake,
    total_notar_stake_verified: Stake,
    total_notar_stake_observed: Stake,
    /// Blocks sorted by how much stake for them has been verified.
    stakes_verified: BTreeSet<(Stake, BlockId)>,
    /// Blocks sorted by how much stake for them has been observed.
    stakes_observed: BTreeSet<(Stake, BlockId)>,
    /// Blocks that have at least one `Vote` waiting to be verified sorted by their observed stake.
    pending_entries: BTreeSet<(Stake, BlockId)>,
    /// Blocks that have not skip + notar threshold of safe-to-notar, sorted by observed stake.
    /// Can also contain candidates that do not have any `Vote`s waiting to be verified.
    safe_to_notar_candidates: BTreeSet<(Stake, BlockId)>,
}

impl NotarStakeState {
    fn new(total_stake: NonZeroStake) -> Self {
        Self {
            total_stake,
            total_notar_stake_verified: Stake(0),
            total_notar_stake_observed: Stake(0),
            stakes_verified: BTreeSet::new(),
            stakes_observed: BTreeSet::new(),
            pending_entries: BTreeSet::new(),
            safe_to_notar_candidates: BTreeSet::new(),
        }
    }

    /// Some skip stake was verified. Retire completed combined notar candidates.
    fn skip_stake_verified(
        &mut self,
        cur_verified_skip_stake: Stake,
        notar_entries: &HashMap<BlockId, VoteEntry>,
    ) {
        // Only blocks with at least 20% verified notar stake can complete,
        // so at most five blocks need to be checked.
        for &(notar_stake, block_id) in self.stakes_verified.iter().rev() {
            if !safe_to_notar_is_combined_threshold_met(
                notar_stake,
                cur_verified_skip_stake,
                self.total_stake,
            ) {
                break;
            }
            self.safe_to_notar_candidates
                .remove(&(notar_entries[&block_id].observed_stake(), block_id));
        }
    }

    /// Some notar stake was verified.
    fn notar_stake_verified(
        &mut self,
        block_id: BlockId,
        prev_observed_notar_stake: Stake,
        prev_verified_notar_stake: Stake,
        additional_notar_stake_verified: Stake,
        notar_stake_not_verified: Stake,
        cur_verified_skip_stake: Stake,
    ) {
        let cur_verified_stake = prev_verified_notar_stake.add(additional_notar_stake_verified);
        let cur_observed_stake = prev_observed_notar_stake.sub(notar_stake_not_verified);
        self.total_notar_stake_verified
            .increment(additional_notar_stake_verified);
        self.total_notar_stake_observed
            .decrement(notar_stake_not_verified);
        self.stakes_verified
            .remove(&(prev_verified_notar_stake, block_id));
        self.stakes_verified.insert((cur_verified_stake, block_id));
        if notar_stake_not_verified.0 != 0 {
            self.stakes_observed
                .remove(&(prev_observed_notar_stake, block_id));
            self.stakes_observed.insert((cur_observed_stake, block_id));
            if self
                .pending_entries
                .remove(&(prev_observed_notar_stake, block_id))
            {
                self.pending_entries.insert((cur_observed_stake, block_id));
            }
        }
        self.safe_to_notar_candidates
            .remove(&(prev_observed_notar_stake, block_id));
        if !safe_to_notar_is_combined_threshold_met(
            cur_verified_stake,
            cur_verified_skip_stake,
            self.total_stake,
        ) {
            self.safe_to_notar_candidates
                .insert((cur_observed_stake, block_id));
        }
    }

    /// A new unverified notar-fallback vote was added.  This might have caused a pending entry to be consumed.
    fn add_unverified_nf_vote(
        &mut self,
        block_id: BlockId,
        observed_notar_stake: Stake,
        entry_batch_is_empty: bool,
    ) {
        if entry_batch_is_empty {
            self.pending_entries
                .remove(&(observed_notar_stake, block_id));
        }
    }

    /// A new unverified notar vote was added.
    fn add_unverified_notar_vote(
        &mut self,
        block_id: BlockId,
        prev_observed_notar_stake: Stake,
        additional_unverified_notar_stake: NonZeroStake,
        entry_batch_is_empty: bool,
    ) {
        let cur_observed_notar_stake =
            prev_observed_notar_stake.add(Stake::from(additional_unverified_notar_stake));
        self.stakes_observed
            .remove(&(prev_observed_notar_stake, block_id));
        self.stakes_observed
            .insert((cur_observed_notar_stake, block_id));
        self.total_notar_stake_observed
            .increment(Stake::from(additional_unverified_notar_stake));
        self.pending_entries
            .remove(&(prev_observed_notar_stake, block_id));
        if !entry_batch_is_empty {
            self.pending_entries
                .insert((cur_observed_notar_stake, block_id));
        }
        let was_candidate = self
            .safe_to_notar_candidates
            .remove(&(prev_observed_notar_stake, block_id));
        // if prev observed stake was 0, then adding a new candidate else updating an existing candidate.
        if was_candidate || prev_observed_notar_stake.0 == 0 {
            self.safe_to_notar_candidates
                .insert((cur_observed_notar_stake, block_id));
        }
    }

    fn validator_banned(
        &mut self,
        block_id: BlockId,
        prev_observed_stake: Stake,
        stake_banned: Stake,
        entry_batch_is_empty: bool,
    ) {
        if stake_banned.0 == 0 {
            return;
        }
        let cur_observed_stake = prev_observed_stake.sub(stake_banned);
        self.total_notar_stake_observed.decrement(stake_banned);
        self.stakes_observed
            .remove(&(prev_observed_stake, block_id));
        self.stakes_observed
            .insert((prev_observed_stake.sub(stake_banned), block_id));
        self.pending_entries
            .remove(&(prev_observed_stake, block_id));
        if !entry_batch_is_empty {
            self.pending_entries
                .insert((prev_observed_stake.sub(stake_banned), block_id));
        }
        let was_candidate = self
            .safe_to_notar_candidates
            .remove(&(prev_observed_stake, block_id));
        // only reinsert if was already a candidate and the stake didn't go to 0.
        if was_candidate && cur_observed_stake.0 != 0 {
            self.safe_to_notar_candidates
                .insert((cur_observed_stake, block_id));
        }
    }

    fn highest_verified_stake(&self) -> Option<Stake> {
        self.stakes_verified.last().map(|(s, _)| *s)
    }

    fn highest_observed_stake(&self) -> Option<Stake> {
        self.stakes_observed.last().map(|(s, _)| *s)
    }

    fn highest_safe_to_notar_candidate_stake(&self) -> Option<Stake> {
        self.safe_to_notar_candidates.last().map(|(s, _)| *s)
    }
}

#[must_use]
fn safe_to_notar_is_combined_threshold_met(
    verified_notar: Stake,
    verified_skip: Stake,
    total_stake: NonZeroStake,
) -> bool {
    total_stake.fraction(verified_notar) >= SAFE_TO_NOTAR_COMBINED_NOTAR_ONLY
        && total_stake.fraction(verified_notar.add(verified_skip))
            >= SAFE_TO_NOTAR_COMBINED_NOTAR_AND_SKIP
}

fn handle_notar_state(
    votes: &mut HashMap<VotePayloadToSign, Batch>,
    pending_votes: &mut PendingVoteIndex,
    vote_entry: &mut VoteEntry,
) {
    vote_entry.try_add_to_votes(votes, pending_votes, NOTAR_CERT_THRESHOLD);
    vote_entry.try_add_to_votes(votes, pending_votes, FAST_FINALIZE_CERT_THRESHOLD);
}
