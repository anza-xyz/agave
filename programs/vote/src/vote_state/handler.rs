//! Vote state handler API.
//!
//! Wraps the vote state behind a "handler" API to support converting from an
//! existing vote state version to whichever version is the target (or
//! "current") vote state version.
//!
//! The program must be generic over whichever vote state version is the
//! target, since at compile time the target version is not known (can be
//! changed with a feature gate). For this reason, the handler offers a
//! getter and setter API around vote state, for all operations required by the
//! vote program.

use {
    solana_clock::{Clock, Epoch, Slot, UnixTimestamp},
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    solana_transaction_context::instruction_accounts::BorrowedInstructionAccount,
    solana_vote_interface::{
        authorized_voters::AuthorizedVoters,
        error::VoteError,
        state::{
            BlockTimestamp, LandedVote, Lockout, VoteInit, VoteState1_14_11, VoteStateV3,
            VoteStateV4, VoteStateVersions, BLS_PUBLIC_KEY_COMPRESSED_SIZE,
            MAX_EPOCH_CREDITS_HISTORY, MAX_LOCKOUT_HISTORY, VOTE_CREDITS_GRACE_SLOTS,
            VOTE_CREDITS_MAXIMUM_PER_SLOT,
        },
    },
    std::collections::VecDeque,
};

/// Trait defining the interface for vote state operations.
pub trait VoteStateHandle {
    fn authorized_withdrawer(&self) -> &Pubkey;

    fn set_authorized_withdrawer(&mut self, authorized_withdrawer: Pubkey);

    fn authorized_voters(&self) -> &AuthorizedVoters;

    fn set_new_authorized_voter<F>(
        &mut self,
        authorized_pubkey: &Pubkey,
        current_epoch: Epoch,
        target_epoch: Epoch,
        verify: F,
    ) -> Result<(), InstructionError>
    where
        F: Fn(Pubkey) -> Result<(), InstructionError>;

    fn get_and_update_authorized_voter(
        &mut self,
        current_epoch: Epoch,
    ) -> Result<Pubkey, InstructionError>;

    fn commission(&self) -> u8;

    fn set_commission(&mut self, commission: u8);

    fn node_pubkey(&self) -> &Pubkey;

    fn set_node_pubkey(&mut self, node_pubkey: Pubkey);

    fn set_block_revenue_collector(&mut self, collector: Pubkey);

    fn votes(&self) -> &VecDeque<LandedVote>;

    fn votes_mut(&mut self) -> &mut VecDeque<LandedVote>;

    fn set_votes(&mut self, votes: VecDeque<LandedVote>);

    /// Returns if the vote state contains a vote for the slot `candidate_slot`
    fn contains_slot(&self, candidate_slot: Slot) -> bool;

    fn last_lockout(&self) -> Option<&Lockout>;

    fn last_voted_slot(&self) -> Option<Slot>;

    fn root_slot(&self) -> Option<Slot>;

    fn set_root_slot(&mut self, root_slot: Option<Slot>);

    fn current_epoch(&self) -> Epoch;

    fn epoch_credits(&self) -> &Vec<(Epoch, u64, u64)>;

    fn epoch_credits_mut(&mut self) -> &mut Vec<(Epoch, u64, u64)>;

    fn last_timestamp(&self) -> &BlockTimestamp;

    fn set_last_timestamp(&mut self, timestamp: BlockTimestamp);

    fn set_vote_account_state(
        self,
        vote_account: &mut BorrowedInstructionAccount,
    ) -> Result<(), InstructionError>;

    fn credits_for_vote_at_index(&self, index: usize) -> u64 {
        let latency = self
            .votes()
            .get(index)
            .map_or(0, |landed_vote| landed_vote.latency);

        // If latency is 0, this means that the Lockout was created and stored from a software version that did not
        // store vote latencies; in this case, 1 credit is awarded
        if latency == 0 {
            1
        } else {
            match latency.checked_sub(VOTE_CREDITS_GRACE_SLOTS) {
                None | Some(0) => {
                    // latency was <= VOTE_CREDITS_GRACE_SLOTS, so maximum credits are awarded
                    VOTE_CREDITS_MAXIMUM_PER_SLOT as u64
                }

                Some(diff) => {
                    // diff = latency - VOTE_CREDITS_GRACE_SLOTS, and diff > 0
                    // Subtract diff from VOTE_CREDITS_MAXIMUM_PER_SLOT which is the number of credits to award
                    match VOTE_CREDITS_MAXIMUM_PER_SLOT.checked_sub(diff) {
                        // If diff >= VOTE_CREDITS_MAXIMUM_PER_SLOT, 1 credit is awarded
                        None | Some(0) => 1,

                        Some(credits) => credits as u64,
                    }
                }
            }
        }
    }

    fn increment_credits(&mut self, epoch: Epoch, credits: u64) {
        // increment credits, record by epoch

        // never seen a credit
        if self.epoch_credits().is_empty() {
            self.epoch_credits_mut().push((epoch, 0, 0));
        } else if epoch != self.epoch_credits().last().unwrap().0 {
            let (_, credits, prev_credits) = *self.epoch_credits().last().unwrap();

            if credits != prev_credits {
                // if credits were earned previous epoch
                // append entry at end of list for the new epoch
                self.epoch_credits_mut().push((epoch, credits, credits));
            } else {
                // else just move the current epoch
                self.epoch_credits_mut().last_mut().unwrap().0 = epoch;
            }

            // Remove too old epoch_credits
            if self.epoch_credits().len() > MAX_EPOCH_CREDITS_HISTORY {
                self.epoch_credits_mut().remove(0);
            }
        }

        self.epoch_credits_mut().last_mut().unwrap().1 = self
            .epoch_credits()
            .last()
            .unwrap()
            .1
            .saturating_add(credits);
    }

    fn process_timestamp(&mut self, slot: Slot, timestamp: UnixTimestamp) -> Result<(), VoteError> {
        let last_timestamp = self.last_timestamp();
        if (slot < last_timestamp.slot || timestamp < last_timestamp.timestamp)
            || (slot == last_timestamp.slot
                && &BlockTimestamp { slot, timestamp } != last_timestamp
                && last_timestamp.slot != 0)
        {
            return Err(VoteError::TimestampTooOld);
        }
        self.set_last_timestamp(BlockTimestamp { slot, timestamp });
        Ok(())
    }

    fn pop_expired_votes(&mut self, next_vote_slot: Slot) {
        while let Some(vote) = self.last_lockout() {
            if !vote.is_locked_out_at_slot(next_vote_slot) {
                self.votes_mut().pop_back();
            } else {
                break;
            }
        }
    }

    fn double_lockouts(&mut self) {
        let stack_depth = self.votes().len();
        for (i, v) in self.votes_mut().iter_mut().enumerate() {
            // Don't increase the lockout for this vote until we get more confirmations
            // than the max number of confirmations this vote has seen
            if stack_depth
                > i.checked_add(v.confirmation_count() as usize).expect(
                    "`confirmation_count` and tower_size should be bounded by \
                     `MAX_LOCKOUT_HISTORY`",
                )
            {
                v.lockout.increase_confirmation_count(1);
            }
        }
    }

    fn process_next_vote_slot(&mut self, next_vote_slot: Slot, epoch: Epoch, current_slot: Slot) {
        // Ignore votes for slots earlier than we already have votes for
        if self
            .last_voted_slot()
            .is_some_and(|last_voted_slot| next_vote_slot <= last_voted_slot)
        {
            return;
        }

        self.pop_expired_votes(next_vote_slot);

        let landed_vote = LandedVote {
            latency: compute_vote_latency(next_vote_slot, current_slot),
            lockout: Lockout::new(next_vote_slot),
        };

        // Once the stack is full, pop the oldest lockout and distribute rewards
        if self.votes().len() == MAX_LOCKOUT_HISTORY {
            let credits = self.credits_for_vote_at_index(0);
            let landed_vote = self.votes_mut().pop_front().unwrap();
            self.set_root_slot(Some(landed_vote.slot()));

            self.increment_credits(epoch, credits);
        }
        self.votes_mut().push_back(landed_vote);
        self.double_lockouts();
    }

    #[cfg(test)]
    fn credits(&self) -> u64 {
        if self.epoch_credits().is_empty() {
            0
        } else {
            self.epoch_credits().last().unwrap().1
        }
    }

    #[cfg(test)]
    fn nth_recent_lockout(&self, position: usize) -> Option<&Lockout> {
        if position < self.votes().len() {
            let pos = self
                .votes()
                .len()
                .checked_sub(position)
                .and_then(|pos| pos.checked_sub(1))?;
            self.votes().get(pos).map(|vote| &vote.lockout)
        } else {
            None
        }
    }
}

impl VoteStateHandle for VoteStateV3 {
    fn authorized_withdrawer(&self) -> &Pubkey {
        &self.authorized_withdrawer
    }

    fn set_authorized_withdrawer(&mut self, authorized_withdrawer: Pubkey) {
        self.authorized_withdrawer = authorized_withdrawer;
    }

    fn authorized_voters(&self) -> &AuthorizedVoters {
        &self.authorized_voters
    }

    fn set_new_authorized_voter<F>(
        &mut self,
        authorized_pubkey: &Pubkey,
        current_epoch: Epoch,
        target_epoch: Epoch,
        verify: F,
    ) -> Result<(), InstructionError>
    where
        F: Fn(Pubkey) -> Result<(), InstructionError>,
    {
        let epoch_authorized_voter = self.get_and_update_authorized_voter(current_epoch)?;
        verify(epoch_authorized_voter)?;

        // The offset in slots `n` on which the target_epoch
        // (default value `DEFAULT_LEADER_SCHEDULE_SLOT_OFFSET`) is
        // calculated is the number of slots available from the
        // first slot `S` of an epoch in which to set a new voter for
        // the epoch at `S` + `n`
        if self.authorized_voters.contains(target_epoch) {
            return Err(VoteError::TooSoonToReauthorize.into());
        }

        // Get the latest authorized_voter
        let (latest_epoch, latest_authorized_pubkey) = self
            .authorized_voters
            .last()
            .ok_or(InstructionError::InvalidAccountData)?;

        // If we're not setting the same pubkey as authorized pubkey again,
        // then update the list of prior voters to mark the expiration
        // of the old authorized pubkey
        if latest_authorized_pubkey != authorized_pubkey {
            // Update the epoch ranges of authorized pubkeys that will be expired
            let epoch_of_last_authorized_switch =
                self.prior_voters.last().map(|range| range.2).unwrap_or(0);

            // target_epoch must:
            // 1) Be monotonically increasing due to the clock always
            //    moving forward
            // 2) not be equal to latest epoch otherwise this
            //    function would have returned TooSoonToReauthorize error
            //    above
            if target_epoch <= *latest_epoch {
                return Err(InstructionError::InvalidAccountData);
            }

            // Commit the new state
            self.prior_voters.append((
                *latest_authorized_pubkey,
                epoch_of_last_authorized_switch,
                target_epoch,
            ));
        }

        self.authorized_voters
            .insert(target_epoch, *authorized_pubkey);

        Ok(())
    }

    fn get_and_update_authorized_voter(
        &mut self,
        current_epoch: Epoch,
    ) -> Result<Pubkey, InstructionError> {
        let pubkey = self
            .authorized_voters
            .get_and_cache_authorized_voter_for_epoch(current_epoch)
            .ok_or(InstructionError::InvalidAccountData)?;
        self.authorized_voters
            .purge_authorized_voters(current_epoch);
        Ok(pubkey)
    }

    fn commission(&self) -> u8 {
        self.commission
    }

    fn set_commission(&mut self, commission: u8) {
        self.commission = commission;
    }

    fn node_pubkey(&self) -> &Pubkey {
        &self.node_pubkey
    }

    fn set_node_pubkey(&mut self, node_pubkey: Pubkey) {
        self.node_pubkey = node_pubkey;
    }

    fn set_block_revenue_collector(&mut self, _collector: Pubkey) {
        // No-op for v3: field does not exist.
    }

    fn votes(&self) -> &VecDeque<LandedVote> {
        &self.votes
    }

    fn votes_mut(&mut self) -> &mut VecDeque<LandedVote> {
        &mut self.votes
    }

    fn set_votes(&mut self, votes: VecDeque<LandedVote>) {
        self.votes = votes;
    }

    fn contains_slot(&self, candidate_slot: Slot) -> bool {
        self.votes
            .binary_search_by(|vote| vote.slot().cmp(&candidate_slot))
            .is_ok()
    }

    fn last_lockout(&self) -> Option<&Lockout> {
        self.votes.back().map(|vote| &vote.lockout)
    }

    fn last_voted_slot(&self) -> Option<Slot> {
        self.last_lockout().map(|v| v.slot())
    }

    fn root_slot(&self) -> Option<Slot> {
        self.root_slot
    }

    fn set_root_slot(&mut self, root_slot: Option<Slot>) {
        self.root_slot = root_slot;
    }

    fn current_epoch(&self) -> Epoch {
        if self.epoch_credits.is_empty() {
            0
        } else {
            self.epoch_credits.last().unwrap().0
        }
    }

    fn epoch_credits(&self) -> &Vec<(Epoch, u64, u64)> {
        &self.epoch_credits
    }

    fn epoch_credits_mut(&mut self) -> &mut Vec<(Epoch, u64, u64)> {
        &mut self.epoch_credits
    }

    fn last_timestamp(&self) -> &BlockTimestamp {
        &self.last_timestamp
    }

    fn set_last_timestamp(&mut self, timestamp: BlockTimestamp) {
        self.last_timestamp = timestamp;
    }

    fn set_vote_account_state(
        self,
        vote_account: &mut BorrowedInstructionAccount,
    ) -> Result<(), InstructionError> {
        // If the account is not large enough to store the vote state, then attempt a realloc to make it large enough.
        // The realloc can only proceed if the vote account has balance sufficient for rent exemption at the new size.
        if (vote_account.get_data().len() < VoteStateV3::size_of())
            && (!vote_account.is_rent_exempt_at_data_length(VoteStateV3::size_of())
                || vote_account
                    .set_data_length(VoteStateV3::size_of())
                    .is_err())
        {
            // Account cannot be resized to the size of a vote state as it will not be rent exempt, or failed to be
            // resized for other reasons.  So store the V1_14_11 version.
            return vote_account.set_state(&VoteStateVersions::V1_14_11(Box::new(
                VoteState1_14_11::from(self),
            )));
        }
        // Vote account is large enough to store the newest version of vote state
        vote_account.set_state(&VoteStateVersions::V3(Box::new(self)))
    }
}

impl VoteStateHandle for VoteStateV4 {
    fn authorized_withdrawer(&self) -> &Pubkey {
        &self.authorized_withdrawer
    }

    fn set_authorized_withdrawer(&mut self, authorized_withdrawer: Pubkey) {
        self.authorized_withdrawer = authorized_withdrawer;
    }

    fn authorized_voters(&self) -> &AuthorizedVoters {
        &self.authorized_voters
    }

    fn set_new_authorized_voter<F>(
        &mut self,
        authorized_pubkey: &Pubkey,
        current_epoch: Epoch,
        target_epoch: Epoch,
        verify: F,
    ) -> Result<(), InstructionError>
    where
        F: Fn(Pubkey) -> Result<(), InstructionError>,
    {
        // Similar to the v3 implementation, but with no `prior_voters` field.

        let epoch_authorized_voter = self.get_and_update_authorized_voter(current_epoch)?;
        verify(epoch_authorized_voter)?;

        // The offset in slots `n` on which the target_epoch
        // (default value `DEFAULT_LEADER_SCHEDULE_SLOT_OFFSET`) is
        // calculated is the number of slots available from the
        // first slot `S` of an epoch in which to set a new voter for
        // the epoch at `S` + `n`
        if self.authorized_voters.contains(target_epoch) {
            return Err(VoteError::TooSoonToReauthorize.into());
        }

        self.authorized_voters
            .insert(target_epoch, *authorized_pubkey);

        Ok(())
    }

    fn get_and_update_authorized_voter(
        &mut self,
        current_epoch: Epoch,
    ) -> Result<Pubkey, InstructionError> {
        let pubkey = self
            .authorized_voters
            .get_and_cache_authorized_voter_for_epoch(current_epoch)
            .ok_or(InstructionError::InvalidAccountData)?;
        // Per SIMD-0185, v4 retains voters for `current_epoch - 1` through
        // `current_epoch + 2`. Only purge entries for epochs less than
        // `current_epoch - 1`.
        self.authorized_voters
            .purge_authorized_voters(current_epoch.saturating_sub(1));
        Ok(pubkey)
    }

    fn commission(&self) -> u8 {
        (self.inflation_rewards_commission_bps / 100) as u8
    }

    #[allow(clippy::arithmetic_side_effects)]
    fn set_commission(&mut self, commission: u8) {
        // Safety: u16::MAX > u8::MAX * 100
        self.inflation_rewards_commission_bps = (commission as u16) * 100;
    }

    fn node_pubkey(&self) -> &Pubkey {
        &self.node_pubkey
    }

    fn set_node_pubkey(&mut self, node_pubkey: Pubkey) {
        self.node_pubkey = node_pubkey;
    }

    fn set_block_revenue_collector(&mut self, collector: Pubkey) {
        self.block_revenue_collector = collector;
    }

    fn votes(&self) -> &VecDeque<LandedVote> {
        &self.votes
    }

    fn votes_mut(&mut self) -> &mut VecDeque<LandedVote> {
        &mut self.votes
    }

    fn set_votes(&mut self, votes: VecDeque<LandedVote>) {
        self.votes = votes;
    }

    fn contains_slot(&self, candidate_slot: Slot) -> bool {
        self.votes
            .binary_search_by(|vote| vote.slot().cmp(&candidate_slot))
            .is_ok()
    }

    fn last_lockout(&self) -> Option<&Lockout> {
        self.votes.back().map(|vote| &vote.lockout)
    }

    fn last_voted_slot(&self) -> Option<Slot> {
        self.last_lockout().map(|v| v.slot())
    }

    fn root_slot(&self) -> Option<Slot> {
        self.root_slot
    }

    fn set_root_slot(&mut self, root_slot: Option<Slot>) {
        self.root_slot = root_slot;
    }

    fn current_epoch(&self) -> Epoch {
        if self.epoch_credits.is_empty() {
            0
        } else {
            self.epoch_credits.last().unwrap().0
        }
    }

    fn epoch_credits(&self) -> &Vec<(Epoch, u64, u64)> {
        &self.epoch_credits
    }

    fn epoch_credits_mut(&mut self) -> &mut Vec<(Epoch, u64, u64)> {
        &mut self.epoch_credits
    }

    fn last_timestamp(&self) -> &BlockTimestamp {
        &self.last_timestamp
    }

    fn set_last_timestamp(&mut self, timestamp: BlockTimestamp) {
        self.last_timestamp = timestamp;
    }

    fn set_vote_account_state(
        self,
        vote_account: &mut BorrowedInstructionAccount,
    ) -> Result<(), InstructionError> {
        // If the account is not large enough to store the vote state, then attempt a realloc to make it large enough.
        // The realloc can only proceed if the vote account has balance sufficient for rent exemption at the new size.
        if (vote_account.get_data().len() < VoteStateV4::size_of())
            && (!vote_account.is_rent_exempt_at_data_length(VoteStateV4::size_of())
                || vote_account
                    .set_data_length(VoteStateV4::size_of())
                    .is_err())
        {
            // Unlike with conversions to v3, we will not gracefully default to
            // storing a v1_14_11. Instead, throw an error, as per SIMD-0185.
            return Err(InstructionError::AccountNotRentExempt);
        }
        // Vote account is large enough to store the newest version of vote state
        vote_account.set_state(&VoteStateVersions::V4(Box::new(self)))
    }
}

/// Default block revenue commission rate in basis points (100%) per SIMD-0185.
const DEFAULT_BLOCK_REVENUE_COMMISSION_BPS: u16 = 10_000;

/// Create a new VoteStateV4 from `VoteInit` with proper SIMD-0185 defaults.
/// Note this is a temporary substitute for `VoteStateV4::new`.
#[allow(clippy::arithmetic_side_effects)]
pub(crate) fn create_new_vote_state_v4(
    vote_pubkey: &Pubkey,
    vote_init: &VoteInit,
    clock: &Clock,
) -> VoteStateV4 {
    VoteStateV4 {
        node_pubkey: vote_init.node_pubkey,
        authorized_voters: AuthorizedVoters::new(clock.epoch, vote_init.authorized_voter),
        authorized_withdrawer: vote_init.authorized_withdrawer,
        inflation_rewards_commission_bps: (vote_init.commission as u16) * 100, // u16::MAX > u8::MAX * 100
        // Per SIMD-0185, set default collectors and commission
        inflation_rewards_collector: *vote_pubkey,
        block_revenue_collector: vote_init.node_pubkey,
        block_revenue_commission_bps: DEFAULT_BLOCK_REVENUE_COMMISSION_BPS,
        ..VoteStateV4::default()
    }
}

/// (Alpenglow) Create a test-only `VoteStateV4` with the provided values.
pub(crate) fn create_new_vote_state_v4_for_tests(
    node_pubkey: &Pubkey,
    authorized_voter: &Pubkey,
    authorized_withdrawer: &Pubkey,
    bls_pubkey_compressed: Option<[u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE]>,
    inflation_rewards_commission_bps: u16,
) -> VoteStateV4 {
    VoteStateV4 {
        node_pubkey: *node_pubkey,
        authorized_voters: AuthorizedVoters::new(0, *authorized_voter),
        authorized_withdrawer: *authorized_withdrawer,
        bls_pubkey_compressed,
        inflation_rewards_commission_bps,
        ..VoteStateV4::default()
    }
}

/// The target version to convert all deserialized vote state into.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VoteStateTargetVersion {
    V3,
    V4,
    // New vote state versions will be added here...
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
enum TargetVoteState {
    V3(VoteStateV3),
    V4(VoteStateV4),
    // New vote state versions will be added here...
}

/// Vote state handler for
/// * Deserializing vote state
/// * Converting vote state in-memory to target version
/// * Operating on the vote state data agnostically
/// * Serializing the resulting state to the vote account
#[derive(Clone, Debug, PartialEq)]
pub struct VoteStateHandler {
    target_state: TargetVoteState,
}

impl VoteStateHandle for VoteStateHandler {
    fn authorized_withdrawer(&self) -> &Pubkey {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.authorized_withdrawer(),
            TargetVoteState::V4(v4) => v4.authorized_withdrawer(),
        }
    }

    fn set_authorized_withdrawer(&mut self, authorized_withdrawer: Pubkey) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.set_authorized_withdrawer(authorized_withdrawer),
            TargetVoteState::V4(v4) => v4.set_authorized_withdrawer(authorized_withdrawer),
        }
    }

    fn authorized_voters(&self) -> &AuthorizedVoters {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.authorized_voters(),
            TargetVoteState::V4(v4) => v4.authorized_voters(),
        }
    }

    fn set_new_authorized_voter<F>(
        &mut self,
        authorized_pubkey: &Pubkey,
        current_epoch: Epoch,
        target_epoch: Epoch,
        verify: F,
    ) -> Result<(), InstructionError>
    where
        F: Fn(Pubkey) -> Result<(), InstructionError>,
    {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => {
                v3.set_new_authorized_voter(authorized_pubkey, current_epoch, target_epoch, verify)
            }
            TargetVoteState::V4(v4) => {
                v4.set_new_authorized_voter(authorized_pubkey, current_epoch, target_epoch, verify)
            }
        }
    }

    fn get_and_update_authorized_voter(
        &mut self,
        current_epoch: Epoch,
    ) -> Result<Pubkey, InstructionError> {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.get_and_update_authorized_voter(current_epoch),
            TargetVoteState::V4(v4) => v4.get_and_update_authorized_voter(current_epoch),
        }
    }

    fn commission(&self) -> u8 {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.commission(),
            TargetVoteState::V4(v4) => v4.commission(),
        }
    }

    fn set_commission(&mut self, commission: u8) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.set_commission(commission),
            TargetVoteState::V4(v4) => v4.set_commission(commission),
        }
    }

    fn node_pubkey(&self) -> &Pubkey {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.node_pubkey(),
            TargetVoteState::V4(v4) => v4.node_pubkey(),
        }
    }

    fn set_node_pubkey(&mut self, node_pubkey: Pubkey) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.set_node_pubkey(node_pubkey),
            TargetVoteState::V4(v4) => v4.set_node_pubkey(node_pubkey),
        }
    }

    fn set_block_revenue_collector(&mut self, collector: Pubkey) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.set_block_revenue_collector(collector),
            TargetVoteState::V4(v4) => v4.set_block_revenue_collector(collector),
        }
    }

    fn votes(&self) -> &VecDeque<LandedVote> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.votes(),
            TargetVoteState::V4(v4) => v4.votes(),
        }
    }

    fn votes_mut(&mut self) -> &mut VecDeque<LandedVote> {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.votes_mut(),
            TargetVoteState::V4(v4) => v4.votes_mut(),
        }
    }

    fn set_votes(&mut self, votes: VecDeque<LandedVote>) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.set_votes(votes),
            TargetVoteState::V4(v4) => v4.set_votes(votes),
        }
    }

    fn contains_slot(&self, candidate_slot: Slot) -> bool {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.contains_slot(candidate_slot),
            TargetVoteState::V4(v4) => v4.contains_slot(candidate_slot),
        }
    }

    fn last_lockout(&self) -> Option<&Lockout> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.last_lockout(),
            TargetVoteState::V4(v4) => v4.last_lockout(),
        }
    }

    fn last_voted_slot(&self) -> Option<Slot> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.last_voted_slot(),
            TargetVoteState::V4(v4) => v4.last_voted_slot(),
        }
    }

    fn root_slot(&self) -> Option<Slot> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.root_slot(),
            TargetVoteState::V4(v4) => v4.root_slot(),
        }
    }

    fn set_root_slot(&mut self, root_slot: Option<Slot>) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.set_root_slot(root_slot),
            TargetVoteState::V4(v4) => v4.set_root_slot(root_slot),
        }
    }

    fn current_epoch(&self) -> Epoch {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.current_epoch(),
            TargetVoteState::V4(v4) => v4.current_epoch(),
        }
    }

    fn epoch_credits(&self) -> &Vec<(Epoch, u64, u64)> {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.epoch_credits(),
            TargetVoteState::V4(v4) => v4.epoch_credits(),
        }
    }

    fn epoch_credits_mut(&mut self) -> &mut Vec<(Epoch, u64, u64)> {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.epoch_credits_mut(),
            TargetVoteState::V4(v4) => v4.epoch_credits_mut(),
        }
    }

    fn last_timestamp(&self) -> &BlockTimestamp {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3.last_timestamp(),
            TargetVoteState::V4(v4) => v4.last_timestamp(),
        }
    }

    fn set_last_timestamp(&mut self, timestamp: BlockTimestamp) {
        match &mut self.target_state {
            TargetVoteState::V3(v3) => v3.set_last_timestamp(timestamp),
            TargetVoteState::V4(v4) => v4.set_last_timestamp(timestamp),
        }
    }

    fn set_vote_account_state(
        self,
        vote_account: &mut BorrowedInstructionAccount,
    ) -> Result<(), InstructionError> {
        match self.target_state {
            TargetVoteState::V3(v3) => v3.set_vote_account_state(vote_account),
            TargetVoteState::V4(v4) => v4.set_vote_account_state(vote_account),
        }
    }
}

impl VoteStateHandler {
    pub fn init_vote_account_state(
        vote_account: &mut BorrowedInstructionAccount,
        vote_init: &VoteInit,
        clock: &Clock,
        target_version: VoteStateTargetVersion,
    ) -> Result<(), InstructionError> {
        match target_version {
            VoteStateTargetVersion::V3 => {
                VoteStateV3::new(vote_init, clock).set_vote_account_state(vote_account)
            }
            VoteStateTargetVersion::V4 => {
                let vote_state = create_new_vote_state_v4(vote_account.get_key(), vote_init, clock);
                vote_state.set_vote_account_state(vote_account)
            }
        }
    }

    pub fn deinitialize_vote_account_state(
        vote_account: &mut BorrowedInstructionAccount,
        target_version: VoteStateTargetVersion,
    ) -> Result<(), InstructionError> {
        match target_version {
            VoteStateTargetVersion::V3 => {
                VoteStateV3::default().set_vote_account_state(vote_account)
            }
            VoteStateTargetVersion::V4 => {
                // As per SIMD-0185, clear the entire account.
                vote_account.get_data_mut()?.fill(0);
                Ok(())
            }
        }
    }

    pub fn check_vote_account_length(
        vote_account: &mut BorrowedInstructionAccount,
        target_version: VoteStateTargetVersion,
    ) -> Result<(), InstructionError> {
        let length = vote_account.get_data().len();
        let expected = match target_version {
            VoteStateTargetVersion::V3 => VoteStateV3::size_of(),
            VoteStateTargetVersion::V4 => VoteStateV4::size_of(),
        };
        if length != expected {
            Err(InstructionError::InvalidAccountData)
        } else {
            Ok(())
        }
    }

    pub(crate) fn new_v3(vote_state: VoteStateV3) -> Self {
        Self {
            target_state: TargetVoteState::V3(vote_state),
        }
    }

    pub(crate) fn new_v4(vote_state: VoteStateV4) -> Self {
        Self {
            target_state: TargetVoteState::V4(vote_state),
        }
    }

    #[cfg(test)]
    pub fn default_v3() -> Self {
        Self::new_v3(VoteStateV3::default())
    }

    #[cfg(test)]
    pub fn default_v4() -> Self {
        Self::new_v4(VoteStateV4::default())
    }

    #[cfg(test)]
    pub fn as_ref_v3(&self) -> &VoteStateV3 {
        match &self.target_state {
            TargetVoteState::V3(v3) => v3,
            _ => panic!("not a v3"),
        }
    }

    #[cfg(test)]
    pub fn as_ref_v4(&self) -> &VoteStateV4 {
        match &self.target_state {
            TargetVoteState::V4(v4) => v4,
            _ => panic!("not a v4"),
        }
    }

    #[cfg(test)]
    pub fn serialize(self) -> Vec<u8> {
        match self.target_state {
            TargetVoteState::V3(v3) => {
                let mut data = vec![0; VoteStateV3::size_of()];
                let versioned = VoteStateVersions::V3(Box::new(v3));
                bincode::serialize_into(&mut data[..], &versioned).unwrap();
                data
            }
            TargetVoteState::V4(v4) => {
                let mut data = vec![0; VoteStateV4::size_of()];
                let versioned = VoteStateVersions::V4(Box::new(v4));
                bincode::serialize_into(&mut data[..], &versioned).unwrap();
                data
            }
        }
    }
}

// Computes the vote latency for vote on voted_for_slot where the vote itself landed in current_slot
pub(crate) fn compute_vote_latency(voted_for_slot: Slot, current_slot: Slot) -> u8 {
    std::cmp::min(current_slot.saturating_sub(voted_for_slot), u8::MAX as u64) as u8
}

pub(crate) fn try_convert_to_vote_state_v4(
    versioned: VoteStateVersions,
    vote_pubkey: &Pubkey,
) -> Result<VoteStateV4, InstructionError> {
    match versioned {
        VoteStateVersions::Uninitialized => Err(InstructionError::UninitializedAccount),
        VoteStateVersions::V1_14_11(state) => Ok(VoteStateV4 {
            node_pubkey: state.node_pubkey,
            authorized_withdrawer: state.authorized_withdrawer,
            inflation_rewards_collector: *vote_pubkey,
            block_revenue_collector: state.node_pubkey,
            inflation_rewards_commission_bps: u16::from(state.commission).saturating_mul(100),
            block_revenue_commission_bps: 10_000u16,
            pending_delegator_rewards: 0,
            bls_pubkey_compressed: None,
            votes: landed_votes_from_lockouts(state.votes),
            root_slot: state.root_slot,
            authorized_voters: state.authorized_voters.clone(),
            epoch_credits: state.epoch_credits,
            last_timestamp: state.last_timestamp,
        }),
        VoteStateVersions::V3(state) => Ok(VoteStateV4 {
            node_pubkey: state.node_pubkey,
            authorized_withdrawer: state.authorized_withdrawer,
            inflation_rewards_collector: *vote_pubkey,
            block_revenue_collector: state.node_pubkey,
            inflation_rewards_commission_bps: u16::from(state.commission).saturating_mul(100),
            block_revenue_commission_bps: 10_000u16,
            pending_delegator_rewards: 0,
            bls_pubkey_compressed: None,
            votes: state.votes,
            root_slot: state.root_slot,
            authorized_voters: state.authorized_voters,
            epoch_credits: state.epoch_credits,
            last_timestamp: state.last_timestamp,
        }),
        VoteStateVersions::V4(state) => Ok(*state),
    }
}

fn landed_votes_from_lockouts(lockouts: VecDeque<Lockout>) -> VecDeque<LandedVote> {
    lockouts.into_iter().map(|lockout| lockout.into()).collect()
}

#[allow(clippy::arithmetic_side_effects)]
#[allow(clippy::type_complexity)]
#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::id,
        solana_account::AccountSharedData,
        solana_clock::Clock,
        solana_epoch_schedule::MAX_LEADER_SCHEDULE_EPOCH_OFFSET,
        solana_pubkey::Pubkey,
        solana_rent::Rent,
        solana_sdk_ids::native_loader,
        solana_transaction_context::{
            instruction_accounts::InstructionAccount, TransactionContext,
        },
        solana_vote_interface::{
            authorized_voters::AuthorizedVoters,
            state::{BlockTimestamp, VoteInit, MAX_EPOCH_CREDITS_HISTORY, MAX_LOCKOUT_HISTORY},
        },
        std::collections::VecDeque,
        test_case::test_case,
    };

    fn mock_transaction_context(
        vote_pubkey: Pubkey,
        vote_account: AccountSharedData,
        rent: Rent,
    ) -> TransactionContext<'static> {
        let program_account = AccountSharedData::new(0, 0, &native_loader::id());
        let mut transaction_context = TransactionContext::new(
            vec![(id(), program_account), (vote_pubkey, vote_account)],
            rent,
            0,
            0,
        );
        transaction_context
            .configure_next_instruction_for_tests(
                0,
                vec![InstructionAccount::new(1, false, true)],
                vec![],
            )
            .unwrap();
        transaction_context
    }

    fn get_max_sized_vote_state_v3() -> VoteStateV3 {
        let mut authorized_voters = AuthorizedVoters::default();
        for i in 0..=MAX_LEADER_SCHEDULE_EPOCH_OFFSET {
            authorized_voters.insert(i, Pubkey::new_unique());
        }

        VoteStateV3 {
            votes: VecDeque::from(vec![LandedVote::default(); MAX_LOCKOUT_HISTORY]),
            root_slot: Some(u64::MAX),
            epoch_credits: vec![(0, 0, 0); MAX_EPOCH_CREDITS_HISTORY],
            authorized_voters,
            ..Default::default()
        }
    }

    fn get_max_sized_vote_state_v4() -> VoteStateV4 {
        let mut authorized_voters = AuthorizedVoters::default();
        for i in 0..=MAX_LEADER_SCHEDULE_EPOCH_OFFSET {
            authorized_voters.insert(i, Pubkey::new_unique());
        }

        VoteStateV4 {
            votes: VecDeque::from(vec![LandedVote::default(); MAX_LOCKOUT_HISTORY]),
            root_slot: Some(u64::MAX),
            epoch_credits: vec![(0, 0, 0); MAX_EPOCH_CREDITS_HISTORY],
            authorized_voters,
            bls_pubkey_compressed: Some([255; BLS_PUBLIC_KEY_COMPRESSED_SIZE]),
            ..Default::default()
        }
    }

    fn set_new_authorized_voter_and_assert<T: VoteStateHandle>(
        vote_state: &mut T,
        original_voter: Pubkey,
        epoch_offset: Epoch,
        prior_voters_last_callback: Option<fn(&T) -> &(Pubkey, Epoch, Epoch)>,
    ) {
        let new_voter = Pubkey::new_unique();
        // Set a new authorized voter
        vote_state
            .set_new_authorized_voter(&new_voter, 0, epoch_offset, |_| Ok(()))
            .unwrap();

        if let Some(prior_voters_last) = prior_voters_last_callback {
            assert_eq!(
                prior_voters_last(vote_state),
                &(original_voter, 0, epoch_offset),
            );
        }

        // Trying to set authorized voter for same epoch again should fail
        assert_eq!(
            vote_state.set_new_authorized_voter(&new_voter, 0, epoch_offset, |_| Ok(())),
            Err(VoteError::TooSoonToReauthorize.into())
        );

        // Setting the same authorized voter again should succeed
        vote_state
            .set_new_authorized_voter(&new_voter, 2, 2 + epoch_offset, |_| Ok(()))
            .unwrap();

        // Set a third and fourth authorized voter
        let new_voter2 = Pubkey::new_unique();
        vote_state
            .set_new_authorized_voter(&new_voter2, 3, 3 + epoch_offset, |_| Ok(()))
            .unwrap();
        if let Some(prior_voters_last) = prior_voters_last_callback {
            assert_eq!(
                prior_voters_last(vote_state),
                &(new_voter, epoch_offset, 3 + epoch_offset),
            );
        }

        let new_voter3 = Pubkey::new_unique();
        vote_state
            .set_new_authorized_voter(&new_voter3, 6, 6 + epoch_offset, |_| Ok(()))
            .unwrap();
        if let Some(prior_voters_last) = prior_voters_last_callback {
            assert_eq!(
                prior_voters_last(vote_state),
                &(new_voter2, 3 + epoch_offset, 6 + epoch_offset),
            );
        }

        // Check can set back to original voter
        vote_state
            .set_new_authorized_voter(&original_voter, 9, 9 + epoch_offset, |_| Ok(()))
            .unwrap();

        // Run with these voters for a while, check the ranges of authorized
        // voters is correct
        for i in 9..epoch_offset {
            assert_eq!(
                vote_state.get_and_update_authorized_voter(i).unwrap(),
                original_voter
            );
        }
        for i in epoch_offset..3 + epoch_offset {
            assert_eq!(
                vote_state.get_and_update_authorized_voter(i).unwrap(),
                new_voter
            );
        }
        for i in 3 + epoch_offset..6 + epoch_offset {
            assert_eq!(
                vote_state.get_and_update_authorized_voter(i).unwrap(),
                new_voter2
            );
        }
        for i in 6 + epoch_offset..9 + epoch_offset {
            assert_eq!(
                vote_state.get_and_update_authorized_voter(i).unwrap(),
                new_voter3
            );
        }
        for i in 9 + epoch_offset..=10 + epoch_offset {
            assert_eq!(
                vote_state.get_and_update_authorized_voter(i).unwrap(),
                original_voter
            );
        }
    }

    #[test]
    fn test_set_new_authorized_voter() {
        let vote_pubkey = Pubkey::new_unique();
        let original_voter = Pubkey::new_unique();
        let epoch_offset = 15;

        let vote_init = VoteInit {
            node_pubkey: original_voter,
            authorized_voter: original_voter,
            authorized_withdrawer: original_voter,
            commission: 0,
        };
        let clock = Clock::default();

        // Start with v3. We'll also check `prior_voters`.
        let mut vote_state = VoteStateV3::new(&vote_init, &clock);
        assert!(vote_state.prior_voters.last().is_none());

        set_new_authorized_voter_and_assert(
            &mut vote_state,
            original_voter,
            epoch_offset,
            Some(|vote_state: &VoteStateV3| vote_state.prior_voters.last().unwrap()),
        );

        // Now try with v4. No `prior_voters` to check.
        let mut vote_state = create_new_vote_state_v4(&vote_pubkey, &vote_init, &clock);

        set_new_authorized_voter_and_assert(&mut vote_state, original_voter, epoch_offset, None);
    }

    fn assert_authorized_voter_is_locked_within_epoch<T: VoteStateHandle>(
        vote_state: &mut T,
        original_voter: &Pubkey,
    ) {
        // Test that it's not possible to set a new authorized
        // voter within the same epoch, even if none has been
        // explicitly set before
        let new_voter = Pubkey::new_unique();
        assert_eq!(
            vote_state.set_new_authorized_voter(&new_voter, 1, 1, |_| Ok(())),
            Err(VoteError::TooSoonToReauthorize.into())
        );
        assert_eq!(
            vote_state.authorized_voters().get_authorized_voter(1),
            Some(*original_voter)
        );
        // Set a new authorized voter for a future epoch
        assert_eq!(
            vote_state.set_new_authorized_voter(&new_voter, 1, 2, |_| Ok(())),
            Ok(())
        );
        // Test that it's not possible to set a new authorized
        // voter within the same epoch, even if none has been
        // explicitly set before
        assert_eq!(
            vote_state.set_new_authorized_voter(original_voter, 3, 3, |_| Ok(())),
            Err(VoteError::TooSoonToReauthorize.into())
        );
        assert_eq!(
            vote_state.authorized_voters().get_authorized_voter(3),
            Some(new_voter)
        );
    }

    #[test]
    fn test_authorized_voter_is_locked_within_epoch() {
        let vote_pubkey = Pubkey::new_unique();
        let original_voter = Pubkey::new_unique();

        let vote_init = VoteInit {
            node_pubkey: original_voter,
            authorized_voter: original_voter,
            authorized_withdrawer: original_voter,
            commission: 0,
        };
        let clock = Clock::default();

        // First test v3.
        let mut vote_state = VoteStateV3::new(&vote_init, &clock);
        assert_authorized_voter_is_locked_within_epoch(&mut vote_state, &original_voter);

        // Now v4.
        let mut vote_state = create_new_vote_state_v4(&vote_pubkey, &vote_init, &clock);
        assert_authorized_voter_is_locked_within_epoch(&mut vote_state, &original_voter);
    }

    #[test]
    fn test_get_and_update_authorized_voter_v3() {
        let original_voter = Pubkey::new_unique();
        let mut vote_state = VoteStateV3::new(
            &VoteInit {
                node_pubkey: original_voter,
                authorized_voter: original_voter,
                authorized_withdrawer: original_voter,
                commission: 0,
            },
            &Clock::default(),
        );

        assert_eq!(vote_state.authorized_voters().len(), 1);
        assert_eq!(
            *vote_state.authorized_voters().first().unwrap().1,
            original_voter
        );

        // If no new authorized voter was set, the same authorized voter
        // is locked into the next epoch
        assert_eq!(
            vote_state.get_and_update_authorized_voter(1).unwrap(),
            original_voter
        );

        // Try to get the authorized voter for epoch 5, implies
        // the authorized voter for epochs 1-4 were unchanged
        assert_eq!(
            vote_state.get_and_update_authorized_voter(5).unwrap(),
            original_voter
        );

        // Authorized voter for expired epoch 0..5 should have been
        // purged and no longer queryable
        assert_eq!(vote_state.authorized_voters().len(), 1);
        for i in 0..5 {
            assert!(vote_state
                .authorized_voters()
                .get_authorized_voter(i)
                .is_none());
        }

        // Set an authorized voter change at slot 7
        let new_authorized_voter = Pubkey::new_unique();
        vote_state
            .set_new_authorized_voter(&new_authorized_voter, 5, 7, |_| Ok(()))
            .unwrap();

        // Try to get the authorized voter for epoch 6, unchanged
        assert_eq!(
            vote_state.get_and_update_authorized_voter(6).unwrap(),
            original_voter
        );

        // Try to get the authorized voter for epoch 7 and onwards, should
        // be the new authorized voter
        for i in 7..10 {
            assert_eq!(
                vote_state.get_and_update_authorized_voter(i).unwrap(),
                new_authorized_voter
            );
        }
        assert_eq!(vote_state.authorized_voters().len(), 1);
    }

    // v4 purging retains one extra epoch compared to v3.
    // Besides that, the functionality should be the same.
    #[test]
    fn test_get_and_update_authorized_voter_v4() {
        let vote_pubkey = Pubkey::new_unique();
        let original_voter = Pubkey::new_unique();
        let mut vote_state = create_new_vote_state_v4(
            &vote_pubkey,
            &VoteInit {
                node_pubkey: original_voter,
                authorized_voter: original_voter,
                authorized_withdrawer: original_voter,
                commission: 0,
            },
            &Clock::default(),
        );

        // Run the same exercise as the v3 test to start.

        assert_eq!(vote_state.authorized_voters().len(), 1);
        assert_eq!(
            *vote_state.authorized_voters().first().unwrap().1,
            original_voter
        );

        // If no new authorized voter was set, the same authorized voter
        // is locked into the next epoch
        assert_eq!(
            vote_state.get_and_update_authorized_voter(1).unwrap(),
            original_voter
        );

        // Try to get the authorized voter for epoch 5, implies
        // the authorized voter for epochs 1-4 were unchanged
        assert_eq!(
            vote_state.get_and_update_authorized_voter(5).unwrap(),
            original_voter
        );

        // Just like with the v3 tests, authorized voters for epochs 0..5 should
        // be purged, but only because we didn't cache an entry for current - 1.
        assert_eq!(vote_state.authorized_voters().len(), 1);
        for i in 0..5 {
            assert!(vote_state
                .authorized_voters()
                .get_authorized_voter(i)
                .is_none());
        }

        // Say we're in epoch 7. Cache entries for both epochs 6 and 7.
        assert_eq!(
            vote_state.get_and_update_authorized_voter(6).unwrap(),
            original_voter
        );
        assert_eq!(
            vote_state.get_and_update_authorized_voter(7).unwrap(),
            original_voter
        );

        // Now we should have length 2.
        assert_eq!(vote_state.authorized_voters().len(), 2);

        // 0..=5 should still be purged.
        for i in 0..=5 {
            assert!(vote_state
                .authorized_voters()
                .get_authorized_voter(i)
                .is_none());
        }

        // Set an authorized voter change at epoch 9.
        let new_authorized_voter = Pubkey::new_unique();
        vote_state
            .set_new_authorized_voter(&new_authorized_voter, 7, 9, |_| Ok(()))
            .unwrap();

        // Try to get the authorized voter for epoch 8, unchanged
        assert_eq!(
            vote_state.get_and_update_authorized_voter(8).unwrap(),
            original_voter
        );

        // Try to get the authorized voter for epoch 9 and onwards, should
        // be the new authorized voter
        for i in 9..12 {
            assert_eq!(
                vote_state.get_and_update_authorized_voter(i).unwrap(),
                new_authorized_voter
            );
        }
        assert_eq!(vote_state.authorized_voters().len(), 2);

        // If we skip a few epochs ahead, only the current epoch is retained.
        assert_eq!(
            vote_state.get_and_update_authorized_voter(15).unwrap(),
            new_authorized_voter
        );
        assert_eq!(vote_state.authorized_voters().len(), 1);
    }

    #[test_case(
        VoteStateV3::size_of(),
        get_max_sized_vote_state_v3(),
        |vote_state, data| {
            let versioned = VoteStateVersions::new_v3(vote_state);
            VoteStateV3::serialize(&versioned, data).unwrap();
        };
        "VoteStateV3"
    )]
    #[test_case(
        VoteStateV4::size_of(),
        get_max_sized_vote_state_v4(),
        |vote_state, data| {
            let versioned = VoteStateVersions::new_v4(vote_state);
            VoteStateV4::serialize(&versioned, data).unwrap();
        };
        "VoteStateV4"
    )]
    fn test_vote_state_max_size<T: Clone + VoteStateHandle>(
        max_size: usize,
        mut vote_state: T,
        verify_serialize: fn(T, &mut [u8]),
    ) {
        let mut max_sized_data = vec![0; max_size];
        let (start_leader_schedule_epoch, _) = vote_state.authorized_voters().last().unwrap();
        let start_current_epoch =
            start_leader_schedule_epoch - MAX_LEADER_SCHEDULE_EPOCH_OFFSET + 1;

        for i in start_current_epoch..start_current_epoch + 2 * MAX_LEADER_SCHEDULE_EPOCH_OFFSET {
            vote_state
                .set_new_authorized_voter(
                    &Pubkey::new_unique(),
                    i,
                    i + MAX_LEADER_SCHEDULE_EPOCH_OFFSET,
                    |_| Ok(()),
                )
                .unwrap();

            verify_serialize(vote_state.clone(), &mut max_sized_data);
        }
    }

    #[test_case(VoteStateV3::default() ; "VoteStateV3")]
    #[test_case(VoteStateV4::default() ; "VoteStateV4")]
    fn test_vote_state_epoch_credits<T: VoteStateHandle>(mut vote_state: T) {
        assert_eq!(vote_state.credits(), 0);
        assert_eq!(vote_state.epoch_credits().clone(), vec![]);

        let mut expected = vec![];
        let mut credits = 0;
        let epochs = (MAX_EPOCH_CREDITS_HISTORY + 2) as u64;
        for epoch in 0..epochs {
            for _j in 0..epoch {
                vote_state.increment_credits(epoch, 1);
                credits += 1;
            }
            expected.push((epoch, credits, credits - epoch));
        }

        while expected.len() > MAX_EPOCH_CREDITS_HISTORY {
            expected.remove(0);
        }

        assert_eq!(vote_state.credits(), credits);
        assert_eq!(vote_state.epoch_credits().clone(), expected);
    }

    #[test_case(VoteStateV3::default() ; "VoteStateV3")]
    #[test_case(VoteStateV4::default() ; "VoteStateV4")]
    fn test_vote_state_epoch0_no_credits<T: VoteStateHandle>(mut vote_state: T) {
        assert_eq!(vote_state.epoch_credits().len(), 0);
        vote_state.increment_credits(1, 1);
        assert_eq!(vote_state.epoch_credits().len(), 1);

        vote_state.increment_credits(2, 1);
        assert_eq!(vote_state.epoch_credits().len(), 2);
    }

    #[test_case(VoteStateV3::default() ; "VoteStateV3")]
    #[test_case(VoteStateV4::default() ; "VoteStateV4")]
    fn test_vote_state_increment_credits<T: VoteStateHandle>(mut vote_state: T) {
        let credits = (MAX_EPOCH_CREDITS_HISTORY + 2) as u64;
        for i in 0..credits {
            vote_state.increment_credits(i, 1);
        }
        assert_eq!(vote_state.credits(), credits);
        assert!(vote_state.epoch_credits().len() <= MAX_EPOCH_CREDITS_HISTORY);
    }

    #[test_case(VoteStateV3::default() ; "VoteStateV3")]
    #[test_case(VoteStateV4::default() ; "VoteStateV4")]
    fn test_vote_process_timestamp<T: VoteStateHandle>(mut vote_state: T) {
        let (slot, timestamp) = (15, 1_575_412_285);
        vote_state.set_last_timestamp(BlockTimestamp { slot, timestamp });

        assert_eq!(
            vote_state.process_timestamp(slot - 1, timestamp + 1),
            Err(VoteError::TimestampTooOld)
        );
        assert_eq!(
            vote_state.last_timestamp(),
            &BlockTimestamp { slot, timestamp }
        );
        assert_eq!(
            vote_state.process_timestamp(slot + 1, timestamp - 1),
            Err(VoteError::TimestampTooOld)
        );
        assert_eq!(
            vote_state.process_timestamp(slot, timestamp + 1),
            Err(VoteError::TimestampTooOld)
        );
        assert_eq!(vote_state.process_timestamp(slot, timestamp), Ok(()));
        assert_eq!(
            vote_state.last_timestamp(),
            &BlockTimestamp { slot, timestamp }
        );
        assert_eq!(vote_state.process_timestamp(slot + 1, timestamp), Ok(()));
        assert_eq!(
            vote_state.last_timestamp(),
            &BlockTimestamp {
                slot: slot + 1,
                timestamp
            }
        );
        assert_eq!(
            vote_state.process_timestamp(slot + 2, timestamp + 1),
            Ok(())
        );
        assert_eq!(
            vote_state.last_timestamp(),
            &BlockTimestamp {
                slot: slot + 2,
                timestamp: timestamp + 1
            }
        );

        // Test initial vote
        vote_state.set_last_timestamp(BlockTimestamp::default());
        assert_eq!(vote_state.process_timestamp(0, timestamp), Ok(()));
    }

    enum ExpectedVoteStateVersion {
        V1_14_11,
        V3,
    }

    fn init_vote_account_state_v3_and_assert(
        vote_pubkey: Pubkey,
        vote_account: AccountSharedData,
        vote_init: &VoteInit,
        clock: &Clock,
        rent: Rent,
        expected_version: ExpectedVoteStateVersion,
    ) {
        let transaction_context = mock_transaction_context(vote_pubkey, vote_account, rent);
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();
        let mut vote_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        // Initialize.
        VoteStateHandler::init_vote_account_state(
            &mut vote_account,
            vote_init,
            clock,
            VoteStateTargetVersion::V3,
        )
        .unwrap();

        let vote_state_versions = vote_account.get_state::<VoteStateVersions>().unwrap();

        match expected_version {
            ExpectedVoteStateVersion::V1_14_11 => {
                assert!(matches!(
                    vote_state_versions,
                    VoteStateVersions::V1_14_11(_)
                ));
                assert!(!vote_state_versions.is_uninitialized());

                // Verify fields.
                if let VoteStateVersions::V1_14_11(v1_14_11) = vote_state_versions {
                    assert_eq!(v1_14_11.node_pubkey, vote_init.node_pubkey);
                    assert_eq!(
                        v1_14_11.authorized_voters.get_authorized_voter(0),
                        Some(vote_init.authorized_voter)
                    );
                    assert_eq!(
                        v1_14_11.authorized_withdrawer,
                        vote_init.authorized_withdrawer
                    );
                    assert_eq!(v1_14_11.commission, vote_init.commission);
                } else {
                    panic!("should be v1_14_11");
                }
            }
            ExpectedVoteStateVersion::V3 => {
                assert!(matches!(vote_state_versions, VoteStateVersions::V3(_)));
                assert!(!vote_state_versions.is_uninitialized());

                // Verify fields.
                if let VoteStateVersions::V3(v3) = vote_state_versions {
                    assert_eq!(v3.node_pubkey, vote_init.node_pubkey);
                    assert_eq!(
                        v3.authorized_voters.get_authorized_voter(0),
                        Some(vote_init.authorized_voter)
                    );
                    assert_eq!(v3.authorized_withdrawer, vote_init.authorized_withdrawer);
                    assert_eq!(v3.commission, vote_init.commission);
                } else {
                    panic!("should be v3");
                }
            }
        }
    }

    fn init_vote_account_state_v4_and_assert(
        vote_pubkey: Pubkey,
        vote_account: AccountSharedData,
        vote_init: &VoteInit,
        clock: &Clock,
        rent: Rent,
        expected_result: Result<(), InstructionError>,
    ) {
        let transaction_context = mock_transaction_context(vote_pubkey, vote_account, rent);
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();
        let mut vote_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        // Initialize.
        let result = VoteStateHandler::init_vote_account_state(
            &mut vote_account,
            vote_init,
            clock,
            VoteStateTargetVersion::V4,
        );
        assert_eq!(result, expected_result);

        if result.is_ok() {
            let vote_state_versions = vote_account.get_state::<VoteStateVersions>().unwrap();
            assert!(matches!(vote_state_versions, VoteStateVersions::V4(_)));
            assert!(!vote_state_versions.is_uninitialized());

            // Verify fields.
            if let VoteStateVersions::V4(v4) = vote_state_versions {
                assert_eq!(v4.node_pubkey, vote_init.node_pubkey);
                assert_eq!(
                    v4.authorized_voters.get_authorized_voter(clock.epoch),
                    Some(vote_init.authorized_voter)
                );
                assert_eq!(v4.authorized_withdrawer, vote_init.authorized_withdrawer);
                assert_eq!(
                    v4.inflation_rewards_commission_bps,
                    (vote_init.commission as u16) * 100
                );

                // SIMD-0185 fields
                assert_eq!(v4.inflation_rewards_collector, vote_pubkey);
                assert_eq!(v4.block_revenue_collector, vote_init.node_pubkey);
                assert_eq!(
                    v4.block_revenue_commission_bps,
                    DEFAULT_BLOCK_REVENUE_COMMISSION_BPS
                );

                // Fields that should be default
                assert_eq!(v4.pending_delegator_rewards, 0);
                assert_eq!(v4.bls_pubkey_compressed, None);
                assert!(v4.votes.is_empty());
                assert_eq!(v4.root_slot, None);
                assert!(v4.epoch_credits.is_empty());
                assert_eq!(v4.last_timestamp, BlockTimestamp::default());
            } else {
                panic!("should be v4");
            }
        }
    }

    fn deinit_vote_account_state_v3_and_assert(
        vote_pubkey: Pubkey,
        vote_account: AccountSharedData,
        rent: Rent,
        expected_version: ExpectedVoteStateVersion,
    ) {
        let transaction_context = mock_transaction_context(vote_pubkey, vote_account, rent.clone());
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();
        let mut vote_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        // Deinitialize.
        VoteStateHandler::deinitialize_vote_account_state(
            &mut vote_account,
            VoteStateTargetVersion::V3,
        )
        .unwrap();

        let vote_state_versions = vote_account.get_state::<VoteStateVersions>().unwrap();

        match expected_version {
            ExpectedVoteStateVersion::V1_14_11 => {
                assert!(matches!(
                    vote_state_versions,
                    VoteStateVersions::V1_14_11(_)
                ));
                assert!(vote_state_versions.is_uninitialized());
            }
            ExpectedVoteStateVersion::V3 => {
                assert!(matches!(vote_state_versions, VoteStateVersions::V3(_)));
                assert!(vote_state_versions.is_uninitialized());
            }
        }
    }

    fn deinit_vote_account_state_v4_and_assert(
        vote_pubkey: Pubkey,
        vote_account: AccountSharedData,
        rent: Rent,
    ) {
        let transaction_context = mock_transaction_context(vote_pubkey, vote_account, rent.clone());
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();
        let mut vote_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        // Deinitialize.
        VoteStateHandler::deinitialize_vote_account_state(
            &mut vote_account,
            VoteStateTargetVersion::V4,
        )
        .unwrap();

        // Per SIMD-0185, V4 should completely zero out the account data.
        let account_data = vote_account.get_data();
        assert!(account_data.iter().all(|&b| b == 0),);

        // Vote account was completely zeroed, so this should deserialize as
        // uninitialized.
        let vote_state_versions = vote_account.get_state::<VoteStateVersions>().unwrap();
        assert!(matches!(
            vote_state_versions,
            VoteStateVersions::Uninitialized
        ));
        assert!(vote_state_versions.is_uninitialized());
    }

    #[test]
    fn test_init_vote_account_state_v3() {
        let vote_pubkey = Pubkey::new_unique();
        let vote_init = VoteInit {
            node_pubkey: Pubkey::new_unique(),
            authorized_voter: Pubkey::new_unique(),
            authorized_withdrawer: Pubkey::new_unique(),
            commission: 5,
        };
        let clock = Clock::default();
        let rent = Rent::default();

        // First create a vote account that's too small for v3.
        let v1_14_11_size = VoteState1_14_11::size_of();
        let lamports = rent.minimum_balance(v1_14_11_size);
        let vote_account = AccountSharedData::new(lamports, v1_14_11_size, &id());

        // Initialize - should default to v1_14_11.
        init_vote_account_state_v3_and_assert(
            vote_pubkey,
            vote_account,
            &vote_init,
            &clock,
            rent.clone(),
            ExpectedVoteStateVersion::V1_14_11,
        );

        // Create a vote account that's too small for v3, but has enough
        // lamports for resize.
        let v3_size = VoteStateV3::size_of();
        let lamports = rent.minimum_balance(v3_size);
        let vote_account = AccountSharedData::new(lamports, v1_14_11_size, &id());

        // Initialize - should resize and create v3.
        init_vote_account_state_v3_and_assert(
            vote_pubkey,
            vote_account,
            &vote_init,
            &clock,
            rent.clone(),
            ExpectedVoteStateVersion::V3,
        );

        // Now create a vote account that's large enough for v3.
        let lamports = rent.minimum_balance(v3_size);
        let vote_account = AccountSharedData::new(lamports, v3_size, &id());

        // Initialize - should create v3.
        init_vote_account_state_v3_and_assert(
            vote_pubkey,
            vote_account,
            &vote_init,
            &clock,
            rent,
            ExpectedVoteStateVersion::V3,
        );
    }

    #[test]
    fn test_init_vote_account_state_v4() {
        let vote_pubkey = Pubkey::new_unique();
        let vote_init = VoteInit {
            node_pubkey: Pubkey::new_unique(),
            authorized_voter: Pubkey::new_unique(),
            authorized_withdrawer: Pubkey::new_unique(),
            commission: 5,
        };
        let clock = Clock::default();
        let rent = Rent::default();

        // First create a vote account that's too small for v4.
        let v1_14_11_size = VoteState1_14_11::size_of();
        let lamports = rent.minimum_balance(v1_14_11_size);
        let vote_account = AccountSharedData::new(lamports, v1_14_11_size, &id());

        // Initialize - should fail.
        init_vote_account_state_v4_and_assert(
            vote_pubkey,
            vote_account,
            &vote_init,
            &clock,
            rent.clone(),
            Err(InstructionError::AccountNotRentExempt),
        );

        // Create a vote account that's too small for v4, but has enough
        // lamports for resize.
        let v4_size = VoteStateV4::size_of();
        let lamports = rent.minimum_balance(v4_size);
        let vote_account = AccountSharedData::new(lamports, v1_14_11_size, &id());

        // Initialize - should resize and create v4.
        init_vote_account_state_v4_and_assert(
            vote_pubkey,
            vote_account,
            &vote_init,
            &clock,
            rent.clone(),
            Ok(()),
        );

        // Now create a vote account that's large enough for v4.
        let lamports = rent.minimum_balance(v4_size);
        let vote_account = AccountSharedData::new(lamports, v4_size, &id());

        // Initialize - should create v4.
        init_vote_account_state_v4_and_assert(
            vote_pubkey,
            vote_account,
            &vote_init,
            &clock,
            rent,
            Ok(()),
        );
    }

    #[test]
    fn test_deinitialize_vote_account_state_v3() {
        let vote_pubkey = Pubkey::new_unique();
        let rent = Rent::default();

        // First create a vote account that's too small for v3.
        let v1_14_11_size = VoteState1_14_11::size_of();
        let lamports = rent.minimum_balance(v1_14_11_size);
        let vote_account = AccountSharedData::new(lamports, v1_14_11_size, &id());

        // Deinitialize - should default to v1_14_11.
        deinit_vote_account_state_v3_and_assert(
            vote_pubkey,
            vote_account,
            rent.clone(),
            ExpectedVoteStateVersion::V1_14_11,
        );

        // Create a vote account that's too small for v3, but has enough
        // lamports for resize.
        let v3_size = VoteStateV3::size_of();
        let lamports = rent.minimum_balance(v3_size);
        let vote_account = AccountSharedData::new(lamports, v1_14_11_size, &id());

        // Deinitialize - should resize and create v3.
        deinit_vote_account_state_v3_and_assert(
            vote_pubkey,
            vote_account,
            rent.clone(),
            ExpectedVoteStateVersion::V3,
        );

        // Now create a vote account that's large enough for v3.
        let lamports = rent.minimum_balance(v3_size);
        let vote_account = AccountSharedData::new(lamports, v3_size, &id());

        // Deinitialize - should create v3.
        deinit_vote_account_state_v3_and_assert(
            vote_pubkey,
            vote_account,
            rent,
            ExpectedVoteStateVersion::V3,
        );
    }

    #[test]
    fn test_deinitialize_vote_account_state_v4() {
        let vote_pubkey = Pubkey::new_unique();
        let rent = Rent::default();

        // First create a vote account that's too small for v4.
        let v1_14_11_size = VoteState1_14_11::size_of();
        let lamports = rent.minimum_balance(v1_14_11_size);
        let vote_account = AccountSharedData::new(lamports, v1_14_11_size, &id());

        // Deinitialize - should fail.
        deinit_vote_account_state_v4_and_assert(vote_pubkey, vote_account, rent.clone());

        // Create a vote account that's too small for v4, but has enough
        // lamports for resize.
        let v4_size = VoteStateV4::size_of();
        let lamports = rent.minimum_balance(v4_size);
        let vote_account = AccountSharedData::new(lamports, v1_14_11_size, &id());

        // Deinitialize - should resize and create v4.
        deinit_vote_account_state_v4_and_assert(vote_pubkey, vote_account, rent.clone());

        // Now create a vote account that's large enough for v4.
        let lamports = rent.minimum_balance(v4_size);
        let vote_account = AccountSharedData::new(lamports, v4_size, &id());

        // Deinitialize - should create v4.
        deinit_vote_account_state_v4_and_assert(vote_pubkey, vote_account, rent);
    }

    #[test]
    fn test_v4_commission_basis_points() {
        // Test that commission is properly converted to basis points
        // (multiplied by 100), as per SIMD-0185.
        let mut handler = VoteStateHandler::new_v4(VoteStateV4::default());

        for (input, expected) in [
            (0, 0),
            (1, 100),
            (5, 500),
            (10, 1_000),
            (25, 2_500),
            (50, 5_000),
            (75, 7_500),
            (100, 10_000),
            (255, 25_500),
        ] {
            handler.set_commission(input);
            assert_eq!(handler.commission(), input);

            // Verify the internal V4 state has the correct basis points.
            let vote_state_v4 = handler.as_ref_v4();
            assert_eq!(vote_state_v4.inflation_rewards_commission_bps, expected);
        }
    }

    #[test]
    fn test_v4_conversion_from_all_versions() {
        // Test conversion from all vote state versions to v4 per SIMD-0185.
        let vote_pubkey = Pubkey::new_unique();
        let node_pubkey = Pubkey::new_unique();
        let authorized_voter = Pubkey::new_unique();
        let authorized_withdrawer = Pubkey::new_unique();
        let commission = 42u8;
        let votes = vec![Lockout::new(100)];
        let votes_deque: VecDeque<Lockout> = votes.iter().cloned().collect();
        let epoch_credits = vec![(5, 200, 100)];
        let root_slot = Some(50);

        // Helper to verify v4 defaults per SIMD-0185.
        let verify_v4_defaults = |v4_state: &VoteStateV4, expected_commission_bps: u16| {
            assert_eq!(
                v4_state.inflation_rewards_commission_bps,
                expected_commission_bps
            );
            assert_eq!(v4_state.inflation_rewards_collector, vote_pubkey);
            assert_eq!(v4_state.block_revenue_collector, node_pubkey);
            assert_eq!(
                v4_state.block_revenue_commission_bps,
                DEFAULT_BLOCK_REVENUE_COMMISSION_BPS
            );
            assert_eq!(v4_state.pending_delegator_rewards, 0);
            assert_eq!(v4_state.bls_pubkey_compressed, None);
        };

        // V0_23_5
        // NOTE: On target_os = "solana", VoteState0_23_5::deserialize should
        // fail, but we can't replicate that in these unit tests. This should
        // be tested in the program's processor tests.

        // V1_14_11
        {
            let vote_state_v2 = VoteState1_14_11 {
                node_pubkey,
                authorized_withdrawer,
                commission,
                authorized_voters: AuthorizedVoters::new(0, authorized_voter),
                votes: votes_deque.clone(),
                epoch_credits: epoch_credits.clone(),
                root_slot,
                ..VoteState1_14_11::default()
            };

            let versioned = VoteStateVersions::V1_14_11(Box::new(vote_state_v2.clone()));
            let vote_state_v4 = try_convert_to_vote_state_v4(versioned, &vote_pubkey).unwrap();

            // Compare fields that were already present in V1_14_11.
            assert_eq!(vote_state_v4.node_pubkey, node_pubkey);
            assert_eq!(vote_state_v4.authorized_withdrawer, authorized_withdrawer);
            assert_eq!(
                vote_state_v4.authorized_voters,
                vote_state_v2.authorized_voters
            );
            assert_eq!(vote_state_v4.epoch_credits, epoch_credits);
            assert_eq!(vote_state_v4.root_slot, root_slot);
            assert_eq!(vote_state_v4.votes.len(), vote_state_v2.votes.len());
            for (v4_vote, v2_vote) in vote_state_v4.votes.iter().zip(vote_state_v2.votes.iter()) {
                assert_eq!(v4_vote.lockout, *v2_vote);
            }

            // Verify SIMD-0185 defaults.
            verify_v4_defaults(&vote_state_v4, (commission as u16) * 100);
        }

        // V3
        {
            let vote_init = VoteInit {
                node_pubkey,
                authorized_voter,
                authorized_withdrawer,
                commission,
            };
            let mut vote_state_v3 = VoteStateV3::new(&vote_init, &Clock::default());
            for lockout in &votes {
                vote_state_v3.votes.push_back(LandedVote {
                    latency: 1,
                    lockout: *lockout,
                });
            }
            vote_state_v3.epoch_credits = epoch_credits.clone();
            vote_state_v3.root_slot = root_slot;

            let versioned = VoteStateVersions::V3(Box::new(vote_state_v3.clone()));
            let vote_state_v4 = try_convert_to_vote_state_v4(versioned, &vote_pubkey).unwrap();

            // Compare fields that were already present in V3.
            assert_eq!(vote_state_v4.node_pubkey, node_pubkey);
            assert_eq!(vote_state_v4.authorized_withdrawer, authorized_withdrawer);
            assert_eq!(
                vote_state_v4.authorized_voters,
                vote_state_v3.authorized_voters
            );
            assert_eq!(vote_state_v4.epoch_credits, epoch_credits);
            assert_eq!(vote_state_v4.root_slot, root_slot);
            assert_eq!(vote_state_v4.last_timestamp, vote_state_v3.last_timestamp);
            assert_eq!(vote_state_v4.votes, vote_state_v3.votes);

            // Verify SIMD-0185 defaults.
            verify_v4_defaults(&vote_state_v4, (commission as u16) * 100);
        }

        // V4
        {
            let mut initial_vote_state_v4 = VoteStateV4 {
                node_pubkey,
                authorized_withdrawer,
                inflation_rewards_commission_bps: 1234,
                block_revenue_commission_bps: 5678,
                inflation_rewards_collector: Pubkey::new_unique(),
                block_revenue_collector: Pubkey::new_unique(),
                pending_delegator_rewards: 999,
                bls_pubkey_compressed: Some([42; BLS_PUBLIC_KEY_COMPRESSED_SIZE]),
                authorized_voters: AuthorizedVoters::new(0, authorized_voter),
                epoch_credits: epoch_credits.clone(),
                root_slot,
                ..VoteStateV4::default()
            };
            for lockout in &votes {
                initial_vote_state_v4.votes.push_back(LandedVote {
                    latency: 1,
                    lockout: *lockout,
                });
            }

            let versioned = VoteStateVersions::V4(Box::new(initial_vote_state_v4.clone()));
            let vote_state_v4 = try_convert_to_vote_state_v4(versioned, &vote_pubkey).unwrap();

            // Should be an exact copy.
            assert_eq!(vote_state_v4, initial_vote_state_v4);
        }
    }

    #[test]
    fn test_v3_v4_size_equality() {
        let v3_size = VoteStateV3::size_of();
        let v4_size = VoteStateV4::size_of();
        assert_eq!(v3_size, v4_size, "V3 and V4 should have the same size");
    }

    #[test_case(
        VoteState1_14_11::size_of(),
        false,
        Err(InstructionError::AccountNotRentExempt);
        "v1_14_11 size insufficient lamports - no fallback"
    )]
    #[test_case(
        VoteState1_14_11::size_of(),
        true,
        Ok(());
        "v1_14_11 size sufficient lamports - resize and succeed"
    )]
    #[test_case(
        VoteStateV4::size_of(),
        true,
        Ok(());
        "v4 properly sized with rent-exempt lamports - succeed"
    )]
    fn test_v4_resize_behavior(
        account_size: usize,
        sufficient_lamports: bool,
        expected_result: Result<(), InstructionError>,
    ) {
        let vote_pubkey = Pubkey::new_unique();
        let node_pubkey = Pubkey::new_unique();
        let authorized_voter = Pubkey::new_unique();
        let authorized_withdrawer = Pubkey::new_unique();
        let rent = Rent::default();

        let vote_init = VoteInit {
            node_pubkey,
            authorized_voter,
            authorized_withdrawer,
            commission: 42,
        };

        let lamports = if sufficient_lamports {
            rent.minimum_balance(VoteStateV4::size_of())
        } else {
            rent.minimum_balance(account_size)
        };

        let mut vote_account = AccountSharedData::new(lamports, account_size, &id());
        vote_account.set_data_from_slice(&vec![0; account_size]);

        let transaction_context = mock_transaction_context(vote_pubkey, vote_account, rent.clone());
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();
        let mut vote_account_borrowed = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        let result = VoteStateHandler::init_vote_account_state(
            &mut vote_account_borrowed,
            &vote_init,
            &Clock::default(),
            VoteStateTargetVersion::V4,
        );

        assert_eq!(result, expected_result);
    }
}
