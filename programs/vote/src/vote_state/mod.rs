//! Vote state, vote program
//! Receive and processes votes from validators

#[cfg(feature = "dev-context-only-utils")]
pub mod handler;
#[cfg(not(feature = "dev-context-only-utils"))]
pub(crate) mod handler;

pub use solana_vote_interface::state::{vote_state_versions::*, *};
use {
    handler::{VoteStateHandle, VoteStateHandler, VoteStateTargetVersion},
    log::*,
    solana_account::{AccountSharedData, WritableAccount},
    solana_clock::{Clock, Epoch, Slot},
    solana_epoch_schedule::EpochSchedule,
    solana_hash::Hash,
    solana_instruction::error::InstructionError,
    solana_pubkey::Pubkey,
    solana_rent::Rent,
    solana_slot_hashes::SlotHash,
    solana_transaction_context::{
        instruction::InstructionContext, instruction_accounts::BorrowedInstructionAccount,
        IndexOfAccount,
    },
    solana_vote_interface::{error::VoteError, program::id},
    std::{
        cmp::Ordering,
        collections::{HashSet, VecDeque},
    },
};

// Switch that preserves old behavior before vote state v4 feature gate.
// This should be cleaned up when vote state v4 is activated.
enum PreserveBehaviorInHandlerHelper {
    V3 { check_initialized: bool },
    V4,
}

impl PreserveBehaviorInHandlerHelper {
    fn new(target_version: VoteStateTargetVersion, check_initialized: bool) -> Self {
        match target_version {
            VoteStateTargetVersion::V3 => Self::V3 { check_initialized },
            VoteStateTargetVersion::V4 => Self::V4,
        }
    }
}

fn get_vote_state_handler_checked(
    vote_account: &BorrowedInstructionAccount,
    preserve_behavior: PreserveBehaviorInHandlerHelper,
) -> Result<VoteStateHandler, InstructionError> {
    match preserve_behavior {
        PreserveBehaviorInHandlerHelper::V3 { check_initialized } => {
            // Existing flow before v4 feature gate activation:
            // 1. Deserialize as `VoteState3`, converting during deserialization
            // 2. Check for uninitialized
            //
            // Some callsites would deserialize without checking initialization
            // status, hence the nested `check_initialized` switch.
            let vote_state = VoteStateV3::deserialize(vote_account.get_data())?;
            if check_initialized && vote_state.is_uninitialized() {
                return Err(InstructionError::UninitializedAccount);
            }
            Ok(VoteStateHandler::new_v3(vote_state))
        }
        PreserveBehaviorInHandlerHelper::V4 => {
            // New flow after v4 feature gate activation:
            // 1. Deserialize as `VoteStateVersions`
            // 2. Check for uninitialized
            // 3. Convert
            let versioned = VoteStateVersions::deserialize(vote_account.get_data())?;
            if versioned.is_uninitialized() {
                return Err(InstructionError::UninitializedAccount);
            }
            let vote_state =
                handler::try_convert_to_vote_state_v4(versioned, vote_account.get_key())?;
            Ok(VoteStateHandler::new_v4(vote_state))
        }
    }
}

/// Checks the proposed vote state with the current and
/// slot hashes, making adjustments to the root / filtering
/// votes as needed.
fn check_and_filter_proposed_vote_state(
    vote_state: &VoteStateHandler,
    proposed_lockouts: &mut VecDeque<Lockout>,
    proposed_root: &mut Option<Slot>,
    proposed_hash: Hash,
    slot_hashes: &[(Slot, Hash)],
) -> Result<(), VoteError> {
    if proposed_lockouts.is_empty() {
        return Err(VoteError::EmptySlots);
    }

    let last_proposed_slot = proposed_lockouts
        .back()
        .expect("must be nonempty, checked above")
        .slot();

    // If the proposed state is not new enough, return
    if let Some(last_vote_slot) = vote_state.votes().back().map(|lockout| lockout.slot()) {
        if last_proposed_slot <= last_vote_slot {
            return Err(VoteError::VoteTooOld);
        }
    }

    if slot_hashes.is_empty() {
        return Err(VoteError::SlotsMismatch);
    }
    let earliest_slot_hash_in_history = slot_hashes.last().unwrap().0;

    // Check if the proposed vote state is too old to be in the SlotHash history
    if last_proposed_slot < earliest_slot_hash_in_history {
        // If this is the last slot in the vote update, it must be in SlotHashes,
        // otherwise we have no way of confirming if the hash matches
        return Err(VoteError::VoteTooOld);
    }

    // Overwrite the proposed root if it is too old to be in the SlotHash history
    if let Some(root) = *proposed_root {
        // If the new proposed root `R` is less than the earliest slot hash in the history
        // such that we cannot verify whether the slot was actually was on this fork, set
        // the root to the latest vote in the vote state that's less than R. If no
        // votes from the vote state are less than R, use its root instead.
        if root < earliest_slot_hash_in_history {
            // First overwrite the proposed root with the vote state's root
            *proposed_root = vote_state.root_slot();

            // Then try to find the latest vote in vote state that's less than R
            for vote in vote_state.votes().iter().rev() {
                if vote.slot() <= root {
                    *proposed_root = Some(vote.slot());
                    break;
                }
            }
        }
    }

    // Index into the new proposed vote state's slots, starting with the root if it exists then
    // we use this mutable root to fold checking the root slot into the below loop
    // for performance
    let mut root_to_check = *proposed_root;
    let mut proposed_lockouts_index = 0;

    // index into the slot_hashes, starting at the oldest known
    // slot hash
    let mut slot_hashes_index = slot_hashes.len();

    let mut proposed_lockouts_indices_to_filter = vec![];

    // Note:
    //
    // 1) `proposed_lockouts` is sorted from oldest/smallest vote to newest/largest
    // vote, due to the way votes are applied to the vote state (newest votes
    // pushed to the back).
    //
    // 2) Conversely, `slot_hashes` is sorted from newest/largest vote to
    // the oldest/smallest vote
    //
    // We check every proposed lockout because have to ensure that every slot is actually part of
    // the history, not just the most recent ones
    while proposed_lockouts_index < proposed_lockouts.len() && slot_hashes_index > 0 {
        let proposed_vote_slot = if let Some(root) = root_to_check {
            root
        } else {
            proposed_lockouts[proposed_lockouts_index].slot()
        };
        if root_to_check.is_none()
            && proposed_lockouts_index > 0
            && proposed_vote_slot
                <= proposed_lockouts[proposed_lockouts_index.checked_sub(1).expect(
                    "`proposed_lockouts_index` is positive when checking `SlotsNotOrdered`",
                )]
                .slot()
        {
            return Err(VoteError::SlotsNotOrdered);
        }
        let ancestor_slot = slot_hashes[slot_hashes_index
            .checked_sub(1)
            .expect("`slot_hashes_index` is positive when computing `ancestor_slot`")]
        .0;

        // Find if this slot in the proposed vote state exists in the SlotHashes history
        // to confirm if it was a valid ancestor on this fork
        match proposed_vote_slot.cmp(&ancestor_slot) {
            Ordering::Less => {
                if slot_hashes_index == slot_hashes.len() {
                    // The vote slot does not exist in the SlotHashes history because it's too old,
                    // i.e. older than the oldest slot in the history.
                    if proposed_vote_slot >= earliest_slot_hash_in_history {
                        return Err(VoteError::AssertionFailed);
                    }
                    if !vote_state.contains_slot(proposed_vote_slot) && root_to_check.is_none() {
                        // If the vote slot is both:
                        // 1) Too old
                        // 2) Doesn't already exist in vote state
                        //
                        // Then filter it out
                        proposed_lockouts_indices_to_filter.push(proposed_lockouts_index);
                    }
                    if let Some(new_proposed_root) = root_to_check {
                        // 1. Because `root_to_check.is_some()`, then we know that
                        // we haven't checked the root yet in this loop, so
                        // `proposed_vote_slot` == `new_proposed_root` == `proposed_root`.
                        assert_eq!(new_proposed_root, proposed_vote_slot);
                        // 2. We know from the assert earlier in the function that
                        // `proposed_vote_slot < earliest_slot_hash_in_history`,
                        // so from 1. we know that `new_proposed_root < earliest_slot_hash_in_history`.
                        if new_proposed_root >= earliest_slot_hash_in_history {
                            return Err(VoteError::AssertionFailed);
                        }
                        root_to_check = None;
                    } else {
                        proposed_lockouts_index = proposed_lockouts_index.checked_add(1).expect(
                            "`proposed_lockouts_index` is bounded by `MAX_LOCKOUT_HISTORY` when \
                             `proposed_vote_slot` is too old to be in SlotHashes history",
                        );
                    }
                    continue;
                } else {
                    // If the vote slot is new enough to be in the slot history,
                    // but is not part of the slot history, then it must belong to another fork,
                    // which means this proposed vote state is invalid.
                    if root_to_check.is_some() {
                        return Err(VoteError::RootOnDifferentFork);
                    } else {
                        return Err(VoteError::SlotsMismatch);
                    }
                }
            }
            Ordering::Greater => {
                // Decrement `slot_hashes_index` to find newer slots in the SlotHashes history
                slot_hashes_index = slot_hashes_index.checked_sub(1).expect(
                    "`slot_hashes_index` is positive when finding newer slots in SlotHashes \
                     history",
                );
                continue;
            }
            Ordering::Equal => {
                // Once the slot in `proposed_lockouts` is found, bump to the next slot
                // in `proposed_lockouts` and continue. If we were checking the root,
                // start checking the vote state instead.
                if root_to_check.is_some() {
                    root_to_check = None;
                } else {
                    proposed_lockouts_index = proposed_lockouts_index.checked_add(1).expect(
                        "`proposed_lockouts_index` is bounded by `MAX_LOCKOUT_HISTORY` when match \
                         is found in SlotHashes history",
                    );
                    slot_hashes_index = slot_hashes_index.checked_sub(1).expect(
                        "`slot_hashes_index` is positive when match is found in SlotHashes history",
                    );
                }
            }
        }
    }

    if proposed_lockouts_index != proposed_lockouts.len() {
        // The last vote slot in the proposed vote state did not exist in SlotHashes
        return Err(VoteError::SlotsMismatch);
    }

    // This assertion must be true at this point because we can assume by now:
    // 1) proposed_lockouts_index == proposed_lockouts.len()
    // 2) last_proposed_slot >= earliest_slot_hash_in_history
    // 3) !proposed_lockouts.is_empty()
    //
    // 1) implies that during the last iteration of the loop above,
    // `proposed_lockouts_index` was equal to `proposed_lockouts.len() - 1`,
    // and was then incremented to `proposed_lockouts.len()`.
    // This means in that last loop iteration,
    // `proposed_vote_slot ==
    //  proposed_lockouts[proposed_lockouts.len() - 1] ==
    //  last_proposed_slot`.
    //
    // Then we know the last comparison `match proposed_vote_slot.cmp(&ancestor_slot)`
    // is equivalent to `match last_proposed_slot.cmp(&ancestor_slot)`. The result
    // of this match to increment `proposed_lockouts_index` must have been either:
    //
    // 1) The Equal case ran, in which case then we know this assertion must be true
    // 2) The Less case ran, and more specifically the case
    // `proposed_vote_slot < earliest_slot_hash_in_history` ran, which is equivalent to
    // `last_proposed_slot < earliest_slot_hash_in_history`, but this is impossible
    // due to assumption 3) above.
    assert_eq!(last_proposed_slot, slot_hashes[slot_hashes_index].0);

    if slot_hashes[slot_hashes_index].1 != proposed_hash {
        // This means the newest vote in the slot has a match that
        // doesn't match the expected hash for that slot on this
        // fork
        warn!(
            "{} dropped vote {:?} root {:?} failed to match hash {} {}",
            vote_state.node_pubkey(),
            proposed_lockouts,
            proposed_root,
            proposed_hash,
            slot_hashes[slot_hashes_index].1
        );
        return Err(VoteError::SlotHashMismatch);
    }

    // Filter out the irrelevant votes
    let mut proposed_lockouts_index = 0;
    let mut filter_votes_index = 0;
    proposed_lockouts.retain(|_lockout| {
        let should_retain = if filter_votes_index == proposed_lockouts_indices_to_filter.len() {
            true
        } else if proposed_lockouts_index == proposed_lockouts_indices_to_filter[filter_votes_index]
        {
            filter_votes_index = filter_votes_index.checked_add(1).unwrap();
            false
        } else {
            true
        };

        proposed_lockouts_index = proposed_lockouts_index.checked_add(1).expect(
            "`proposed_lockouts_index` is bounded by `MAX_LOCKOUT_HISTORY` when filtering out \
             irrelevant votes",
        );
        should_retain
    });

    Ok(())
}

fn check_slots_are_valid<T: VoteStateHandle>(
    vote_state: &T,
    vote_slots: &[Slot],
    vote_hash: &Hash,
    slot_hashes: &[(Slot, Hash)],
) -> Result<(), VoteError> {
    // index into the vote's slots, starting at the oldest
    // slot
    let mut i = 0;

    // index into the slot_hashes, starting at the oldest known
    // slot hash
    let mut j = slot_hashes.len();

    // Note:
    //
    // 1) `vote_slots` is sorted from oldest/smallest vote to newest/largest
    // vote, due to the way votes are applied to the vote state (newest votes
    // pushed to the back).
    //
    // 2) Conversely, `slot_hashes` is sorted from newest/largest vote to
    // the oldest/smallest vote
    while i < vote_slots.len() && j > 0 {
        // 1) increment `i` to find the smallest slot `s` in `vote_slots`
        // where `s` >= `last_voted_slot`
        if vote_state
            .last_voted_slot()
            .is_some_and(|last_voted_slot| vote_slots[i] <= last_voted_slot)
        {
            i = i
                .checked_add(1)
                .expect("`i` is bounded by `MAX_LOCKOUT_HISTORY` when finding larger slots");
            continue;
        }

        // 2) Find the hash for this slot `s`.
        if vote_slots[i] != slot_hashes[j.checked_sub(1).expect("`j` is positive")].0 {
            // Decrement `j` to find newer slots
            j = j
                .checked_sub(1)
                .expect("`j` is positive when finding newer slots");
            continue;
        }

        // 3) Once the hash for `s` is found, bump `s` to the next slot
        // in `vote_slots` and continue.
        i = i
            .checked_add(1)
            .expect("`i` is bounded by `MAX_LOCKOUT_HISTORY` when hash is found");
        j = j
            .checked_sub(1)
            .expect("`j` is positive when hash is found");
    }

    if j == slot_hashes.len() {
        // This means we never made it to steps 2) or 3) above, otherwise
        // `j` would have been decremented at least once. This means
        // there are not slots in `vote_slots` greater than `last_voted_slot`
        debug!(
            "{} dropped vote slots {:?}, vote hash: {:?} slot hashes:SlotHash {:?}, too old ",
            vote_state.node_pubkey(),
            vote_slots,
            vote_hash,
            slot_hashes
        );
        return Err(VoteError::VoteTooOld);
    }
    if i != vote_slots.len() {
        // This means there existed some slot for which we couldn't find
        // a matching slot hash in step 2)
        info!(
            "{} dropped vote slots {:?} failed to match slot hashes: {:?}",
            vote_state.node_pubkey(),
            vote_slots,
            slot_hashes,
        );
        return Err(VoteError::SlotsMismatch);
    }
    if &slot_hashes[j].1 != vote_hash {
        // This means the newest slot in the `vote_slots` has a match that
        // doesn't match the expected hash for that slot on this
        // fork
        warn!(
            "{} dropped vote slots {:?} failed to match hash {} {}",
            vote_state.node_pubkey(),
            vote_slots,
            vote_hash,
            slot_hashes[j].1
        );
        return Err(VoteError::SlotHashMismatch);
    }
    Ok(())
}

// Ensure `check_and_filter_proposed_vote_state(&)` runs on the slots in `new_state`
// before `process_new_vote_state()` is called

// This function should guarantee the following about `new_state`:
//
// 1) It's well ordered, i.e. the slots are sorted from smallest to largest,
// and the confirmations sorted from largest to smallest.
// 2) Confirmations `c` on any vote slot satisfy `0 < c <= MAX_LOCKOUT_HISTORY`
// 3) Lockouts are not expired by consecutive votes, i.e. for every consecutive
// `v_i`, `v_{i + 1}` satisfy `v_i.last_locked_out_slot() >= v_{i + 1}`.

// We also guarantee that compared to the current vote state, `new_state`
// introduces no rollback. This means:
//
// 1) The last slot in `new_state` is always greater than any slot in the
// current vote state.
//
// 2) From 1), this means that for every vote `s` in the current state:
//    a) If there exists an `s'` in `new_state` where `s.slot == s'.slot`, then
//    we must guarantee `s.confirmations <= s'.confirmations`
//
//    b) If there does not exist any such `s'` in `new_state`, then there exists
//    some `t` that is the smallest vote in `new_state` where `t.slot > s.slot`.
//    `t` must have expired/popped off s', so it must be guaranteed that
//    `s.last_locked_out_slot() < t`.

// Note these two above checks do not guarantee that the vote state being submitted
// is a vote state that could have been created by iteratively building a tower
// by processing one vote at a time. For instance, the tower:
//
// { slot 0, confirmations: 31 }
// { slot 1, confirmations: 30 }
//
// is a legal tower that could be submitted on top of a previously empty tower. However,
// there is no way to create this tower from the iterative process, because slot 1 would
// have to have at least one other slot on top of it, even if the first 30 votes were all
// popped off.
pub fn process_new_vote_state(
    vote_state: &mut VoteStateHandler,
    mut new_state: VecDeque<LandedVote>,
    new_root: Option<Slot>,
    timestamp: Option<i64>,
    epoch: Epoch,
    current_slot: Slot,
) -> Result<(), VoteError> {
    assert!(!new_state.is_empty());
    if new_state.len() > MAX_LOCKOUT_HISTORY {
        return Err(VoteError::TooManyVotes);
    }

    match (new_root, vote_state.root_slot()) {
        (Some(new_root), Some(current_root)) => {
            if new_root < current_root {
                return Err(VoteError::RootRollBack);
            }
        }
        (None, Some(_)) => {
            return Err(VoteError::RootRollBack);
        }
        _ => (),
    }

    let mut previous_vote: Option<&LandedVote> = None;

    // Check that all the votes in the new proposed state are:
    // 1) Strictly sorted from oldest to newest vote
    // 2) The confirmations are strictly decreasing
    // 3) Not zero confirmation votes
    for vote in &new_state {
        if vote.confirmation_count() == 0 {
            return Err(VoteError::ZeroConfirmations);
        } else if vote.confirmation_count() > MAX_LOCKOUT_HISTORY as u32 {
            return Err(VoteError::ConfirmationTooLarge);
        } else if let Some(new_root) = new_root {
            if vote.slot() <= new_root
                &&
                // This check is necessary because
                // https://github.com/ryoqun/solana/blob/df55bfb46af039cbc597cd60042d49b9d90b5961/core/src/consensus.rs#L120
                // always sets a root for even empty towers, which is then hard unwrapped here
                // https://github.com/ryoqun/solana/blob/df55bfb46af039cbc597cd60042d49b9d90b5961/core/src/consensus.rs#L776
                new_root != Slot::default()
            {
                return Err(VoteError::SlotSmallerThanRoot);
            }
        }

        if let Some(previous_vote) = previous_vote {
            if previous_vote.slot() >= vote.slot() {
                return Err(VoteError::SlotsNotOrdered);
            } else if previous_vote.confirmation_count() <= vote.confirmation_count() {
                return Err(VoteError::ConfirmationsNotOrdered);
            } else if vote.slot() > previous_vote.lockout.last_locked_out_slot() {
                return Err(VoteError::NewVoteStateLockoutMismatch);
            }
        }
        previous_vote = Some(vote);
    }

    // Find the first vote in the current vote state for a slot greater
    // than the new proposed root
    let mut current_vote_state_index: usize = 0;
    let mut new_vote_state_index = 0;

    // Accumulate credits earned by newly rooted slots
    let mut earned_credits = 0_u64;

    if let Some(new_root) = new_root {
        for current_vote in vote_state.votes() {
            // Find the first vote in the current vote state for a slot greater
            // than the new proposed root
            if current_vote.slot() <= new_root {
                earned_credits = earned_credits
                    .checked_add(vote_state.credits_for_vote_at_index(current_vote_state_index))
                    .expect("`earned_credits` does not overflow");
                current_vote_state_index = current_vote_state_index.checked_add(1).expect(
                    "`current_vote_state_index` is bounded by `MAX_LOCKOUT_HISTORY` when \
                     processing new root",
                );
                continue;
            }

            break;
        }
    }

    // For any slots newly added to the new vote state, the vote latency of that slot is not provided by the
    // vote instruction contents, but instead is computed from the actual latency of the vote
    // instruction. This prevents other validators from manipulating their own vote latencies within their vote states
    // and forcing the rest of the cluster to accept these possibly fraudulent latency values.  If the
    // timly_vote_credits feature is not enabled then vote latency is set to 0 for new votes.
    //
    // For any slot that is in both the new state and the current state, the vote latency of the new state is taken
    // from the current state.
    //
    // Thus vote latencies are set here for any newly vote-on slots when a vote instruction is received.
    // They are copied into the new vote state after every vote for already voted-on slots.
    // And when voted-on slots are rooted, the vote latencies stored in the vote state of all the rooted slots is used
    // to compute credits earned.
    // All validators compute the same vote latencies because all process the same vote instruction at the
    // same slot, and the only time vote latencies are ever computed is at the time that their slot is first voted on;
    // after that, the latencies are retained unaltered until the slot is rooted.

    // All the votes in our current vote state that are missing from the new vote state
    // must have been expired by later votes. Check that the lockouts match this assumption.
    while current_vote_state_index < vote_state.votes().len()
        && new_vote_state_index < new_state.len()
    {
        let current_vote = &vote_state.votes()[current_vote_state_index];
        let new_vote = &mut new_state[new_vote_state_index];

        // If the current slot is less than the new proposed slot, then the
        // new slot must have popped off the old slot, so check that the
        // lockouts are corrects.
        match current_vote.slot().cmp(&new_vote.slot()) {
            Ordering::Less => {
                if current_vote.lockout.last_locked_out_slot() >= new_vote.slot() {
                    return Err(VoteError::LockoutConflict);
                }
                current_vote_state_index = current_vote_state_index.checked_add(1).expect(
                    "`current_vote_state_index` is bounded by `MAX_LOCKOUT_HISTORY` when slot is \
                     less than proposed",
                );
            }
            Ordering::Equal => {
                // The new vote state should never have less lockout than
                // the previous vote state for the same slot
                if new_vote.confirmation_count() < current_vote.confirmation_count() {
                    return Err(VoteError::ConfirmationRollBack);
                }

                // Copy the vote slot latency in from the current state to the new state
                new_vote.latency = vote_state.votes()[current_vote_state_index].latency;

                current_vote_state_index = current_vote_state_index.checked_add(1).expect(
                    "`current_vote_state_index` is bounded by `MAX_LOCKOUT_HISTORY` when slot is \
                     equal to proposed",
                );
                new_vote_state_index = new_vote_state_index.checked_add(1).expect(
                    "`new_vote_state_index` is bounded by `MAX_LOCKOUT_HISTORY` when slot is \
                     equal to proposed",
                );
            }
            Ordering::Greater => {
                new_vote_state_index = new_vote_state_index.checked_add(1).expect(
                    "`new_vote_state_index` is bounded by `MAX_LOCKOUT_HISTORY` when slot is \
                     greater than proposed",
                );
            }
        }
    }

    // `new_vote_state` passed all the checks, finalize the change by rewriting
    // our state.

    // Now set the vote latencies on new slots not in the current state.  New slots not in the current vote state will
    // have had their latency initialized to 0 by the above loop.  Those will now be updated to their actual latency.
    for new_vote in new_state.iter_mut() {
        if new_vote.latency == 0 {
            new_vote.latency = handler::compute_vote_latency(new_vote.slot(), current_slot);
        }
    }

    if vote_state.root_slot() != new_root {
        // Award vote credits based on the number of slots that were voted on and have reached finality
        // For each finalized slot, there was one voted-on slot in the new vote state that was responsible for
        // finalizing it. Each of those votes is awarded 1 credit.
        vote_state.increment_credits(epoch, earned_credits);
    }
    if let Some(timestamp) = timestamp {
        let last_slot = new_state.back().unwrap().slot();
        vote_state.process_timestamp(last_slot, timestamp)?;
    }
    vote_state.set_root_slot(new_root);
    vote_state.set_votes(new_state);

    Ok(())
}

pub fn process_vote_unfiltered<T: VoteStateHandle>(
    vote_state: &mut T,
    vote_slots: &[Slot],
    vote: &Vote,
    slot_hashes: &[SlotHash],
    epoch: Epoch,
    current_slot: Slot,
) -> Result<(), VoteError> {
    check_slots_are_valid(vote_state, vote_slots, &vote.hash, slot_hashes)?;
    vote_slots
        .iter()
        .for_each(|s| vote_state.process_next_vote_slot(*s, epoch, current_slot));
    Ok(())
}

pub fn process_vote(
    vote_state: &mut VoteStateHandler,
    vote: &Vote,
    slot_hashes: &[SlotHash],
    epoch: Epoch,
    current_slot: Slot,
) -> Result<(), VoteError> {
    if vote.slots.is_empty() {
        return Err(VoteError::EmptySlots);
    }
    let earliest_slot_in_history = slot_hashes.last().map(|(slot, _hash)| *slot).unwrap_or(0);
    let vote_slots = vote
        .slots
        .iter()
        .filter(|slot| **slot >= earliest_slot_in_history)
        .cloned()
        .collect::<Vec<Slot>>();
    if vote_slots.is_empty() {
        return Err(VoteError::VotesTooOldAllFiltered);
    }
    process_vote_unfiltered(
        vote_state,
        &vote_slots,
        vote,
        slot_hashes,
        epoch,
        current_slot,
    )
}

/// "unchecked" functions used by tests and Tower
pub fn process_vote_unchecked<T: VoteStateHandle>(
    vote_state: &mut T,
    vote: Vote,
) -> Result<(), VoteError> {
    if vote.slots.is_empty() {
        return Err(VoteError::EmptySlots);
    }
    let slot_hashes: Vec<_> = vote.slots.iter().rev().map(|x| (*x, vote.hash)).collect();
    process_vote_unfiltered(
        vote_state,
        &vote.slots,
        &vote,
        &slot_hashes,
        vote_state.current_epoch(),
        0,
    )
}

#[cfg(test)]
pub fn process_slot_votes_unchecked<T: VoteStateHandle>(vote_state: &mut T, slots: &[Slot]) {
    for slot in slots {
        process_slot_vote_unchecked(vote_state, *slot);
    }
}

pub fn process_slot_vote_unchecked<T: VoteStateHandle>(vote_state: &mut T, slot: Slot) {
    let _ = process_vote_unchecked(vote_state, Vote::new(vec![slot], Hash::default()));
}

/// Authorize the given pubkey to withdraw or sign votes. This may be called multiple times,
/// but will implicitly withdraw authorization from the previously authorized
/// key
pub fn authorize<S: std::hash::BuildHasher>(
    vote_account: &mut BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
    authorized: &Pubkey,
    vote_authorize: VoteAuthorize,
    signers: &HashSet<Pubkey, S>,
    clock: &Clock,
) -> Result<(), InstructionError> {
    let mut vote_state = get_vote_state_handler_checked(
        vote_account,
        PreserveBehaviorInHandlerHelper::new(target_version, false),
    )?;

    match vote_authorize {
        VoteAuthorize::Voter => {
            let authorized_withdrawer_signer =
                verify_authorized_signer(vote_state.authorized_withdrawer(), signers).is_ok();

            vote_state.set_new_authorized_voter(
                authorized,
                clock.epoch,
                clock
                    .leader_schedule_epoch
                    .checked_add(1)
                    .ok_or(InstructionError::InvalidAccountData)?,
                |epoch_authorized_voter| {
                    // current authorized withdrawer or authorized voter must say "yay"
                    if authorized_withdrawer_signer {
                        Ok(())
                    } else {
                        verify_authorized_signer(&epoch_authorized_voter, signers)
                    }
                },
            )?;
        }
        VoteAuthorize::Withdrawer => {
            // current authorized withdrawer must say "yay"
            verify_authorized_signer(vote_state.authorized_withdrawer(), signers)?;
            vote_state.set_authorized_withdrawer(*authorized);
        }
        // VoterWithBLS not yet implemented.
        VoteAuthorize::VoterWithBLS(_) => {
            return Err(InstructionError::InvalidInstructionData);
        }
    }

    vote_state.set_vote_account_state(vote_account)
}

/// Update the node_pubkey, requires signature of the authorized voter
pub fn update_validator_identity<S: std::hash::BuildHasher>(
    vote_account: &mut BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
    node_pubkey: &Pubkey,
    signers: &HashSet<Pubkey, S>,
) -> Result<(), InstructionError> {
    let mut vote_state = get_vote_state_handler_checked(
        vote_account,
        PreserveBehaviorInHandlerHelper::new(target_version, false),
    )?;

    // current authorized withdrawer must say "yay"
    verify_authorized_signer(vote_state.authorized_withdrawer(), signers)?;

    // new node must say "yay"
    verify_authorized_signer(node_pubkey, signers)?;

    vote_state.set_node_pubkey(*node_pubkey);
    // Keep block_revenue_collector in sync with node_pubkey until SIMD-0232
    // is implemented.
    vote_state.set_block_revenue_collector(*node_pubkey);

    vote_state.set_vote_account_state(vote_account)
}

/// Update the vote account's commission
pub fn update_commission<S: std::hash::BuildHasher>(
    vote_account: &mut BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
    commission: u8,
    signers: &HashSet<Pubkey, S>,
    epoch_schedule: &EpochSchedule,
    clock: &Clock,
    disable_commission_update_rule: bool,
) -> Result<(), InstructionError> {
    let vote_state_result = get_vote_state_handler_checked(
        vote_account,
        PreserveBehaviorInHandlerHelper::new(target_version, false),
    );
    let enforce_commission_update_rule = !disable_commission_update_rule
        && match vote_state_result.as_ref() {
            Ok(decoded_vote_state) => commission > decoded_vote_state.commission(),
            Err(_) => true,
        };

    if enforce_commission_update_rule && !is_commission_update_allowed(clock.slot, epoch_schedule) {
        return Err(VoteError::CommissionUpdateTooLate.into());
    }

    let mut vote_state = vote_state_result?;

    // current authorized withdrawer must say "yay"
    verify_authorized_signer(vote_state.authorized_withdrawer(), signers)?;

    vote_state.set_commission(commission);

    vote_state.set_vote_account_state(vote_account)
}

/// Given the current slot and epoch schedule, determine if a commission change
/// is allowed
pub fn is_commission_update_allowed(slot: Slot, epoch_schedule: &EpochSchedule) -> bool {
    // always allowed during warmup epochs
    if let Some(relative_slot) = slot
        .saturating_sub(epoch_schedule.first_normal_slot)
        .checked_rem(epoch_schedule.slots_per_epoch)
    {
        // allowed up to the midpoint of the epoch
        relative_slot.saturating_mul(2) <= epoch_schedule.slots_per_epoch
    } else {
        // no slots per epoch, just allow it, even though this should never happen
        true
    }
}

fn verify_authorized_signer<S: std::hash::BuildHasher>(
    authorized: &Pubkey,
    signers: &HashSet<Pubkey, S>,
) -> Result<(), InstructionError> {
    if signers.contains(authorized) {
        Ok(())
    } else {
        Err(InstructionError::MissingRequiredSignature)
    }
}

/// Withdraw funds from the vote account
pub fn withdraw<S: std::hash::BuildHasher>(
    instruction_context: &InstructionContext,
    vote_account_index: IndexOfAccount,
    target_version: VoteStateTargetVersion,
    lamports: u64,
    to_account_index: IndexOfAccount,
    signers: &HashSet<Pubkey, S>,
    rent_sysvar: &Rent,
    clock: &Clock,
) -> Result<(), InstructionError> {
    let mut vote_account =
        instruction_context.try_borrow_instruction_account(vote_account_index)?;
    let vote_state = get_vote_state_handler_checked(
        &vote_account,
        PreserveBehaviorInHandlerHelper::new(target_version, false),
    )?;

    verify_authorized_signer(vote_state.authorized_withdrawer(), signers)?;

    let remaining_balance = vote_account
        .get_lamports()
        .checked_sub(lamports)
        .ok_or(InstructionError::InsufficientFunds)?;

    if remaining_balance == 0 {
        let reject_active_vote_account_close = vote_state
            .epoch_credits()
            .last()
            .map(|(last_epoch_with_credits, _, _)| {
                let current_epoch = clock.epoch;
                // if current_epoch - last_epoch_with_credits < 2 then the validator has received credits
                // either in the current epoch or the previous epoch. If it's >= 2 then it has been at least
                // one full epoch since the validator has received credits.
                current_epoch.saturating_sub(*last_epoch_with_credits) < 2
            })
            .unwrap_or(false);

        if reject_active_vote_account_close {
            return Err(VoteError::ActiveVoteAccountClose.into());
        } else {
            // Deinitialize upon zero-balance
            VoteStateHandler::deinitialize_vote_account_state(&mut vote_account, target_version)?;
        }
    } else {
        let min_rent_exempt_balance = rent_sysvar.minimum_balance(vote_account.get_data().len());
        if remaining_balance < min_rent_exempt_balance {
            return Err(InstructionError::InsufficientFunds);
        }
    }

    vote_account.checked_sub_lamports(lamports)?;
    drop(vote_account);
    let mut to_account = instruction_context.try_borrow_instruction_account(to_account_index)?;
    to_account.checked_add_lamports(lamports)?;
    Ok(())
}

/// Initialize the vote_state for a vote account
/// Assumes that the account is being init as part of a account creation or balance transfer and
/// that the transaction must be signed by the staker's keys
pub fn initialize_account<S: std::hash::BuildHasher>(
    vote_account: &mut BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
    vote_init: &VoteInit,
    signers: &HashSet<Pubkey, S>,
    clock: &Clock,
) -> Result<(), InstructionError> {
    VoteStateHandler::check_vote_account_length(vote_account, target_version)?;
    let versioned = vote_account.get_state::<VoteStateVersions>()?;

    if !versioned.is_uninitialized() {
        return Err(InstructionError::AccountAlreadyInitialized);
    }

    // node must agree to accept this vote account
    verify_authorized_signer(&vote_init.node_pubkey, signers)?;

    VoteStateHandler::init_vote_account_state(vote_account, vote_init, clock, target_version)
}

pub fn process_vote_with_account<S: std::hash::BuildHasher>(
    vote_account: &mut BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
    slot_hashes: &[SlotHash],
    clock: &Clock,
    vote: &Vote,
    signers: &HashSet<Pubkey, S>,
) -> Result<(), InstructionError> {
    let mut vote_state = get_vote_state_handler_checked(
        vote_account,
        PreserveBehaviorInHandlerHelper::new(target_version, true),
    )?;

    let authorized_voter = vote_state.get_and_update_authorized_voter(clock.epoch)?;
    verify_authorized_signer(&authorized_voter, signers)?;

    process_vote(&mut vote_state, vote, slot_hashes, clock.epoch, clock.slot)?;
    if let Some(timestamp) = vote.timestamp {
        vote.slots
            .iter()
            .max()
            .ok_or(VoteError::EmptySlots)
            .and_then(|slot| vote_state.process_timestamp(*slot, timestamp))?;
    }
    vote_state.set_vote_account_state(vote_account)
}

pub fn process_vote_state_update<S: std::hash::BuildHasher>(
    vote_account: &mut BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
    slot_hashes: &[SlotHash],
    clock: &Clock,
    vote_state_update: VoteStateUpdate,
    signers: &HashSet<Pubkey, S>,
) -> Result<(), InstructionError> {
    let mut vote_state = get_vote_state_handler_checked(
        vote_account,
        PreserveBehaviorInHandlerHelper::new(target_version, true),
    )?;

    let authorized_voter = vote_state.get_and_update_authorized_voter(clock.epoch)?;
    verify_authorized_signer(&authorized_voter, signers)?;

    do_process_vote_state_update(
        &mut vote_state,
        slot_hashes,
        clock.epoch,
        clock.slot,
        vote_state_update,
    )?;
    vote_state.set_vote_account_state(vote_account)
}

pub fn do_process_vote_state_update(
    vote_state: &mut VoteStateHandler,
    slot_hashes: &[SlotHash],
    epoch: u64,
    slot: u64,
    mut vote_state_update: VoteStateUpdate,
) -> Result<(), VoteError> {
    check_and_filter_proposed_vote_state(
        vote_state,
        &mut vote_state_update.lockouts,
        &mut vote_state_update.root,
        vote_state_update.hash,
        slot_hashes,
    )?;
    process_new_vote_state(
        vote_state,
        vote_state_update
            .lockouts
            .iter()
            .map(|lockout| LandedVote::from(*lockout))
            .collect(),
        vote_state_update.root,
        vote_state_update.timestamp,
        epoch,
        slot,
    )
}

pub fn process_tower_sync<S: std::hash::BuildHasher>(
    vote_account: &mut BorrowedInstructionAccount,
    target_version: VoteStateTargetVersion,
    slot_hashes: &[SlotHash],
    clock: &Clock,
    tower_sync: TowerSync,
    signers: &HashSet<Pubkey, S>,
) -> Result<(), InstructionError> {
    let mut vote_state = get_vote_state_handler_checked(
        vote_account,
        PreserveBehaviorInHandlerHelper::new(target_version, true),
    )?;

    let authorized_voter = vote_state.get_and_update_authorized_voter(clock.epoch)?;
    verify_authorized_signer(&authorized_voter, signers)?;

    do_process_tower_sync(
        &mut vote_state,
        slot_hashes,
        clock.epoch,
        clock.slot,
        tower_sync,
    )?;
    vote_state.set_vote_account_state(vote_account)
}

fn do_process_tower_sync(
    vote_state: &mut VoteStateHandler,
    slot_hashes: &[SlotHash],
    epoch: u64,
    slot: u64,
    mut tower_sync: TowerSync,
) -> Result<(), VoteError> {
    check_and_filter_proposed_vote_state(
        vote_state,
        &mut tower_sync.lockouts,
        &mut tower_sync.root,
        tower_sync.hash,
        slot_hashes,
    )?;
    process_new_vote_state(
        vote_state,
        tower_sync
            .lockouts
            .iter()
            .map(|lockout| LandedVote::from(*lockout))
            .collect(),
        tower_sync.root,
        tower_sync.timestamp,
        epoch,
        slot,
    )
}

#[cfg(test)]
pub fn create_account_with_authorized(
    node_pubkey: &Pubkey,
    authorized_voter: &Pubkey,
    authorized_withdrawer: &Pubkey,
    commission: u8,
    lamports: u64,
) -> AccountSharedData {
    let mut vote_account = AccountSharedData::new(lamports, VoteStateV3::size_of(), &id());

    let vote_state = VoteStateV3::new(
        &VoteInit {
            node_pubkey: *node_pubkey,
            authorized_voter: *authorized_voter,
            authorized_withdrawer: *authorized_withdrawer,
            commission,
        },
        &Clock::default(),
    );

    VoteStateV3::serialize(
        &VoteStateVersions::V3(Box::new(vote_state)),
        vote_account.data_as_mut_slice(),
    )
    .unwrap();

    vote_account
}

pub fn create_v4_account_with_authorized(
    node_pubkey: &Pubkey,
    authorized_voter: &Pubkey,
    authorized_withdrawer: &Pubkey,
    bls_pubkey_compressed: Option<[u8; BLS_PUBLIC_KEY_COMPRESSED_SIZE]>,
    inflation_rewards_commission_bps: u16,
    lamports: u64,
) -> AccountSharedData {
    let mut vote_account = AccountSharedData::new(lamports, VoteStateV4::size_of(), &id());

    let vote_state = handler::create_new_vote_state_v4_for_tests(
        node_pubkey,
        authorized_voter,
        authorized_withdrawer,
        bls_pubkey_compressed,
        inflation_rewards_commission_bps,
    );

    VoteStateV4::serialize(
        &VoteStateVersions::V4(Box::new(vote_state)),
        vote_account.data_as_mut_slice(),
    )
    .unwrap();

    vote_account
}

#[allow(clippy::arithmetic_side_effects)]
#[cfg(test)]
mod tests {
    use {
        super::*,
        assert_matches::assert_matches,
        solana_account::{AccountSharedData, ReadableAccount},
        solana_clock::DEFAULT_SLOTS_PER_EPOCH,
        solana_sha256_hasher::hash,
        solana_transaction_context::{
            instruction_accounts::InstructionAccount, TransactionContext,
        },
        solana_vote_interface::authorized_voters::AuthorizedVoters,
        test_case::{test_case, test_matrix},
    };

    const MAX_RECENT_VOTES: usize = 16;

    fn vote_state_new_for_test(
        vote_pubkey: &Pubkey,
        target_version: VoteStateTargetVersion,
    ) -> VoteStateHandler {
        let auth_pubkey = solana_pubkey::new_rand();
        let vote_init = VoteInit {
            node_pubkey: solana_pubkey::new_rand(),
            authorized_voter: auth_pubkey,
            authorized_withdrawer: auth_pubkey,
            commission: 0,
        };
        let clock = Clock::default();

        match target_version {
            VoteStateTargetVersion::V3 => {
                VoteStateHandler::new_v3(VoteStateV3::new(&vote_init, &clock))
            }
            VoteStateTargetVersion::V4 => VoteStateHandler::new_v4(
                handler::create_new_vote_state_v4(vote_pubkey, &vote_init, &clock),
            ),
        }
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_vote_state_upgrade_from_1_14_11(target_version: VoteStateTargetVersion) {
        let vote_pubkey = solana_pubkey::new_rand();
        let mut vote_state = vote_state_new_for_test(&vote_pubkey, target_version);

        // Simulate prior epochs completed with credits and each setting a new authorized voter
        vote_state.increment_credits(0, 100);
        assert_eq!(
            vote_state.set_new_authorized_voter(&solana_pubkey::new_rand(), 0, 1, |_pubkey| Ok(())),
            Ok(())
        );
        vote_state.increment_credits(1, 200);
        assert_eq!(
            vote_state.set_new_authorized_voter(&solana_pubkey::new_rand(), 1, 2, |_pubkey| Ok(())),
            Ok(())
        );
        vote_state.increment_credits(2, 300);
        assert_eq!(
            vote_state.set_new_authorized_voter(&solana_pubkey::new_rand(), 2, 3, |_pubkey| Ok(())),
            Ok(())
        );

        // Simulate votes having occurred
        vec![
            100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116,
            117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133,
            134, 135,
        ]
        .into_iter()
        .for_each(|v| vote_state.process_next_vote_slot(v, 4, 0));

        // Create an initial vote account that is sized for the 1_14_11 version of vote state, and has only the
        // required lamports for rent exempt minimum at that size
        let vote_state_v1_14_11 = match target_version {
            VoteStateTargetVersion::V3 => {
                // v3 can be converted directly to V1_14_11.
                VoteState1_14_11::from(vote_state.as_ref_v3().clone())
            }
            VoteStateTargetVersion::V4 => {
                // v4 cannot be converted directly to V1_14_11.
                VoteState1_14_11 {
                    node_pubkey: *vote_state.node_pubkey(),
                    authorized_withdrawer: *vote_state.authorized_withdrawer(),
                    commission: vote_state.commission(),
                    votes: vote_state
                        .votes()
                        .iter()
                        .map(|landed_vote| (*landed_vote).into())
                        .collect(),
                    root_slot: vote_state.root_slot(),
                    authorized_voters: vote_state.authorized_voters().clone(),
                    epoch_credits: vote_state.epoch_credits().clone(),
                    last_timestamp: vote_state.last_timestamp().clone(),
                    prior_voters: CircBuf::default(), // v4 does not store prior_voters
                }
            }
        };
        let version1_14_11_serialized =
            bincode::serialize(&VoteStateVersions::V1_14_11(Box::new(vote_state_v1_14_11)))
                .unwrap();
        let version1_14_11_serialized_len = version1_14_11_serialized.len();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(version1_14_11_serialized_len);
        let mut vote_account =
            AccountSharedData::new(lamports, version1_14_11_serialized_len, &id());
        vote_account.set_data_from_slice(&version1_14_11_serialized);

        // Create a fake TransactionContext with a fake InstructionContext with a single account which is the
        // vote account that was just created
        let processor_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let mut transaction_context = TransactionContext::new(
            vec![(id(), processor_account), (vote_pubkey, vote_account)],
            rent.clone(),
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
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();

        // Get the BorrowedAccount from the InstructionContext which is what is used to manipulate and inspect account
        // state
        let mut borrowed_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        // Ensure that the vote state started out at 1_14_11
        let vote_state_version = borrowed_account.get_state::<VoteStateVersions>().unwrap();
        assert_matches!(vote_state_version, VoteStateVersions::V1_14_11(_));

        // Convert the vote state to current as would occur during vote instructions
        let converted_vote_state = get_vote_state_handler_checked(
            &borrowed_account,
            PreserveBehaviorInHandlerHelper::new(target_version, true),
        )
        .unwrap();

        // Check to make sure that the vote_state is unchanged
        assert!(vote_state == converted_vote_state);

        let vote_state = converted_vote_state;

        // Now re-set the vote account state, knowing the account only has
        // enough lamports for V1_14_11.
        match target_version {
            VoteStateTargetVersion::V3 => {
                // V3 will write out as V1_14_11.
                assert_eq!(
                    vote_state
                        .clone()
                        .set_vote_account_state(&mut borrowed_account),
                    Ok(())
                );
                let vote_state_version = borrowed_account.get_state::<VoteStateVersions>().unwrap();
                assert_matches!(vote_state_version, VoteStateVersions::V1_14_11(_));
            }
            VoteStateTargetVersion::V4 => {
                // V4 will throw an error.
                assert_eq!(
                    vote_state
                        .clone()
                        .set_vote_account_state(&mut borrowed_account),
                    Err(InstructionError::AccountNotRentExempt)
                );
            }
        }

        // Convert the vote state to current as would occur during vote instructions
        let converted_vote_state = get_vote_state_handler_checked(
            &borrowed_account,
            PreserveBehaviorInHandlerHelper::new(target_version, true),
        )
        .unwrap();

        // Check to make sure that the vote_state is unchanged
        assert!(vote_state == converted_vote_state);

        let vote_state = converted_vote_state;

        // Now top-up the vote account's lamports to be rent exempt for the target version.
        let space = match target_version {
            VoteStateTargetVersion::V3 => VoteStateV3::size_of(),
            VoteStateTargetVersion::V4 => VoteStateV4::size_of(), // They're the same, but for posterity
        };
        assert_eq!(
            borrowed_account.set_lamports(rent.minimum_balance(space)),
            Ok(())
        );
        assert_eq!(
            vote_state
                .clone()
                .set_vote_account_state(&mut borrowed_account),
            Ok(())
        );

        // The vote state version should match the target version.
        let vote_state_version = borrowed_account.get_state::<VoteStateVersions>().unwrap();
        match target_version {
            VoteStateTargetVersion::V3 => {
                assert_matches!(vote_state_version, VoteStateVersions::V3(_));
            }
            VoteStateTargetVersion::V4 => {
                assert_matches!(vote_state_version, VoteStateVersions::V4(_));
            }
        }

        // Convert the vote state to current as would occur during vote instructions
        let converted_vote_state = get_vote_state_handler_checked(
            &borrowed_account,
            PreserveBehaviorInHandlerHelper::new(target_version, true),
        )
        .unwrap();

        // Check to make sure that the vote_state is unchanged
        assert_eq!(vote_state, converted_vote_state);
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_vote_lockout(target_version: VoteStateTargetVersion) {
        let mut vote_state = vote_state_new_for_test(&solana_pubkey::new_rand(), target_version);

        for i in 0..(MAX_LOCKOUT_HISTORY + 1) {
            process_slot_vote_unchecked(&mut vote_state, (INITIAL_LOCKOUT * i) as u64);
        }

        // The last vote should have been popped b/c it reached a depth of MAX_LOCKOUT_HISTORY
        assert_eq!(vote_state.votes().len(), MAX_LOCKOUT_HISTORY);
        assert_eq!(vote_state.root_slot(), Some(0));
        check_lockouts(&vote_state);

        // One more vote that confirms the entire stack,
        // the root_slot should change to the
        // second vote
        let top_vote = vote_state.votes().front().unwrap().slot();
        let slot = vote_state.last_lockout().unwrap().last_locked_out_slot();
        process_slot_vote_unchecked(&mut vote_state, slot);
        assert_eq!(Some(top_vote), vote_state.root_slot());

        // Expire everything except the first vote
        let slot = vote_state
            .votes()
            .front()
            .unwrap()
            .lockout
            .last_locked_out_slot();
        process_slot_vote_unchecked(&mut vote_state, slot);
        // First vote and new vote are both stored for a total of 2 votes
        assert_eq!(vote_state.votes().len(), 2);
    }

    #[test_matrix(
        [VoteStateTargetVersion::V3, VoteStateTargetVersion::V4],
        [true, false]
    )]
    fn test_update_commission(
        target_version: VoteStateTargetVersion,
        disable_commission_update_rule: bool,
    ) {
        let mut vote_state = vote_state_new_for_test(&solana_pubkey::new_rand(), target_version);
        let node_pubkey = *vote_state.node_pubkey();
        let withdrawer_pubkey = *vote_state.authorized_withdrawer();

        // Set commission to start.
        vote_state.set_commission(10);

        let serialized = vote_state.serialize();
        let serialized_len = serialized.len();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(serialized_len);
        let mut vote_account = AccountSharedData::new(lamports, serialized_len, &id());
        vote_account.set_data_from_slice(&serialized);

        // Create a fake TransactionContext with a fake InstructionContext with a single account which is the
        // vote account that was just created
        let processor_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let mut transaction_context = TransactionContext::new(
            vec![(id(), processor_account), (node_pubkey, vote_account)],
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
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();

        // Get the BorrowedAccount from the InstructionContext which is what is used to manipulate and inspect account
        // state
        let mut borrowed_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        let epoch_schedule = std::sync::Arc::new(EpochSchedule::without_warmup());

        let first_half_clock = std::sync::Arc::new(Clock {
            slot: epoch_schedule.slots_per_epoch / 4,
            ..Clock::default()
        });

        let second_half_clock = std::sync::Arc::new(Clock {
            slot: (epoch_schedule.slots_per_epoch * 3) / 4,
            ..Clock::default()
        });

        let signers: HashSet<Pubkey> = vec![withdrawer_pubkey].into_iter().collect();

        // Increase commission in first half of epoch -- allowed
        assert_eq!(
            get_vote_state_handler_checked(
                &borrowed_account,
                PreserveBehaviorInHandlerHelper::new(target_version, true),
            )
            .unwrap()
            .commission(),
            10
        );
        assert_matches!(
            update_commission(
                &mut borrowed_account,
                target_version,
                11,
                &signers,
                &epoch_schedule,
                &first_half_clock,
                disable_commission_update_rule,
            ),
            Ok(())
        );
        assert_eq!(
            get_vote_state_handler_checked(
                &borrowed_account,
                PreserveBehaviorInHandlerHelper::new(target_version, true),
            )
            .unwrap()
            .commission(),
            11
        );

        // Increase commission in second half of epoch -- disallowed if update rule is enabled
        let result = update_commission(
            &mut borrowed_account,
            target_version,
            12,
            &signers,
            &epoch_schedule,
            &second_half_clock,
            disable_commission_update_rule,
        );
        let state_commission = get_vote_state_handler_checked(
            &borrowed_account,
            PreserveBehaviorInHandlerHelper::new(target_version, true),
        )
        .unwrap()
        .commission();
        if disable_commission_update_rule {
            assert_matches!(result, Ok(()));
            assert_eq!(state_commission, 12);
        } else {
            assert_matches!(result, Err(_));
            assert_eq!(state_commission, 11);
        }

        // Decrease commission in first half of epoch -- always allowed
        assert_matches!(
            update_commission(
                &mut borrowed_account,
                target_version,
                10,
                &signers,
                &epoch_schedule,
                &first_half_clock,
                disable_commission_update_rule,
            ),
            Ok(())
        );
        assert_eq!(
            get_vote_state_handler_checked(
                &borrowed_account,
                PreserveBehaviorInHandlerHelper::new(target_version, true),
            )
            .unwrap()
            .commission(),
            10
        );

        assert_eq!(
            get_vote_state_handler_checked(
                &borrowed_account,
                PreserveBehaviorInHandlerHelper::new(target_version, true),
            )
            .unwrap()
            .commission(),
            10
        );

        // Decrease commission in second half of epoch -- always allowed
        assert_matches!(
            update_commission(
                &mut borrowed_account,
                target_version,
                9,
                &signers,
                &epoch_schedule,
                &second_half_clock,
                disable_commission_update_rule,
            ),
            Ok(())
        );
        assert_eq!(
            get_vote_state_handler_checked(
                &borrowed_account,
                PreserveBehaviorInHandlerHelper::new(target_version, true),
            )
            .unwrap()
            .commission(),
            9
        );
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_vote_double_lockout_after_expiration(target_version: VoteStateTargetVersion) {
        let mut vote_state = vote_state_new_for_test(&solana_pubkey::new_rand(), target_version);

        for i in 0..3 {
            process_slot_vote_unchecked(&mut vote_state, i as u64);
        }

        check_lockouts(&vote_state);

        // Expire the third vote (which was a vote for slot 2). The height of the
        // vote stack is unchanged, so none of the previous votes should have
        // doubled in lockout
        process_slot_vote_unchecked(&mut vote_state, (2 + INITIAL_LOCKOUT + 1) as u64);
        check_lockouts(&vote_state);

        // Vote again, this time the vote stack depth increases, so the votes should
        // double for everybody
        process_slot_vote_unchecked(&mut vote_state, (2 + INITIAL_LOCKOUT + 2) as u64);
        check_lockouts(&vote_state);

        // Vote again, this time the vote stack depth increases, so the votes should
        // double for everybody
        process_slot_vote_unchecked(&mut vote_state, (2 + INITIAL_LOCKOUT + 3) as u64);
        check_lockouts(&vote_state);
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_expire_multiple_votes(target_version: VoteStateTargetVersion) {
        let mut vote_state = vote_state_new_for_test(&solana_pubkey::new_rand(), target_version);

        for i in 0..3 {
            process_slot_vote_unchecked(&mut vote_state, i as u64);
        }

        assert_eq!(vote_state.votes()[0].confirmation_count(), 3);

        // Expire the second and third votes
        let expire_slot =
            vote_state.votes()[1].slot() + vote_state.votes()[1].lockout.lockout() + 1;
        process_slot_vote_unchecked(&mut vote_state, expire_slot);
        assert_eq!(vote_state.votes().len(), 2);

        // Check that the old votes expired
        assert_eq!(vote_state.votes()[0].slot(), 0);
        assert_eq!(vote_state.votes()[1].slot(), expire_slot);

        // Process one more vote
        process_slot_vote_unchecked(&mut vote_state, expire_slot + 1);

        // Confirmation count for the older first vote should remain unchanged
        assert_eq!(vote_state.votes()[0].confirmation_count(), 3);

        // The later votes should still have increasing confirmation counts
        assert_eq!(vote_state.votes()[1].confirmation_count(), 2);
        assert_eq!(vote_state.votes()[2].confirmation_count(), 1);
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_vote_credits(target_version: VoteStateTargetVersion) {
        let mut vote_state = vote_state_new_for_test(&solana_pubkey::new_rand(), target_version);

        for i in 0..MAX_LOCKOUT_HISTORY {
            process_slot_vote_unchecked(&mut vote_state, i as u64);
        }

        assert_eq!(vote_state.credits(), 0);

        process_slot_vote_unchecked(&mut vote_state, MAX_LOCKOUT_HISTORY as u64 + 1);
        assert_eq!(vote_state.credits(), 1);
        process_slot_vote_unchecked(&mut vote_state, MAX_LOCKOUT_HISTORY as u64 + 2);
        assert_eq!(vote_state.credits(), 2);
        process_slot_vote_unchecked(&mut vote_state, MAX_LOCKOUT_HISTORY as u64 + 3);
        assert_eq!(vote_state.credits(), 3);
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_duplicate_vote(target_version: VoteStateTargetVersion) {
        let mut vote_state = vote_state_new_for_test(&solana_pubkey::new_rand(), target_version);
        process_slot_vote_unchecked(&mut vote_state, 0);
        process_slot_vote_unchecked(&mut vote_state, 1);
        process_slot_vote_unchecked(&mut vote_state, 0);
        assert_eq!(vote_state.nth_recent_lockout(0).unwrap().slot(), 1);
        assert_eq!(vote_state.nth_recent_lockout(1).unwrap().slot(), 0);
        assert!(vote_state.nth_recent_lockout(2).is_none());
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_nth_recent_lockout(target_version: VoteStateTargetVersion) {
        let mut vote_state = vote_state_new_for_test(&solana_pubkey::new_rand(), target_version);
        for i in 0..MAX_LOCKOUT_HISTORY {
            process_slot_vote_unchecked(&mut vote_state, i as u64);
        }
        for i in 0..(MAX_LOCKOUT_HISTORY - 1) {
            assert_eq!(
                vote_state.nth_recent_lockout(i).unwrap().slot() as usize,
                MAX_LOCKOUT_HISTORY - i - 1,
            );
        }
        assert!(vote_state.nth_recent_lockout(MAX_LOCKOUT_HISTORY).is_none());
    }

    fn check_lockouts(vote_state: &VoteStateHandler) {
        let votes = vote_state.votes();
        for (i, vote) in votes.iter().enumerate() {
            let num_votes = votes
                .len()
                .checked_sub(i)
                .expect("`i` is less than `vote_state.votes().len()`");
            assert_eq!(
                vote.lockout.lockout(),
                INITIAL_LOCKOUT.pow(num_votes as u32) as u64
            );
        }
    }

    fn recent_votes(vote_state: &VoteStateHandler) -> Vec<Vote> {
        let votes = vote_state.votes();
        let start = votes.len().saturating_sub(MAX_RECENT_VOTES);
        (start..votes.len())
            .map(|i| Vote::new(vec![votes.get(i).unwrap().slot()], Hash::default()))
            .collect()
    }

    /// check that two accounts with different data can be brought to the same state with one vote submission
    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_process_missed_votes(target_version: VoteStateTargetVersion) {
        let mut vote_state_a = vote_state_new_for_test(&solana_pubkey::new_rand(), target_version);
        let mut vote_state_b = vote_state_new_for_test(&solana_pubkey::new_rand(), target_version);

        // process some votes on account a
        (0..5).for_each(|i| process_slot_vote_unchecked(&mut vote_state_a, i as u64));
        assert_ne!(recent_votes(&vote_state_a), recent_votes(&vote_state_b));

        // as long as b has missed less than "NUM_RECENT" votes both accounts should be in sync
        let slots = (0u64..MAX_RECENT_VOTES as u64).collect();
        let vote = Vote::new(slots, Hash::default());
        let slot_hashes: Vec<_> = vote.slots.iter().rev().map(|x| (*x, vote.hash)).collect();

        assert_eq!(
            process_vote(&mut vote_state_a, &vote, &slot_hashes, 0, 0),
            Ok(())
        );
        assert_eq!(
            process_vote(&mut vote_state_b, &vote, &slot_hashes, 0, 0),
            Ok(())
        );
        assert_eq!(recent_votes(&vote_state_a), recent_votes(&vote_state_b));
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_vote_skips_old_vote(mut vote_state: VoteStateHandler) {
        let vote = Vote::new(vec![0], Hash::default());
        let slot_hashes: Vec<_> = vec![(0, vote.hash)];
        assert_eq!(
            process_vote(&mut vote_state, &vote, &slot_hashes, 0, 0),
            Ok(())
        );
        let recent = recent_votes(&vote_state);
        assert_eq!(
            process_vote(&mut vote_state, &vote, &slot_hashes, 0, 0),
            Err(VoteError::VoteTooOld)
        );
        assert_eq!(recent, recent_votes(&vote_state));
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_check_slots_are_valid_vote_empty_slot_hashes(vote_state: VoteStateHandler) {
        let vote = Vote::new(vec![0], Hash::default());
        assert_eq!(
            check_slots_are_valid(&vote_state, &vote.slots, &vote.hash, &[]),
            Err(VoteError::VoteTooOld)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_check_slots_are_valid_new_vote(vote_state: VoteStateHandler) {
        let vote = Vote::new(vec![0], Hash::default());
        let slot_hashes: Vec<_> = vec![(*vote.slots.last().unwrap(), vote.hash)];
        assert_eq!(
            check_slots_are_valid(&vote_state, &vote.slots, &vote.hash, &slot_hashes),
            Ok(())
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_check_slots_are_valid_bad_hash(vote_state: VoteStateHandler) {
        let vote = Vote::new(vec![0], Hash::default());
        let slot_hashes: Vec<_> = vec![(*vote.slots.last().unwrap(), hash(vote.hash.as_ref()))];
        assert_eq!(
            check_slots_are_valid(&vote_state, &vote.slots, &vote.hash, &slot_hashes),
            Err(VoteError::SlotHashMismatch)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_check_slots_are_valid_bad_slot(vote_state: VoteStateHandler) {
        let vote = Vote::new(vec![1], Hash::default());
        let slot_hashes: Vec<_> = vec![(0, vote.hash)];
        assert_eq!(
            check_slots_are_valid(&vote_state, &vote.slots, &vote.hash, &slot_hashes),
            Err(VoteError::SlotsMismatch)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_check_slots_are_valid_duplicate_vote(mut vote_state: VoteStateHandler) {
        let vote = Vote::new(vec![0], Hash::default());
        let slot_hashes: Vec<_> = vec![(*vote.slots.last().unwrap(), vote.hash)];
        assert_eq!(
            process_vote(&mut vote_state, &vote, &slot_hashes, 0, 0),
            Ok(())
        );
        assert_eq!(
            check_slots_are_valid(&vote_state, &vote.slots, &vote.hash, &slot_hashes),
            Err(VoteError::VoteTooOld)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_check_slots_are_valid_next_vote(mut vote_state: VoteStateHandler) {
        let vote = Vote::new(vec![0], Hash::default());
        let slot_hashes: Vec<_> = vec![(*vote.slots.last().unwrap(), vote.hash)];
        assert_eq!(
            process_vote(&mut vote_state, &vote, &slot_hashes, 0, 0),
            Ok(())
        );

        let vote = Vote::new(vec![0, 1], Hash::default());
        let slot_hashes: Vec<_> = vec![(1, vote.hash), (0, vote.hash)];
        assert_eq!(
            check_slots_are_valid(&vote_state, &vote.slots, &vote.hash, &slot_hashes),
            Ok(())
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_check_slots_are_valid_next_vote_only(mut vote_state: VoteStateHandler) {
        let vote = Vote::new(vec![0], Hash::default());
        let slot_hashes: Vec<_> = vec![(*vote.slots.last().unwrap(), vote.hash)];
        assert_eq!(
            process_vote(&mut vote_state, &vote, &slot_hashes, 0, 0),
            Ok(())
        );

        let vote = Vote::new(vec![1], Hash::default());
        let slot_hashes: Vec<_> = vec![(1, vote.hash), (0, vote.hash)];
        assert_eq!(
            check_slots_are_valid(&vote_state, &vote.slots, &vote.hash, &slot_hashes),
            Ok(())
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_vote_empty_slots(mut vote_state: VoteStateHandler) {
        let vote = Vote::new(vec![], Hash::default());
        assert_eq!(
            process_vote(&mut vote_state, &vote, &[], 0, 0),
            Err(VoteError::EmptySlots)
        );
    }

    pub fn process_new_vote_state_from_lockouts(
        vote_state: &mut VoteStateHandler,
        new_state: VecDeque<Lockout>,
        new_root: Option<Slot>,
        timestamp: Option<i64>,
        epoch: Epoch,
    ) -> Result<(), VoteError> {
        process_new_vote_state(
            vote_state,
            new_state.into_iter().map(LandedVote::from).collect(),
            new_root,
            timestamp,
            epoch,
            0,
        )
    }

    // Test vote credit updates after "one credit per slot" feature is enabled
    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_vote_state_update_increment_credits(mut vote_state: VoteStateHandler) {
        // Test data: a sequence of groups of votes to simulate having been cast, after each group a vote
        // state update is compared to "normal" vote processing to ensure that credits are earned equally
        let test_vote_groups: Vec<Vec<Slot>> = vec![
            // Initial set of votes that don't dequeue any slots, so no credits earned
            vec![1, 2, 3, 4, 5, 6, 7, 8],
            vec![
                9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29,
                30, 31,
            ],
            // Now a single vote which should result in the first root and first credit earned
            vec![32],
            // Now another vote, should earn one credit
            vec![33],
            // Two votes in sequence
            vec![34, 35],
            // 3 votes in sequence
            vec![36, 37, 38],
            // 30 votes in sequence
            vec![
                39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59,
                60, 61, 62, 63, 64, 65, 66, 67, 68,
            ],
            // 31 votes in sequence
            vec![
                69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89,
                90, 91, 92, 93, 94, 95, 96, 97, 98, 99,
            ],
            // Votes with expiry
            vec![100, 101, 106, 107, 112, 116, 120, 121, 122, 124],
            // More votes with expiry of a large number of votes
            vec![200, 201],
            vec![
                202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 217,
                218, 219, 220, 221, 222, 223, 224, 225, 226,
            ],
            vec![227, 228, 229, 230, 231, 232, 233, 234, 235, 236],
        ];

        for vote_group in test_vote_groups {
            // Duplicate vote_state so that the new vote can be applied
            let mut vote_state_after_vote = vote_state.clone();

            process_vote_unchecked(
                &mut vote_state_after_vote,
                Vote {
                    slots: vote_group.clone(),
                    hash: Hash::new_unique(),
                    timestamp: None,
                },
            )
            .unwrap();

            // Now use the resulting new vote state to perform a vote state update on vote_state
            assert_eq!(
                process_new_vote_state(
                    &mut vote_state,
                    vote_state_after_vote.votes().clone(),
                    vote_state_after_vote.root_slot(),
                    None,
                    0,
                    0,
                ),
                Ok(())
            );

            // And ensure that the credits earned were the same
            assert_eq!(
                vote_state.epoch_credits(),
                vote_state_after_vote.epoch_credits()
            );
        }
    }

    // Test vote credit updates after "timely vote credits" feature is enabled
    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_timely_credits(target_version: VoteStateTargetVersion) {
        // Each of the following (Vec<Slot>, Slot, u32) tuples gives a set of slots to cast votes on, a slot in which
        // the vote was cast, and the number of credits that should have been earned by the vote account after this
        // and all prior votes were cast.
        let test_vote_groups: Vec<(Vec<Slot>, Slot, u32)> = vec![
            // Initial set of votes that don't dequeue any slots, so no credits earned
            (
                vec![1, 2, 3, 4, 5, 6, 7, 8],
                9,
                // root: none, no credits earned
                0,
            ),
            (
                vec![
                    9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
                    29, 30, 31,
                ],
                34,
                // lockouts full
                // root: none, no credits earned
                0,
            ),
            // Now a single vote which should result in the first root and first credit earned
            (
                vec![32],
                35,
                // root: 1
                // when slot 1 was voted on in slot 9, it earned 10 credits
                10,
            ),
            // Now another vote, should earn one credit
            (
                vec![33],
                36,
                // root: 2
                // when slot 2 was voted on in slot 9, it earned 11 credits
                10 + 11, // 21
            ),
            // Two votes in sequence
            (
                vec![34, 35],
                37,
                // root: 4
                // when slots 3 and 4 were voted on in slot 9, they earned 12 and 13 credits
                21 + 12 + 13, // 46
            ),
            // 3 votes in sequence
            (
                vec![36, 37, 38],
                39,
                // root: 7
                // slots 5, 6, and 7 earned 14, 15, and 16 credits when voted in slot 9
                46 + 14 + 15 + 16, // 91
            ),
            (
                // 30 votes in sequence
                vec![
                    39, 40, 41, 42, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57,
                    58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68,
                ],
                69,
                // root: 37
                // slot 8 was voted in slot 9, earning 16 credits
                // slots 9 - 25 earned 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 3, 4, 5, 6, 7, 8, and 9 credits when voted in
                //   slot 34
                // slot 26, 27, 28, 29, 30, 31 earned 10, 11, 12, 13, 14, 15 credits when voted in slot 34
                // slot 32 earned 15 credits when voted in slot 35
                // slot 33 earned 15 credits when voted in slot 36
                // slot 34 and 35 earned 15 and 16 credits when voted in slot 37
                // slot 36 and 37 earned 15 and 16 credits when voted in slot 39
                91 + 16
                    + 9 // * 1
                    + 2
                    + 3
                    + 4
                    + 5
                    + 6
                    + 7
                    + 8
                    + 9
                    + 10
                    + 11
                    + 12
                    + 13
                    + 14
                    + 15
                    + 15
                    + 15
                    + 15
                    + 16
                    + 15
                    + 16, // 327
            ),
            // 31 votes in sequence
            (
                vec![
                    69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88,
                    89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99,
                ],
                100,
                // root: 68
                // slot 38 earned 16 credits when voted in slot 39
                // slot 39 - 60 earned 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 3, 4, 5, 6, 7, 8, and 9 credits
                //   when voted in slot 69
                // slot 61, 62, 63, 64, 65, 66, 67, 68 earned 10, 11, 12, 13, 14, 15, 16, and 16 credits when
                //   voted in slot 69
                327 + 16
                    + 14 // * 1
                    + 2
                    + 3
                    + 4
                    + 5
                    + 6
                    + 7
                    + 8
                    + 9
                    + 10
                    + 11
                    + 12
                    + 13
                    + 14
                    + 15
                    + 16
                    + 16, // 508
            ),
            // Votes with expiry
            (
                vec![115, 116, 117, 118, 119, 120, 121, 122, 123, 124],
                130,
                // root: 74
                // slots 96 - 114 expire
                // slots 69 - 74 earned 1 credit when voted in slot 100
                508 + ((74 - 69) + 1), // 514
            ),
            // More votes with expiry of a large number of votes
            (
                vec![200, 201],
                202,
                // root: 74
                // slots 119 - 124 expire
                514,
            ),
            (
                vec![
                    202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 217,
                    218, 219, 220, 221, 222, 223, 224, 225, 226,
                ],
                227,
                // root: 95
                // slot 75 - 91 earned 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 3, 4, 5, 6, 7, 8, and 9 credits when voted in
                //   slot 100
                // slot 92, 93, 94, 95 earned 10, 11, 12, 13, credits when voted in slot 100
                514 + 9 + 2 + 3 + 4 + 5 + 6 + 7 + 8 + 9 + 10 + 11 + 12 + 13, // 613
            ),
            (
                vec![227, 228, 229, 230, 231, 232, 233, 234, 235, 236],
                237,
                // root: 205
                // slot 115 - 118 earned 3, 4, 5, and 6 credits when voted in slot 130
                // slot 200 and 201 earned 16 credits when voted in slot 202
                // slots 202 - 205 earned 1 credit when voted in slot 227
                613 + 3 + 4 + 5 + 6 + 16 + 16 + 1 + 1 + 1 + 1, // 667
            ),
        ];

        let new_vote_state = || match target_version {
            VoteStateTargetVersion::V3 => VoteStateHandler::default_v3(),
            VoteStateTargetVersion::V4 => VoteStateHandler::default_v4(),
        };

        // For each vote group, process all vote groups leading up to it and it itself, and ensure that the number of
        // credits earned is correct for both regular votes and vote state updates
        for i in 0..test_vote_groups.len() {
            // Create a new VoteStateV3 for vote transaction
            let mut vote_state_1 = new_vote_state();
            // Create a new VoteStateV3 for vote state update transaction
            let mut vote_state_2 = new_vote_state();
            test_vote_groups.iter().take(i + 1).for_each(|vote_group| {
                let vote = Vote {
                    slots: vote_group.0.clone(), //vote_group.0 is the set of slots to cast votes on
                    hash: Hash::new_unique(),
                    timestamp: None,
                };
                let slot_hashes: Vec<_> =
                    vote.slots.iter().rev().map(|x| (*x, vote.hash)).collect();
                assert_eq!(
                    process_vote(
                        &mut vote_state_1,
                        &vote,
                        &slot_hashes,
                        0,
                        vote_group.1, // vote_group.1 is the slot in which the vote was cast
                    ),
                    Ok(())
                );

                assert_eq!(
                    process_new_vote_state(
                        &mut vote_state_2,
                        vote_state_1.votes().clone(),
                        vote_state_1.root_slot(),
                        None,
                        0,
                        vote_group.1, // vote_group.1 is the slot in which the vote was cast
                    ),
                    Ok(())
                );
            });

            // Ensure that the credits earned is correct for both vote states
            let vote_group = &test_vote_groups[i];
            assert_eq!(vote_state_1.credits(), vote_group.2 as u64); // vote_group.2 is the expected number of credits
            assert_eq!(vote_state_2.credits(), vote_group.2 as u64); // vote_group.2 is the expected number of credits
        }
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_retroactive_voting_timely_credits(mut vote_state: VoteStateHandler) {
        // Each of the following (Vec<(Slot, int)>, Slot, Option<Slot>, u32) tuples gives the following data:
        // Vec<(Slot, int)> -- the set of slots and confirmation_counts that is the proposed vote state
        // Slot -- the slot in which the proposed vote state landed
        // Option<Slot> -- the root after processing the proposed vote state
        // u32 -- the credits after processing the proposed vote state
        #[allow(clippy::type_complexity)]
        let test_vote_state_updates: Vec<(Vec<(Slot, u32)>, Slot, Option<Slot>, u32)> = vec![
            // proposed vote state to set initial vote state
            (
                vec![(7, 4), (8, 3), (9, 2), (10, 1)],
                11,
                // root: none
                None,
                // no credits earned
                0,
            ),
            // proposed vote state to include the missing slots *prior to previously included slots*
            (
                vec![
                    (1, 10),
                    (2, 9),
                    (3, 8),
                    (4, 7),
                    (5, 6),
                    (6, 5),
                    (7, 4),
                    (8, 3),
                    (9, 2),
                    (10, 1),
                ],
                12,
                // root: none
                None,
                // no credits earned
                0,
            ),
            // Now a single proposed vote state which roots all of the slots from 1 - 10
            (
                vec![
                    (11, 31),
                    (12, 30),
                    (13, 29),
                    (14, 28),
                    (15, 27),
                    (16, 26),
                    (17, 25),
                    (18, 24),
                    (19, 23),
                    (20, 22),
                    (21, 21),
                    (22, 20),
                    (23, 19),
                    (24, 18),
                    (25, 17),
                    (26, 16),
                    (27, 15),
                    (28, 14),
                    (29, 13),
                    (30, 12),
                    (31, 11),
                    (32, 10),
                    (33, 9),
                    (34, 8),
                    (35, 7),
                    (36, 6),
                    (37, 5),
                    (38, 4),
                    (39, 3),
                    (40, 2),
                    (41, 1),
                ],
                42,
                // root: 10
                Some(10),
                // when slots 1 - 6 were voted on in slot 12, they earned 7, 8, 9, 10, 11, and 12 credits
                // when slots 7 - 10 were voted on in slot 11, they earned 14, 15, 16, and 16 credits
                7 + 8 + 9 + 10 + 11 + 12 + 14 + 15 + 16 + 16,
            ),
        ];

        // Process the vote state updates in sequence and ensure that the credits earned after each is processed is
        // correct
        test_vote_state_updates
            .iter()
            .for_each(|proposed_vote_state| {
                let new_state = proposed_vote_state
                    .0 // proposed_vote_state.0 is the set of slots and confirmation_counts that is the proposed vote state
                    .iter()
                    .map(|(slot, confirmation_count)| LandedVote {
                        latency: 0,
                        lockout: Lockout::new_with_confirmation_count(*slot, *confirmation_count),
                    })
                    .collect::<VecDeque<LandedVote>>();
                assert_eq!(
                    process_new_vote_state(
                        &mut vote_state,
                        new_state,
                        proposed_vote_state.2, // proposed_vote_state.2 is root after processing the proposed vote state
                        None,
                        0,
                        proposed_vote_state.1, // proposed_vote_state.1 is the slot in which the proposed vote state was applied
                    ),
                    Ok(())
                );

                // Ensure that the credits earned is correct
                assert_eq!(vote_state.credits(), proposed_vote_state.3 as u64);
            });
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_too_many_votes(mut vote_state1: VoteStateHandler) {
        let bad_votes: VecDeque<Lockout> = (0..=MAX_LOCKOUT_HISTORY)
            .map(|slot| {
                Lockout::new_with_confirmation_count(
                    slot as Slot,
                    (MAX_LOCKOUT_HISTORY - slot + 1) as u32,
                )
            })
            .collect();

        let current_epoch = vote_state1.current_epoch();
        assert_eq!(
            process_new_vote_state_from_lockouts(
                &mut vote_state1,
                bad_votes,
                None,
                None,
                current_epoch,
            ),
            Err(VoteError::TooManyVotes)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_state_root_rollback(mut vote_state1: VoteStateHandler) {
        for i in 0..MAX_LOCKOUT_HISTORY + 2 {
            process_slot_vote_unchecked(&mut vote_state1, i as Slot);
        }
        assert_eq!(vote_state1.root_slot().unwrap(), 1);

        // Update vote_state2 with a higher slot so that `process_new_vote_state`
        // doesn't panic.
        let mut vote_state2 = vote_state1.clone();
        process_slot_vote_unchecked(&mut vote_state2, MAX_LOCKOUT_HISTORY as Slot + 3);

        // Trying to set a lesser root should error
        let lesser_root = Some(0);

        let current_epoch = vote_state2.current_epoch();
        assert_eq!(
            process_new_vote_state(
                &mut vote_state1,
                vote_state2.votes().clone(),
                lesser_root,
                None,
                current_epoch,
                0,
            ),
            Err(VoteError::RootRollBack)
        );

        // Trying to set root to None should error
        let none_root = None;
        assert_eq!(
            process_new_vote_state(
                &mut vote_state1,
                vote_state2.votes().clone(),
                none_root,
                None,
                current_epoch,
                0,
            ),
            Err(VoteError::RootRollBack)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_state_zero_confirmations(mut vote_state1: VoteStateHandler) {
        let current_epoch = vote_state1.current_epoch();

        let bad_votes: VecDeque<Lockout> = vec![
            Lockout::new_with_confirmation_count(0, 0),
            Lockout::new_with_confirmation_count(1, 1),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            process_new_vote_state_from_lockouts(
                &mut vote_state1,
                bad_votes,
                None,
                None,
                current_epoch,
            ),
            Err(VoteError::ZeroConfirmations)
        );

        let bad_votes: VecDeque<Lockout> = vec![
            Lockout::new_with_confirmation_count(0, 2),
            Lockout::new_with_confirmation_count(1, 0),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            process_new_vote_state_from_lockouts(
                &mut vote_state1,
                bad_votes,
                None,
                None,
                current_epoch,
            ),
            Err(VoteError::ZeroConfirmations)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_state_confirmations_too_large(initial_vote_state: VoteStateHandler) {
        let mut vote_state1 = initial_vote_state.clone();
        let current_epoch = vote_state1.current_epoch();

        let good_votes: VecDeque<Lockout> = vec![Lockout::new_with_confirmation_count(
            0,
            MAX_LOCKOUT_HISTORY as u32,
        )]
        .into_iter()
        .collect();

        process_new_vote_state_from_lockouts(
            &mut vote_state1,
            good_votes,
            None,
            None,
            current_epoch,
        )
        .unwrap();

        let mut vote_state1 = initial_vote_state;
        let bad_votes: VecDeque<Lockout> = vec![Lockout::new_with_confirmation_count(
            0,
            MAX_LOCKOUT_HISTORY as u32 + 1,
        )]
        .into_iter()
        .collect();
        assert_eq!(
            process_new_vote_state_from_lockouts(
                &mut vote_state1,
                bad_votes,
                None,
                None,
                current_epoch,
            ),
            Err(VoteError::ConfirmationTooLarge)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_state_slot_smaller_than_root(mut vote_state1: VoteStateHandler) {
        let current_epoch = vote_state1.current_epoch();
        let root_slot = 5;

        let bad_votes: VecDeque<Lockout> = vec![
            Lockout::new_with_confirmation_count(root_slot, 2),
            Lockout::new_with_confirmation_count(root_slot + 1, 1),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            process_new_vote_state_from_lockouts(
                &mut vote_state1,
                bad_votes,
                Some(root_slot),
                None,
                current_epoch,
            ),
            Err(VoteError::SlotSmallerThanRoot)
        );

        let bad_votes: VecDeque<Lockout> = vec![
            Lockout::new_with_confirmation_count(root_slot - 1, 2),
            Lockout::new_with_confirmation_count(root_slot + 1, 1),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            process_new_vote_state_from_lockouts(
                &mut vote_state1,
                bad_votes,
                Some(root_slot),
                None,
                current_epoch,
            ),
            Err(VoteError::SlotSmallerThanRoot)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_state_slots_not_ordered(mut vote_state1: VoteStateHandler) {
        let current_epoch = vote_state1.current_epoch();

        let bad_votes: VecDeque<Lockout> = vec![
            Lockout::new_with_confirmation_count(1, 2),
            Lockout::new_with_confirmation_count(0, 1),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            process_new_vote_state_from_lockouts(
                &mut vote_state1,
                bad_votes,
                None,
                None,
                current_epoch,
            ),
            Err(VoteError::SlotsNotOrdered)
        );

        let bad_votes: VecDeque<Lockout> = vec![
            Lockout::new_with_confirmation_count(1, 2),
            Lockout::new_with_confirmation_count(1, 1),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            process_new_vote_state_from_lockouts(
                &mut vote_state1,
                bad_votes,
                None,
                None,
                current_epoch,
            ),
            Err(VoteError::SlotsNotOrdered)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_state_confirmations_not_ordered(mut vote_state1: VoteStateHandler) {
        let current_epoch = vote_state1.current_epoch();

        let bad_votes: VecDeque<Lockout> = vec![
            Lockout::new_with_confirmation_count(0, 1),
            Lockout::new_with_confirmation_count(1, 2),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            process_new_vote_state_from_lockouts(
                &mut vote_state1,
                bad_votes,
                None,
                None,
                current_epoch,
            ),
            Err(VoteError::ConfirmationsNotOrdered)
        );

        let bad_votes: VecDeque<Lockout> = vec![
            Lockout::new_with_confirmation_count(0, 1),
            Lockout::new_with_confirmation_count(1, 1),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            process_new_vote_state_from_lockouts(
                &mut vote_state1,
                bad_votes,
                None,
                None,
                current_epoch,
            ),
            Err(VoteError::ConfirmationsNotOrdered)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_state_new_vote_state_lockout_mismatch(
        mut vote_state1: VoteStateHandler,
    ) {
        let current_epoch = vote_state1.current_epoch();

        let bad_votes: VecDeque<Lockout> = vec![
            Lockout::new_with_confirmation_count(0, 2),
            Lockout::new_with_confirmation_count(7, 1),
        ]
        .into_iter()
        .collect();

        // Slot 7 should have expired slot 0
        assert_eq!(
            process_new_vote_state_from_lockouts(
                &mut vote_state1,
                bad_votes,
                None,
                None,
                current_epoch,
            ),
            Err(VoteError::NewVoteStateLockoutMismatch)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_state_confirmation_rollback(mut vote_state1: VoteStateHandler) {
        let current_epoch = vote_state1.current_epoch();
        let votes: VecDeque<Lockout> = vec![
            Lockout::new_with_confirmation_count(0, 4),
            Lockout::new_with_confirmation_count(1, 3),
        ]
        .into_iter()
        .collect();
        process_new_vote_state_from_lockouts(&mut vote_state1, votes, None, None, current_epoch)
            .unwrap();

        let votes: VecDeque<Lockout> = vec![
            Lockout::new_with_confirmation_count(0, 4),
            // Confirmation count lowered illegally
            Lockout::new_with_confirmation_count(1, 2),
            Lockout::new_with_confirmation_count(2, 1),
        ]
        .into_iter()
        .collect();
        // Should error because newer vote state should not have lower confirmation the same slot
        // 1
        assert_eq!(
            process_new_vote_state_from_lockouts(
                &mut vote_state1,
                votes,
                None,
                None,
                current_epoch,
            ),
            Err(VoteError::ConfirmationRollBack)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_state_root_progress(mut vote_state1: VoteStateHandler) {
        for i in 0..MAX_LOCKOUT_HISTORY {
            process_slot_vote_unchecked(&mut vote_state1, i as u64);
        }

        assert!(vote_state1.root_slot().is_none());
        let mut vote_state2 = vote_state1.clone();

        // 1) Try to update `vote_state1` with no root,
        // to `vote_state2`, which has a new root, should succeed.
        //
        // 2) Then try to update`vote_state1` with an existing root,
        // to `vote_state2`, which has a newer root, which
        // should succeed.
        for new_vote in MAX_LOCKOUT_HISTORY + 1..=MAX_LOCKOUT_HISTORY + 2 {
            process_slot_vote_unchecked(&mut vote_state2, new_vote as Slot);
            assert_ne!(vote_state1.root_slot(), vote_state2.root_slot());

            process_new_vote_state(
                &mut vote_state1,
                vote_state2.votes().clone(),
                vote_state2.root_slot(),
                None,
                vote_state2.current_epoch(),
                0,
            )
            .unwrap();

            assert_eq!(vote_state1, vote_state2);
        }
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_state_same_slot_but_not_common_ancestor(
        initial_vote_state: VoteStateHandler,
    ) {
        // It might be possible that during the switch from old vote instructions
        // to new vote instructions, new_state contains votes for slots LESS
        // than the current state, for instance:
        //
        // Current on-chain state: 1, 5
        // New state: 1, 2 (lockout: 4), 3, 5, 7
        //
        // Imagine the validator made two of these votes:
        // 1) The first vote {1, 2, 3} didn't land in the old state, but didn't
        // land on chain
        // 2) A second vote {1, 2, 5} was then submitted, which landed
        //
        //
        // 2 is not popped off in the local tower because 3 doubled the lockout.
        // However, 3 did not land in the on-chain state, so the vote {1, 2, 6}
        // will immediately pop off 2.

        // Construct on-chain vote state
        let mut vote_state1 = initial_vote_state.clone();
        process_slot_votes_unchecked(&mut vote_state1, &[1, 2, 5]);
        assert_eq!(
            vote_state1
                .votes()
                .iter()
                .map(|vote| vote.slot())
                .collect::<Vec<Slot>>(),
            vec![1, 5]
        );

        // Construct local tower state
        let mut vote_state2 = initial_vote_state;
        process_slot_votes_unchecked(&mut vote_state2, &[1, 2, 3, 5, 7]);
        assert_eq!(
            vote_state2
                .votes()
                .iter()
                .map(|vote| vote.slot())
                .collect::<Vec<Slot>>(),
            vec![1, 2, 3, 5, 7]
        );

        // See that on-chain vote state can update properly
        process_new_vote_state(
            &mut vote_state1,
            vote_state2.votes().clone(),
            vote_state2.root_slot(),
            None,
            vote_state2.current_epoch(),
            0,
        )
        .unwrap();

        assert_eq!(vote_state1, vote_state2);
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_state_lockout_violation(initial_vote_state: VoteStateHandler) {
        // Construct on-chain vote state
        let mut vote_state1 = initial_vote_state.clone();
        process_slot_votes_unchecked(&mut vote_state1, &[1, 2, 4, 5]);
        assert_eq!(
            vote_state1
                .votes()
                .iter()
                .map(|vote| vote.slot())
                .collect::<Vec<Slot>>(),
            vec![1, 2, 4, 5]
        );

        // Construct conflicting tower state. Vote 4 is missing,
        // but 5 should not have popped off vote 4.
        let mut vote_state2 = initial_vote_state;
        process_slot_votes_unchecked(&mut vote_state2, &[1, 2, 3, 5, 7]);
        assert_eq!(
            vote_state2
                .votes()
                .iter()
                .map(|vote| vote.slot())
                .collect::<Vec<Slot>>(),
            vec![1, 2, 3, 5, 7]
        );

        // See that on-chain vote state can update properly
        assert_eq!(
            process_new_vote_state(
                &mut vote_state1,
                vote_state2.votes().clone(),
                vote_state2.root_slot(),
                None,
                vote_state2.current_epoch(),
                0,
            ),
            Err(VoteError::LockoutConflict)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_state_lockout_violation2(initial_vote_state: VoteStateHandler) {
        // Construct on-chain vote state
        let mut vote_state1 = initial_vote_state.clone();
        process_slot_votes_unchecked(&mut vote_state1, &[1, 2, 5, 6, 7]);
        assert_eq!(
            vote_state1
                .votes()
                .iter()
                .map(|vote| vote.slot())
                .collect::<Vec<Slot>>(),
            vec![1, 5, 6, 7]
        );

        // Construct a new vote state. Violates on-chain state because 8
        // should not have popped off 7
        let mut vote_state2 = initial_vote_state;
        process_slot_votes_unchecked(&mut vote_state2, &[1, 2, 3, 5, 6, 8]);
        assert_eq!(
            vote_state2
                .votes()
                .iter()
                .map(|vote| vote.slot())
                .collect::<Vec<Slot>>(),
            vec![1, 2, 3, 5, 6, 8]
        );

        // Both vote states contain `5`, but `5` is not part of the common prefix
        // of both vote states. However, the violation should still be detected.
        assert_eq!(
            process_new_vote_state(
                &mut vote_state1,
                vote_state2.votes().clone(),
                vote_state2.root_slot(),
                None,
                vote_state2.current_epoch(),
                0,
            ),
            Err(VoteError::LockoutConflict)
        );
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_state_expired_ancestor_not_removed(mut vote_state1: VoteStateHandler) {
        // Construct on-chain vote state
        process_slot_votes_unchecked(&mut vote_state1, &[1, 2, 3, 9]);
        assert_eq!(
            vote_state1
                .votes()
                .iter()
                .map(|vote| vote.slot())
                .collect::<Vec<Slot>>(),
            vec![1, 9]
        );

        // Example: {1: lockout 8, 9: lockout 2}, vote on 10 will not pop off 1
        // because 9 is not popped off yet
        let mut vote_state2 = vote_state1.clone();
        process_slot_vote_unchecked(&mut vote_state2, 10);

        // Slot 1 has been expired by 10, but is kept alive by its descendant
        // 9 which has not been expired yet.
        assert_eq!(vote_state2.votes()[0].slot(), 1);
        assert_eq!(vote_state2.votes()[0].lockout.last_locked_out_slot(), 9);
        assert_eq!(
            vote_state2
                .votes()
                .iter()
                .map(|vote| vote.slot())
                .collect::<Vec<Slot>>(),
            vec![1, 9, 10]
        );

        // Should be able to update vote_state1
        process_new_vote_state(
            &mut vote_state1,
            vote_state2.votes().clone(),
            vote_state2.root_slot(),
            None,
            vote_state2.current_epoch(),
            0,
        )
        .unwrap();
        assert_eq!(vote_state1, vote_state2,);
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_process_new_vote_current_state_contains_bigger_slots(
        mut vote_state1: VoteStateHandler,
    ) {
        process_slot_votes_unchecked(&mut vote_state1, &[6, 7, 8]);
        assert_eq!(
            vote_state1
                .votes()
                .iter()
                .map(|vote| vote.slot())
                .collect::<Vec<Slot>>(),
            vec![6, 7, 8]
        );

        // Try to process something with lockout violations
        let bad_votes: VecDeque<Lockout> = vec![
            Lockout::new_with_confirmation_count(2, 5),
            // Slot 14 could not have popped off slot 6 yet
            Lockout::new_with_confirmation_count(14, 1),
        ]
        .into_iter()
        .collect();
        let root = Some(1);

        let current_epoch = vote_state1.current_epoch();
        assert_eq!(
            process_new_vote_state_from_lockouts(
                &mut vote_state1,
                bad_votes,
                root,
                None,
                current_epoch,
            ),
            Err(VoteError::LockoutConflict)
        );

        let good_votes: VecDeque<LandedVote> = vec![
            Lockout::new_with_confirmation_count(2, 5).into(),
            Lockout::new_with_confirmation_count(15, 1).into(),
        ]
        .into_iter()
        .collect();

        let current_epoch = vote_state1.current_epoch();
        process_new_vote_state(
            &mut vote_state1,
            good_votes.clone(),
            root,
            None,
            current_epoch,
            0,
        )
        .unwrap();
        assert_eq!(*vote_state1.votes(), good_votes);
    }

    #[test_case(VoteStateHandler::default_v3() ; "VoteStateV3")]
    #[test_case(VoteStateHandler::default_v4() ; "VoteStateV4")]
    fn test_filter_old_votes(mut vote_state: VoteStateHandler) {
        let old_vote_slot = 1;
        let vote = Vote::new(vec![old_vote_slot], Hash::default());

        // Vote with all slots that are all older than the SlotHashes history should
        // error with `VotesTooOldAllFiltered`
        let slot_hashes = vec![(3, Hash::new_unique()), (2, Hash::new_unique())];
        assert_eq!(
            process_vote(&mut vote_state, &vote, &slot_hashes, 0, 0),
            Err(VoteError::VotesTooOldAllFiltered)
        );

        // Vote with only some slots older than the SlotHashes history should
        // filter out those older slots
        let vote_slot = 2;
        let vote_slot_hash = slot_hashes
            .iter()
            .find(|(slot, _hash)| *slot == vote_slot)
            .unwrap()
            .1;

        let vote = Vote::new(vec![old_vote_slot, vote_slot], vote_slot_hash);
        process_vote(&mut vote_state, &vote, &slot_hashes, 0, 0).unwrap();
        assert_eq!(
            vote_state
                .votes()
                .iter()
                .map(|vote| vote.lockout)
                .collect::<Vec<Lockout>>(),
            vec![Lockout::new_with_confirmation_count(vote_slot, 1)]
        );
    }

    fn build_slot_hashes(slots: Vec<Slot>) -> Vec<(Slot, Hash)> {
        slots
            .iter()
            .rev()
            .map(|x| (*x, Hash::new_unique()))
            .collect()
    }

    fn build_vote_state(
        target_version: VoteStateTargetVersion,
        vote_slots: Vec<Slot>,
        slot_hashes: &[(Slot, Hash)],
    ) -> VoteStateHandler {
        let mut vote_state = match target_version {
            VoteStateTargetVersion::V3 => VoteStateHandler::default_v3(),
            VoteStateTargetVersion::V4 => VoteStateHandler::default_v4(),
        };

        if !vote_slots.is_empty() {
            let vote_hash = slot_hashes
                .iter()
                .find(|(slot, _hash)| slot == vote_slots.last().unwrap())
                .unwrap()
                .1;
            let vote = Vote::new(vote_slots, vote_hash);
            process_vote_unfiltered(&mut vote_state, &vote.slots, &vote, slot_hashes, 0, 0)
                .unwrap();
        }

        vote_state
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_check_and_filter_proposed_vote_state_empty(target_version: VoteStateTargetVersion) {
        let empty_slot_hashes = build_slot_hashes(vec![]);
        let empty_vote_state = build_vote_state(target_version, vec![], &empty_slot_hashes);

        // Test with empty TowerSync, should return EmptySlots error
        let mut tower_sync = TowerSync::from(vec![]);
        assert_eq!(
            check_and_filter_proposed_vote_state(
                &empty_vote_state,
                &mut tower_sync.lockouts,
                &mut tower_sync.root,
                tower_sync.hash,
                &empty_slot_hashes
            ),
            Err(VoteError::EmptySlots),
        );

        // Test with non-empty TowerSync, should return SlotsMismatch since nothing exists in SlotHashes
        let mut tower_sync = TowerSync::from(vec![(0, 1)]);
        assert_eq!(
            check_and_filter_proposed_vote_state(
                &empty_vote_state,
                &mut tower_sync.lockouts,
                &mut tower_sync.root,
                tower_sync.hash,
                &empty_slot_hashes
            ),
            Err(VoteError::SlotsMismatch),
        );
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_check_and_filter_proposed_vote_state_too_old(target_version: VoteStateTargetVersion) {
        let slot_hashes = build_slot_hashes(vec![1, 2, 3, 4]);
        let latest_vote = 4;
        let vote_state = build_vote_state(target_version, vec![1, 2, 3, latest_vote], &slot_hashes);

        // Test with a vote for a slot less than the latest vote in the vote_state,
        // should return error `VoteTooOld`
        let mut tower_sync = TowerSync::from(vec![(latest_vote, 1)]);
        assert_eq!(
            check_and_filter_proposed_vote_state(
                &vote_state,
                &mut tower_sync.lockouts,
                &mut tower_sync.root,
                tower_sync.hash,
                &slot_hashes
            ),
            Err(VoteError::VoteTooOld),
        );

        // Test with a vote state update where the latest slot `X` in the update is
        // 1) Less than the earliest slot in slot_hashes history, AND
        // 2) `X` > latest_vote
        let earliest_slot_in_history = latest_vote + 2;
        let slot_hashes = build_slot_hashes(vec![earliest_slot_in_history]);
        let mut tower_sync = TowerSync::from(vec![(earliest_slot_in_history - 1, 1)]);
        assert_eq!(
            check_and_filter_proposed_vote_state(
                &vote_state,
                &mut tower_sync.lockouts,
                &mut tower_sync.root,
                tower_sync.hash,
                &slot_hashes
            ),
            Err(VoteError::VoteTooOld),
        );
    }

    fn run_test_check_and_filter_proposed_vote_state_older_than_history_root(
        target_version: VoteStateTargetVersion,
        earliest_slot_in_history: Slot,
        current_vote_state_slots: Vec<Slot>,
        current_vote_state_root: Option<Slot>,
        proposed_slots_and_lockouts: Vec<(Slot, u32)>,
        proposed_root: Slot,
        expected_root: Option<Slot>,
        expected_vote_state: Vec<Lockout>,
    ) {
        assert!(proposed_root < earliest_slot_in_history);
        assert_eq!(
            expected_root,
            current_vote_state_slots
                .iter()
                .rev()
                .find(|slot| **slot <= proposed_root)
                .cloned()
        );
        let latest_slot_in_history = proposed_slots_and_lockouts
            .last()
            .unwrap()
            .0
            .max(earliest_slot_in_history);
        let mut slot_hashes = build_slot_hashes(
            (current_vote_state_slots.first().copied().unwrap_or(0)..=latest_slot_in_history)
                .collect::<Vec<Slot>>(),
        );

        let mut vote_state =
            build_vote_state(target_version, current_vote_state_slots, &slot_hashes);
        vote_state.set_root_slot(current_vote_state_root);

        slot_hashes.retain(|slot| slot.0 >= earliest_slot_in_history);
        assert!(!proposed_slots_and_lockouts.is_empty());
        let proposed_hash = slot_hashes
            .iter()
            .find(|(slot, _hash)| *slot == proposed_slots_and_lockouts.last().unwrap().0)
            .unwrap()
            .1;

        // Test with a `TowerSync` where the root is less than `earliest_slot_in_history`.
        // Root slot in the `TowerSync` should be updated to match the root slot in the
        // current vote state
        let mut tower_sync = TowerSync::from(proposed_slots_and_lockouts);
        tower_sync.hash = proposed_hash;
        tower_sync.root = Some(proposed_root);
        check_and_filter_proposed_vote_state(
            &vote_state,
            &mut tower_sync.lockouts,
            &mut tower_sync.root,
            tower_sync.hash,
            &slot_hashes,
        )
        .unwrap();
        assert_eq!(tower_sync.root, expected_root);

        // The proposed root slot should become the biggest slot in the current vote state less than
        // `earliest_slot_in_history`.
        assert!(
            do_process_tower_sync(&mut vote_state, &slot_hashes, 0, 0, tower_sync.clone(),).is_ok()
        );
        assert_eq!(vote_state.root_slot(), expected_root);
        assert_eq!(
            vote_state
                .votes()
                .iter()
                .map(|vote| vote.lockout)
                .collect::<Vec<Lockout>>(),
            expected_vote_state,
        );
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_check_and_filter_proposed_vote_state_older_than_history_root(
        target_version: VoteStateTargetVersion,
    ) {
        // Test when `proposed_root` is in `current_vote_state_slots` but it's not the latest
        // slot
        let earliest_slot_in_history = 5;
        let current_vote_state_slots: Vec<Slot> = vec![1, 2, 3, 4];
        let current_vote_state_root = None;
        let proposed_slots_and_lockouts = vec![(5, 1)];
        let proposed_root = 4;
        let expected_root = Some(4);
        let expected_vote_state = vec![Lockout::new_with_confirmation_count(5, 1)];
        run_test_check_and_filter_proposed_vote_state_older_than_history_root(
            target_version,
            earliest_slot_in_history,
            current_vote_state_slots,
            current_vote_state_root,
            proposed_slots_and_lockouts,
            proposed_root,
            expected_root,
            expected_vote_state,
        );

        // Test when `proposed_root` is in `current_vote_state_slots` but it's not the latest
        // slot and the `current_vote_state_root.is_some()`.
        let earliest_slot_in_history = 5;
        let current_vote_state_slots: Vec<Slot> = vec![1, 2, 3, 4];
        let current_vote_state_root = Some(0);
        let proposed_slots_and_lockouts = vec![(5, 1)];
        let proposed_root = 4;
        let expected_root = Some(4);
        let expected_vote_state = vec![Lockout::new_with_confirmation_count(5, 1)];
        run_test_check_and_filter_proposed_vote_state_older_than_history_root(
            target_version,
            earliest_slot_in_history,
            current_vote_state_slots,
            current_vote_state_root,
            proposed_slots_and_lockouts,
            proposed_root,
            expected_root,
            expected_vote_state,
        );

        // Test when `proposed_root` is in `current_vote_state_slots` but it's not the latest
        // slot
        let earliest_slot_in_history = 5;
        let current_vote_state_slots: Vec<Slot> = vec![1, 2, 3, 4];
        let current_vote_state_root = Some(0);
        let proposed_slots_and_lockouts = vec![(4, 2), (5, 1)];
        let proposed_root = 3;
        let expected_root = Some(3);
        let expected_vote_state = vec![
            Lockout::new_with_confirmation_count(4, 2),
            Lockout::new_with_confirmation_count(5, 1),
        ];
        run_test_check_and_filter_proposed_vote_state_older_than_history_root(
            target_version,
            earliest_slot_in_history,
            current_vote_state_slots,
            current_vote_state_root,
            proposed_slots_and_lockouts,
            proposed_root,
            expected_root,
            expected_vote_state,
        );

        // Test when `proposed_root` is not in `current_vote_state_slots`
        let earliest_slot_in_history = 5;
        let current_vote_state_slots: Vec<Slot> = vec![1, 2, 4];
        let current_vote_state_root = Some(0);
        let proposed_slots_and_lockouts = vec![(4, 2), (5, 1)];
        let proposed_root = 3;
        let expected_root = Some(2);
        let expected_vote_state = vec![
            Lockout::new_with_confirmation_count(4, 2),
            Lockout::new_with_confirmation_count(5, 1),
        ];
        run_test_check_and_filter_proposed_vote_state_older_than_history_root(
            target_version,
            earliest_slot_in_history,
            current_vote_state_slots,
            current_vote_state_root,
            proposed_slots_and_lockouts,
            proposed_root,
            expected_root,
            expected_vote_state,
        );

        // Test when the `proposed_root` is smaller than all the slots in
        // `current_vote_state_slots`, no roots should be set.
        let earliest_slot_in_history = 4;
        let current_vote_state_slots: Vec<Slot> = vec![3, 4];
        let current_vote_state_root = None;
        let proposed_slots_and_lockouts = vec![(3, 3), (4, 2), (5, 1)];
        let proposed_root = 2;
        let expected_root = None;
        let expected_vote_state = vec![
            Lockout::new_with_confirmation_count(3, 3),
            Lockout::new_with_confirmation_count(4, 2),
            Lockout::new_with_confirmation_count(5, 1),
        ];
        run_test_check_and_filter_proposed_vote_state_older_than_history_root(
            target_version,
            earliest_slot_in_history,
            current_vote_state_slots,
            current_vote_state_root,
            proposed_slots_and_lockouts,
            proposed_root,
            expected_root,
            expected_vote_state,
        );

        // Test when `current_vote_state_slots` is empty, no roots should be set
        let earliest_slot_in_history = 4;
        let current_vote_state_slots: Vec<Slot> = vec![];
        let current_vote_state_root = None;
        let proposed_slots_and_lockouts = vec![(5, 1)];
        let proposed_root = 2;
        let expected_root = None;
        let expected_vote_state = vec![Lockout::new_with_confirmation_count(5, 1)];
        run_test_check_and_filter_proposed_vote_state_older_than_history_root(
            target_version,
            earliest_slot_in_history,
            current_vote_state_slots,
            current_vote_state_root,
            proposed_slots_and_lockouts,
            proposed_root,
            expected_root,
            expected_vote_state,
        );
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_check_and_filter_proposed_vote_state_slots_not_ordered(
        target_version: VoteStateTargetVersion,
    ) {
        let slot_hashes = build_slot_hashes(vec![1, 2, 3, 4]);
        let vote_state = build_vote_state(target_version, vec![1], &slot_hashes);

        // Test with a `TowerSync` where the slots are out of order
        let vote_slot = 3;
        let vote_slot_hash = slot_hashes
            .iter()
            .find(|(slot, _hash)| *slot == vote_slot)
            .unwrap()
            .1;
        let mut tower_sync = TowerSync::from(vec![(2, 2), (1, 3), (vote_slot, 1)]);
        tower_sync.hash = vote_slot_hash;
        assert_eq!(
            check_and_filter_proposed_vote_state(
                &vote_state,
                &mut tower_sync.lockouts,
                &mut tower_sync.root,
                tower_sync.hash,
                &slot_hashes
            ),
            Err(VoteError::SlotsNotOrdered),
        );

        // Test with a `TowerSync` where there are multiples of the same slot
        let mut tower_sync = TowerSync::from(vec![(2, 2), (2, 2), (vote_slot, 1)]);
        tower_sync.hash = vote_slot_hash;
        assert_eq!(
            check_and_filter_proposed_vote_state(
                &vote_state,
                &mut tower_sync.lockouts,
                &mut tower_sync.root,
                tower_sync.hash,
                &slot_hashes
            ),
            Err(VoteError::SlotsNotOrdered),
        );
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_check_and_filter_proposed_vote_state_older_than_history_slots_filtered(
        target_version: VoteStateTargetVersion,
    ) {
        let slot_hashes = build_slot_hashes(vec![1, 2, 3, 4]);
        let mut vote_state = build_vote_state(target_version, vec![1, 2, 3, 4], &slot_hashes);

        // Test with a `TowerSync` where there:
        // 1) Exists a slot less than `earliest_slot_in_history`
        // 2) This slot does not exist in the vote state already
        // This slot should be filtered out
        let earliest_slot_in_history = 11;
        let slot_hashes = build_slot_hashes(vec![earliest_slot_in_history, 12, 13, 14]);
        let vote_slot = 12;
        let vote_slot_hash = slot_hashes
            .iter()
            .find(|(slot, _hash)| *slot == vote_slot)
            .unwrap()
            .1;
        let missing_older_than_history_slot = earliest_slot_in_history - 1;
        let mut tower_sync = TowerSync::from(vec![
            (1, 4),
            (missing_older_than_history_slot, 2),
            (vote_slot, 3),
        ]);
        tower_sync.hash = vote_slot_hash;
        check_and_filter_proposed_vote_state(
            &vote_state,
            &mut tower_sync.lockouts,
            &mut tower_sync.root,
            tower_sync.hash,
            &slot_hashes,
        )
        .unwrap();

        // Check the earlier slot was filtered out
        assert_eq!(
            tower_sync
                .clone()
                .lockouts
                .into_iter()
                .collect::<Vec<Lockout>>(),
            vec![
                Lockout::new_with_confirmation_count(1, 4),
                Lockout::new_with_confirmation_count(vote_slot, 3)
            ]
        );
        assert!(do_process_tower_sync(&mut vote_state, &slot_hashes, 0, 0, tower_sync,).is_ok());
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_check_and_filter_proposed_vote_state_older_than_history_slots_not_filtered(
        target_version: VoteStateTargetVersion,
    ) {
        let slot_hashes = build_slot_hashes(vec![4]);
        let mut vote_state = build_vote_state(target_version, vec![4], &slot_hashes);

        // Test with a `TowerSync` where there:
        // 1) Exists a slot less than `earliest_slot_in_history`
        // 2) This slot exists in the vote state already
        // This slot should *NOT* be filtered out
        let earliest_slot_in_history = 11;
        let slot_hashes = build_slot_hashes(vec![earliest_slot_in_history, 12, 13, 14]);
        let vote_slot = 12;
        let vote_slot_hash = slot_hashes
            .iter()
            .find(|(slot, _hash)| *slot == vote_slot)
            .unwrap()
            .1;
        let existing_older_than_history_slot = 4;
        let mut tower_sync =
            TowerSync::from(vec![(existing_older_than_history_slot, 3), (vote_slot, 2)]);
        tower_sync.hash = vote_slot_hash;
        check_and_filter_proposed_vote_state(
            &vote_state,
            &mut tower_sync.lockouts,
            &mut tower_sync.root,
            tower_sync.hash,
            &slot_hashes,
        )
        .unwrap();
        // Check the earlier slot was *NOT* filtered out
        assert_eq!(tower_sync.lockouts.len(), 2);
        assert_eq!(
            tower_sync
                .clone()
                .lockouts
                .into_iter()
                .collect::<Vec<Lockout>>(),
            vec![
                Lockout::new_with_confirmation_count(existing_older_than_history_slot, 3),
                Lockout::new_with_confirmation_count(vote_slot, 2)
            ]
        );
        assert!(do_process_tower_sync(&mut vote_state, &slot_hashes, 0, 0, tower_sync,).is_ok());
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_check_and_filter_proposed_vote_state_older_than_history_slots_filtered_and_not_filtered(
        target_version: VoteStateTargetVersion,
    ) {
        let slot_hashes = build_slot_hashes(vec![6]);
        let mut vote_state = build_vote_state(target_version, vec![6], &slot_hashes);

        // Test with a `TowerSync` where there exists both a slot:
        // 1) Less than `earliest_slot_in_history`
        // 2) This slot exists in the vote state already
        // which should not be filtered
        //
        // AND a slot that
        //
        // 1) Less than `earliest_slot_in_history`
        // 2) This slot does not exist in the vote state already
        // which should be filtered
        let earliest_slot_in_history = 11;
        let slot_hashes = build_slot_hashes(vec![earliest_slot_in_history, 12, 13, 14]);
        let vote_slot = 14;
        let vote_slot_hash = slot_hashes
            .iter()
            .find(|(slot, _hash)| *slot == vote_slot)
            .unwrap()
            .1;

        let missing_older_than_history_slot = 4;
        let existing_older_than_history_slot = 6;

        let mut tower_sync = TowerSync::from(vec![
            (missing_older_than_history_slot, 4),
            (existing_older_than_history_slot, 3),
            (12, 2),
            (vote_slot, 1),
        ]);
        tower_sync.hash = vote_slot_hash;
        check_and_filter_proposed_vote_state(
            &vote_state,
            &mut tower_sync.lockouts,
            &mut tower_sync.root,
            tower_sync.hash,
            &slot_hashes,
        )
        .unwrap();
        assert_eq!(tower_sync.lockouts.len(), 3);
        assert_eq!(
            tower_sync
                .clone()
                .lockouts
                .into_iter()
                .collect::<Vec<Lockout>>(),
            vec![
                Lockout::new_with_confirmation_count(existing_older_than_history_slot, 3),
                Lockout::new_with_confirmation_count(12, 2),
                Lockout::new_with_confirmation_count(vote_slot, 1)
            ]
        );
        assert!(do_process_tower_sync(&mut vote_state, &slot_hashes, 0, 0, tower_sync,).is_ok());
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_check_and_filter_proposed_vote_state_slot_not_on_fork(
        target_version: VoteStateTargetVersion,
    ) {
        let slot_hashes = build_slot_hashes(vec![2, 4, 6, 8]);
        let vote_state = build_vote_state(target_version, vec![2, 4, 6], &slot_hashes);

        // Test with a `TowerSync` where there:
        // 1) Exists a slot not in the slot hashes history
        // 2) The slot is greater than the earliest slot in the history
        // Thus this slot is not part of the fork and the update should be rejected
        // with error `SlotsMismatch`
        let missing_vote_slot = 3;

        // Have to vote for a slot greater than the last vote in the vote state to avoid VoteTooOld
        // errors
        let vote_slot = vote_state.votes().back().unwrap().slot() + 2;
        let vote_slot_hash = slot_hashes
            .iter()
            .find(|(slot, _hash)| *slot == vote_slot)
            .unwrap()
            .1;
        let mut tower_sync = TowerSync::from(vec![(missing_vote_slot, 2), (vote_slot, 3)]);
        tower_sync.hash = vote_slot_hash;
        assert_eq!(
            check_and_filter_proposed_vote_state(
                &vote_state,
                &mut tower_sync.lockouts,
                &mut tower_sync.root,
                tower_sync.hash,
                &slot_hashes
            ),
            Err(VoteError::SlotsMismatch),
        );

        // Test where some earlier vote slots exist in the history, but others don't
        let missing_vote_slot = 7;
        let mut tower_sync = TowerSync::from(vec![
            (2, 5),
            (4, 4),
            (6, 3),
            (missing_vote_slot, 2),
            (vote_slot, 1),
        ]);
        tower_sync.hash = vote_slot_hash;
        assert_eq!(
            check_and_filter_proposed_vote_state(
                &vote_state,
                &mut tower_sync.lockouts,
                &mut tower_sync.root,
                tower_sync.hash,
                &slot_hashes
            ),
            Err(VoteError::SlotsMismatch),
        );
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_check_and_filter_proposed_vote_state_root_on_different_fork(
        target_version: VoteStateTargetVersion,
    ) {
        let slot_hashes = build_slot_hashes(vec![2, 4, 6, 8]);
        let vote_state = build_vote_state(target_version, vec![6], &slot_hashes);

        // Test with a `TowerSync` where:
        // 1) The root is not present in slot hashes history
        // 2) The slot is greater than the earliest slot in the history
        // Thus this slot is not part of the fork and the update should be rejected
        // with error `RootOnDifferentFork`
        let new_root = 3;

        // Have to vote for a slot greater than the last vote in the vote state to avoid VoteTooOld
        // errors, but also this slot must be present in SlotHashes
        let vote_slot = 8;
        assert_eq!(vote_slot, slot_hashes.first().unwrap().0);
        let vote_slot_hash = slot_hashes
            .iter()
            .find(|(slot, _hash)| *slot == vote_slot)
            .unwrap()
            .1;
        let mut tower_sync = TowerSync::from(vec![(vote_slot, 1)]);
        tower_sync.hash = vote_slot_hash;
        tower_sync.root = Some(new_root);
        assert_eq!(
            check_and_filter_proposed_vote_state(
                &vote_state,
                &mut tower_sync.lockouts,
                &mut tower_sync.root,
                tower_sync.hash,
                &slot_hashes
            ),
            Err(VoteError::RootOnDifferentFork),
        );
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_check_and_filter_proposed_vote_state_slot_newer_than_slot_history(
        target_version: VoteStateTargetVersion,
    ) {
        let slot_hashes = build_slot_hashes(vec![2, 4, 6, 8, 10]);
        let vote_state = build_vote_state(target_version, vec![2, 4, 6], &slot_hashes);

        // Test with a `TowerSync` where there:
        // 1) The last slot in the update is a slot not in the slot hashes history
        // 2) The slot is greater than the newest slot in the slot history
        // Thus this slot is not part of the fork and the update should be rejected
        // with error `SlotsMismatch`
        let missing_vote_slot = slot_hashes.first().unwrap().0 + 1;
        let vote_slot_hash = Hash::new_unique();
        let mut tower_sync = TowerSync::from(vec![(8, 2), (missing_vote_slot, 3)]);
        tower_sync.hash = vote_slot_hash;
        assert_eq!(
            check_and_filter_proposed_vote_state(
                &vote_state,
                &mut tower_sync.lockouts,
                &mut tower_sync.root,
                tower_sync.hash,
                &slot_hashes
            ),
            Err(VoteError::SlotsMismatch),
        );
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_check_and_filter_proposed_vote_state_slot_all_slot_hashes_in_update_ok(
        target_version: VoteStateTargetVersion,
    ) {
        let slot_hashes = build_slot_hashes(vec![2, 4, 6, 8]);
        let mut vote_state = build_vote_state(target_version, vec![2, 4, 6], &slot_hashes);

        // Test with a `TowerSync` where every slot in the history is
        // in the update

        // Have to vote for a slot greater than the last vote in the vote state to avoid VoteTooOld
        // errors
        let vote_slot = vote_state.votes().back().unwrap().slot() + 2;
        let vote_slot_hash = slot_hashes
            .iter()
            .find(|(slot, _hash)| *slot == vote_slot)
            .unwrap()
            .1;
        let mut tower_sync = TowerSync::from(vec![(2, 4), (4, 3), (6, 2), (vote_slot, 1)]);
        tower_sync.hash = vote_slot_hash;
        check_and_filter_proposed_vote_state(
            &vote_state,
            &mut tower_sync.lockouts,
            &mut tower_sync.root,
            tower_sync.hash,
            &slot_hashes,
        )
        .unwrap();

        // Nothing in the update should have been filtered out
        assert_eq!(
            tower_sync
                .clone()
                .lockouts
                .into_iter()
                .collect::<Vec<Lockout>>(),
            vec![
                Lockout::new_with_confirmation_count(2, 4),
                Lockout::new_with_confirmation_count(4, 3),
                Lockout::new_with_confirmation_count(6, 2),
                Lockout::new_with_confirmation_count(vote_slot, 1)
            ]
        );

        assert!(do_process_tower_sync(&mut vote_state, &slot_hashes, 0, 0, tower_sync,).is_ok());
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_check_and_filter_proposed_vote_state_slot_some_slot_hashes_in_update_ok(
        target_version: VoteStateTargetVersion,
    ) {
        let slot_hashes = build_slot_hashes(vec![2, 4, 6, 8, 10]);
        let mut vote_state = build_vote_state(target_version, vec![6], &slot_hashes);

        // Test with a `TowerSync` where only some slots in the history are
        // in the update, and others slots in the history are missing.

        // Have to vote for a slot greater than the last vote in the vote state to avoid VoteTooOld
        // errors
        let vote_slot = vote_state.votes().back().unwrap().slot() + 2;
        let vote_slot_hash = slot_hashes
            .iter()
            .find(|(slot, _hash)| *slot == vote_slot)
            .unwrap()
            .1;
        let mut tower_sync = TowerSync::from(vec![(4, 2), (vote_slot, 1)]);
        tower_sync.hash = vote_slot_hash;
        check_and_filter_proposed_vote_state(
            &vote_state,
            &mut tower_sync.lockouts,
            &mut tower_sync.root,
            tower_sync.hash,
            &slot_hashes,
        )
        .unwrap();

        // Nothing in the update should have been filtered out
        assert_eq!(
            tower_sync
                .clone()
                .lockouts
                .into_iter()
                .collect::<Vec<Lockout>>(),
            vec![
                Lockout::new_with_confirmation_count(4, 2),
                Lockout::new_with_confirmation_count(vote_slot, 1)
            ]
        );

        // Because 6 from the original VoteStateV3
        // should not have been popped off in the proposed state,
        // we should get a lockout conflict
        assert_eq!(
            do_process_tower_sync(&mut vote_state, &slot_hashes, 0, 0, tower_sync,),
            Err(VoteError::LockoutConflict)
        );
    }

    #[test_case(VoteStateTargetVersion::V3 ; "VoteStateV3")]
    #[test_case(VoteStateTargetVersion::V4 ; "VoteStateV4")]
    fn test_check_and_filter_proposed_vote_state_slot_hash_mismatch(
        target_version: VoteStateTargetVersion,
    ) {
        let slot_hashes = build_slot_hashes(vec![2, 4, 6, 8]);
        let vote_state = build_vote_state(target_version, vec![2, 4, 6], &slot_hashes);

        // Test with a `TowerSync` where the hash is mismatched

        // Have to vote for a slot greater than the last vote in the vote state to avoid VoteTooOld
        // errors
        let vote_slot = vote_state.votes().back().unwrap().slot() + 2;
        let vote_slot_hash = Hash::new_unique();
        let mut tower_sync = TowerSync::from(vec![(2, 4), (4, 3), (6, 2), (vote_slot, 1)]);
        tower_sync.hash = vote_slot_hash;
        assert_eq!(
            check_and_filter_proposed_vote_state(
                &vote_state,
                &mut tower_sync.lockouts,
                &mut tower_sync.root,
                tower_sync.hash,
                &slot_hashes,
            ),
            Err(VoteError::SlotHashMismatch),
        );
    }

    #[test_case(0, true; "first slot")]
    #[test_case(DEFAULT_SLOTS_PER_EPOCH / 2, true; "halfway through epoch")]
    #[test_case((DEFAULT_SLOTS_PER_EPOCH / 2).saturating_add(1), false; "halfway through epoch plus one")]
    #[test_case(DEFAULT_SLOTS_PER_EPOCH.saturating_sub(1), false; "last slot in epoch")]
    #[test_case(DEFAULT_SLOTS_PER_EPOCH, true; "first slot in second epoch")]
    fn test_epoch_half_check(slot: Slot, expected_allowed: bool) {
        let epoch_schedule = EpochSchedule::without_warmup();
        assert_eq!(
            is_commission_update_allowed(slot, &epoch_schedule),
            expected_allowed
        );
    }

    #[test]
    fn test_warmup_epoch_half_check_with_warmup() {
        let epoch_schedule = EpochSchedule::default();
        let first_normal_slot = epoch_schedule.first_normal_slot;
        // first slot works
        assert!(is_commission_update_allowed(0, &epoch_schedule));
        // right before first normal slot works, since all warmup slots allow
        // commission updates
        assert!(is_commission_update_allowed(
            first_normal_slot - 1,
            &epoch_schedule
        ));
    }

    #[test_case(0, true; "first slot")]
    #[test_case(DEFAULT_SLOTS_PER_EPOCH / 2, true; "halfway through epoch")]
    #[test_case((DEFAULT_SLOTS_PER_EPOCH / 2).saturating_add(1), false; "halfway through epoch plus one")]
    #[test_case(DEFAULT_SLOTS_PER_EPOCH.saturating_sub(1), false; "last slot in epoch")]
    #[test_case(DEFAULT_SLOTS_PER_EPOCH, true; "first slot in second epoch")]
    fn test_epoch_half_check_with_warmup(slot: Slot, expected_allowed: bool) {
        let epoch_schedule = EpochSchedule::default();
        let first_normal_slot = epoch_schedule.first_normal_slot;
        assert_eq!(
            is_commission_update_allowed(first_normal_slot.saturating_add(slot), &epoch_schedule),
            expected_allowed
        );
    }

    #[test]
    fn test_create_v4_account_with_authorized() {
        let node_pubkey = Pubkey::new_unique();
        let authorized_voter = Pubkey::new_unique();
        let authorized_withdrawer = Pubkey::new_unique();
        let bls_pubkey_compressed = [42; 48];
        let inflation_rewards_commission_bps = 10000;
        let lamports = 100;
        let vote_account = create_v4_account_with_authorized(
            &node_pubkey,
            &authorized_voter,
            &authorized_withdrawer,
            Some(bls_pubkey_compressed),
            inflation_rewards_commission_bps,
            lamports,
        );
        assert_eq!(vote_account.lamports(), lamports);
        assert_eq!(vote_account.owner(), &id());
        assert_eq!(vote_account.data().len(), VoteStateV4::size_of());
        let vote_state_v4 = VoteStateV4::deserialize(vote_account.data(), &node_pubkey).unwrap();
        assert_eq!(vote_state_v4.node_pubkey, node_pubkey);
        assert_eq!(
            vote_state_v4.authorized_voters,
            AuthorizedVoters::new(0, authorized_voter)
        );
        assert_eq!(vote_state_v4.authorized_withdrawer, authorized_withdrawer);
        assert_eq!(
            vote_state_v4.bls_pubkey_compressed,
            Some(bls_pubkey_compressed)
        );
        assert_eq!(
            vote_state_v4.inflation_rewards_commission_bps,
            inflation_rewards_commission_bps
        );
    }

    #[test]
    fn test_update_validator_identity_syncs_block_revenue_collector() {
        let vote_state =
            vote_state_new_for_test(&solana_pubkey::new_rand(), VoteStateTargetVersion::V4);
        let node_pubkey = *vote_state.node_pubkey();
        let withdrawer_pubkey = *vote_state.authorized_withdrawer();

        let serialized = vote_state.serialize();
        let serialized_len = serialized.len();
        let rent = Rent::default();
        let lamports = rent.minimum_balance(serialized_len);
        let mut vote_account = AccountSharedData::new(lamports, serialized_len, &id());
        vote_account.set_data_from_slice(&serialized);

        let processor_account = AccountSharedData::new(0, 0, &solana_sdk_ids::native_loader::id());
        let mut transaction_context = TransactionContext::new(
            vec![(id(), processor_account), (node_pubkey, vote_account)],
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
        let instruction_context = transaction_context.get_next_instruction_context().unwrap();
        let mut borrowed_account = instruction_context
            .try_borrow_instruction_account(0)
            .unwrap();

        let new_node_pubkey = solana_pubkey::new_rand();
        let signers: HashSet<Pubkey> = vec![withdrawer_pubkey, new_node_pubkey]
            .into_iter()
            .collect();

        update_validator_identity(
            &mut borrowed_account,
            VoteStateTargetVersion::V4,
            &new_node_pubkey,
            &signers,
        )
        .unwrap();

        // Both `node_pubkey` and `block_revenue_collector` should be set to
        // the new node pubkey.
        let vote_state =
            VoteStateV4::deserialize(borrowed_account.get_data(), &new_node_pubkey).unwrap();
        assert_eq!(vote_state.node_pubkey, new_node_pubkey);
        assert_eq!(vote_state.block_revenue_collector, new_node_pubkey);

        // Run it again.
        let new_node_pubkey = solana_pubkey::new_rand();
        let signers: HashSet<Pubkey> = vec![withdrawer_pubkey, new_node_pubkey]
            .into_iter()
            .collect();

        update_validator_identity(
            &mut borrowed_account,
            VoteStateTargetVersion::V4,
            &new_node_pubkey,
            &signers,
        )
        .unwrap();

        let vote_state =
            VoteStateV4::deserialize(borrowed_account.get_data(), &new_node_pubkey).unwrap();
        assert_eq!(vote_state.node_pubkey, new_node_pubkey);
        assert_eq!(vote_state.block_revenue_collector, new_node_pubkey);
    }
}
