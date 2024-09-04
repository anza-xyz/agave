use {
    super::{EpochCreditsItem, PriorVotersItem, MAX_EPOCH_CREDITS_HISTORY, MAX_LOCKOUT_HISTORY},
    crate::{
        instruction::InstructionError,
        serialize_utils::cursor::*,
        vote::{
            authorized_voters::AuthorizedVoters,
            state::{BlockTimestamp, LandedVote, Lockout, VoteState, MAX_ITEMS},
        },
    },
    std::{
        collections::VecDeque,
        io::{Cursor, Read},
        mem,
        ptr::addr_of_mut,
        slice,
    },
};

pub(super) fn deserialize_vote_state_into(
    cursor: &mut Cursor<&[u8]>,
    vote_state: *mut VoteState,
    has_latency: bool,
) -> Result<(), InstructionError> {
    // General safety note: we must use add_or_mut! to access the `vote_state` fields as the value
    // is assumed to be _uninitialized_, so creating references to the state or any of its inner
    // fields is UB.

    read_pubkey_into(
        cursor,
        // Safety: if vote_state is non-null, node_pubkey is guaranteed to be valid too
        unsafe { addr_of_mut!((*vote_state).node_pubkey) },
    )?;
    read_pubkey_into(
        cursor,
        // Safety: if vote_state is non-null, authorized_withdrawer is guaranteed to be valid too
        unsafe { addr_of_mut!((*vote_state).authorized_withdrawer) },
    )?;
    let commission = read_u8(cursor)?;
    let votes = read_votes(cursor, has_latency)?;
    let root_slot = read_option_u64(cursor)?;
    let authorized_voters = read_authorized_voters(cursor)?;
    read_prior_voters_into(cursor, vote_state)?;
    let epoch_credits = read_epoch_credits(cursor)?;
    read_last_timestamp_into(cursor, vote_state)?;

    // Safety: if vote_state is non-null, all the fields are guaranteed to be
    // valid pointers.
    //
    // Heap allocated collections - votes, authorized_voters and epoch_credits -
    // are guaranteed not to leak after this point as the VoteState is fully
    // initialized and will be regularly dropped.
    unsafe {
        addr_of_mut!((*vote_state).commission).write(commission);
        addr_of_mut!((*vote_state).votes).write(votes);
        addr_of_mut!((*vote_state).root_slot).write(root_slot);
        addr_of_mut!((*vote_state).authorized_voters).write(authorized_voters);
        addr_of_mut!((*vote_state).epoch_credits).write(epoch_credits);
    }

    Ok(())
}

fn read_votes<T: AsRef<[u8]>>(
    cursor: &mut Cursor<T>,
    has_latency: bool,
) -> Result<VecDeque<LandedVote>, InstructionError> {
    let vote_count = read_u64(cursor)? as usize;
    let mut votes = VecDeque::with_capacity(vote_count.min(MAX_LOCKOUT_HISTORY));

    for _ in 0..vote_count {
        let latency = if has_latency { read_u8(cursor)? } else { 0 };

        let slot = read_u64(cursor)?;
        let confirmation_count = read_u32(cursor)?;
        let lockout = Lockout::new_with_confirmation_count(slot, confirmation_count);

        votes.push_back(LandedVote { latency, lockout });
    }

    Ok(votes)
}

fn read_authorized_voters<T: AsRef<[u8]>>(
    cursor: &mut Cursor<T>,
) -> Result<AuthorizedVoters, InstructionError> {
    let authorized_voter_count = read_u64(cursor)?;
    let mut authorized_voters = AuthorizedVoters::default();

    for _ in 0..authorized_voter_count {
        let epoch = read_u64(cursor)?;
        let authorized_voter = read_pubkey(cursor)?;
        authorized_voters.insert(epoch, authorized_voter);
    }

    Ok(authorized_voters)
}

fn read_prior_voters_into<T: AsRef<[u8]>>(
    cursor: &mut Cursor<T>,
    vote_state: *mut VoteState,
) -> Result<(), InstructionError> {
    // Safety: if vote_state is non-null, prior_voters is guaranteed to be valid too
    unsafe {
        let prior_voters = addr_of_mut!((*vote_state).prior_voters);
        let prior_voters_buf = addr_of_mut!((*prior_voters).buf) as *mut PriorVotersItem;

        // Vote state is always serialized using bincode which uses little
        // endian by default so when targeting little endian, we can optimize
        // deserialization by copying the serialized data directly into memory
        if cfg!(target_endian = "little") {
            let total_bytes = MAX_ITEMS.saturating_mul(mem::size_of::<PriorVotersItem>());
            let buffer_ptr = prior_voters_buf as *mut u8;

            // Read the bytes directly into the buffer's uninitialized memory
            cursor
                .read_exact(slice::from_raw_parts_mut(buffer_ptr, total_bytes))
                .map_err(|_| InstructionError::InvalidAccountData)?;
        } else {
            for i in 0..MAX_ITEMS {
                let prior_voter = read_pubkey(cursor)?;
                let from_epoch = read_u64(cursor)?;
                let until_epoch = read_u64(cursor)?;

                prior_voters_buf.add(i).write(PriorVotersItem {
                    voter: prior_voter,
                    start_epoch_inclusive: from_epoch,
                    end_epoch_exclusive: until_epoch,
                });
            }
        }

        (*vote_state).prior_voters.idx = read_u64(cursor)? as usize;
        (*vote_state).prior_voters.is_empty = read_bool(cursor)?;
    }
    Ok(())
}

fn read_epoch_credits<T: AsRef<[u8]>>(
    cursor: &mut Cursor<T>,
) -> Result<Vec<EpochCreditsItem>, InstructionError> {
    let epoch_credit_count = read_u64(cursor)? as usize;
    let mut epoch_credits = Vec::with_capacity(epoch_credit_count.min(MAX_EPOCH_CREDITS_HISTORY));

    // Vote state is always serialized using bincode which uses little
    // endian by default so when targeting little endian, we can optimize
    // deserialization by copying the serialized data directly into memory.
    // But only do this optimization if the epoch_credits_count isn't bigger
    // than the max history so that we don't overshoot the allocated vec buffer
    // bounds.
    if cfg!(target_endian = "little") && epoch_credit_count <= MAX_EPOCH_CREDITS_HISTORY {
        let total_bytes = epoch_credit_count.saturating_mul(mem::size_of::<EpochCreditsItem>());

        // Safety: This is safe because we have pre-allocated enough space in the Vec,
        // and we will manually initialize each element by reading from the cursor.
        unsafe {
            // Get a pointer to the start of the vector's buffer
            let buffer_ptr = epoch_credits.as_mut_ptr() as *mut u8;

            // Read the bytes directly into the vector's uninitialized memory
            cursor
                .read_exact(slice::from_raw_parts_mut(buffer_ptr, total_bytes))
                .map_err(|_| InstructionError::InvalidAccountData)?;

            // Set the length of the vector to reflect the initialized elements
            epoch_credits.set_len(epoch_credit_count);
        }
    } else {
        for _ in 0..epoch_credit_count {
            let epoch = read_u64(cursor)?;
            let credits = read_u64(cursor)?;
            let prev_credits = read_u64(cursor)?;
            epoch_credits.push(EpochCreditsItem {
                epoch,
                credits,
                prev_credits,
            });
        }
    }

    Ok(epoch_credits)
}

fn read_last_timestamp_into<T: AsRef<[u8]>>(
    cursor: &mut Cursor<T>,
    vote_state: *mut VoteState,
) -> Result<(), InstructionError> {
    let slot = read_u64(cursor)?;
    let timestamp = read_i64(cursor)?;

    let last_timestamp = BlockTimestamp { slot, timestamp };

    // Safety: if vote_state is non-null, last_timestamp is guaranteed to be valid too
    unsafe {
        addr_of_mut!((*vote_state).last_timestamp).write(last_timestamp);
    }

    Ok(())
}
