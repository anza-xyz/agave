use {
    crate::alpenglow::{
        error::VoteError,
        state::{PodSlot, VoteState},
    },
    bytemuck::{Pod, Zeroable},
    solana_program::{
        account_info::AccountInfo,
        clock::{Clock, Slot},
        hash::Hash,
        program_error::ProgramError,
        pubkey::Pubkey,
        sysvar::slot_hashes::PodSlotHashes,
    },
};

pub(crate) const CURRENT_NOTARIZE_VOTE_VERSION: u8 = 1;

/// Number of slots of grace period for which maximum vote credits are awarded - votes landing
/// within this number of slots of the slot that is being voted on are awarded full credits.
pub const VOTE_CREDITS_GRACE_SLOTS: u64 = 2;

/// Maximum number of credits to award for a vote; this number of credits is awarded to votes on
/// slots that land within the grace period. After that grace period, vote credits are reduced.
pub const VOTE_CREDITS_MAXIMUM_PER_SLOT: u64 = 16;

/// A notarization vote, the data expected by
/// `VoteInstruction::Notarize` and `VoteInstruction::NotarizeFallback`
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct NotarizationVoteInstructionData {
    /// The version of this vote message
    pub version: u8,

    /// The slot being notarized
    pub slot: PodSlot,

    /// The block id of this slot
    pub block_id: Hash,

    /// The slot of the last replayed block
    /// Only relevant after APE
    pub _replayed_slot: PodSlot,

    /// The bank_hash of the last replayed block
    /// Prior to APE this is the bank hash of `slot`
    pub replayed_bank_hash: Hash,
}

// SAFETY: for our purposes we treat a zero timestamp as the validator not
// supplying a timestamp, so timestamp is safe to be zeroable
unsafe impl Zeroable for NotarizationVoteInstructionData {}
unsafe impl Pod for NotarizationVoteInstructionData {}

/// Credits are awarded as a piece-wise linear function; up to a certain amount of block latency,
/// the vote program awards the maximum number of credits. Then, the number of awarded credits goes
/// down at a rate of 1 credit per block. The minimum number of awarded credits is 1.
fn latency_to_credits(latency: u64) -> u64 {
    let (kink_lo, kink_hi) = (
        VOTE_CREDITS_GRACE_SLOTS,
        VOTE_CREDITS_MAXIMUM_PER_SLOT + VOTE_CREDITS_GRACE_SLOTS - 1,
    );

    if latency <= kink_lo {
        VOTE_CREDITS_MAXIMUM_PER_SLOT
    } else if kink_lo < latency && latency <= kink_hi {
        // NOTE: checked_sub isn't necessary, since latency < kink_hi. Eventually, just use
        // unchecked_sub.
        kink_hi.saturating_add(1).saturating_sub(latency)
    } else {
        1
    }
}

fn set_credits(
    vote_state: &mut VoteState,
    epoch: u64,
    earned_credits: u64,
) -> Result<(), ProgramError> {
    let epoch_credits = &mut vote_state.epoch_credits;

    if epoch == epoch_credits.epoch() {
        epoch_credits.set_credits(epoch_credits.credits().saturating_add(earned_credits));
        Ok(())
    } else {
        let prev_credits = epoch_credits
            .prev_credits()
            .saturating_add(epoch_credits.credits());

        epoch_credits.set_epoch(epoch);
        epoch_credits.set_prev_credits(prev_credits);
        epoch_credits.set_credits(earned_credits.saturating_add(prev_credits));
        Ok(())
    }
}

/// Award credits based on latency of `vote_slot`
fn award_credits(
    vote_state: &mut VoteState,
    vote_slot: u64,
    clock: &Clock,
) -> Result<(), ProgramError> {
    // NOTE: clock.slot >= vote_slot; otherwise, replay_bank_hash_checks would have returned an
    // error (vote.slot would not be in our slot hashes). Eventually, just use unchecked_sub.
    let earned_credits = latency_to_credits(clock.slot.saturating_sub(vote_slot));
    // Although this vote might be for a previous epoch, the checks in the caller
    // ensure that this is a new vote. We mirror the logic in the previous vote
    // program and award credits based on `clock.epoch`
    set_credits(vote_state, clock.epoch, earned_credits)
}

/// Award credits for notarization vote
fn award_notarization_credits(
    vote_state: &mut VoteState,
    vote_slot: Slot,
    bank_hash: Hash,
    clock: &Clock,
    slot_hashes: &PodSlotHashes,
) -> Result<(), ProgramError> {
    // We must have already executed `notarized_slot` and stored the associated bank hash
    // (error out otherwise). Ensure that our bank hash matches what we observe.
    if bank_hash
        != slot_hashes
            .get(&vote_slot)
            .map_err(|_| ProgramError::from(VoteError::MissingSlotHashesSysvar))?
            .ok_or::<ProgramError>(VoteError::SlotHashesMissingKey.into())?
    {
        Err(VoteError::ReplayBankHashMismatch.into())
    } else {
        award_credits(vote_state, vote_slot, clock)
    }
}

/// Award credits for finalization votes
fn award_finalization_credits(
    vote_state: &mut VoteState,
    vote_slot: Slot,
    clock: &Clock,
) -> Result<(), ProgramError> {
    award_credits(vote_state, vote_slot, clock)
}

/// Award credits for skip votes
fn award_skip_credits(
    vote_state: &mut VoteState,
    skip_slot: Slot,
    clock: &Clock,
    slot_hashes: &PodSlotHashes,
) -> Result<(), ProgramError> {
    if skip_slot >= clock.slot {
        return Err(VoteError::SkipSlotExceedsCurrentSlot.into());
    }

    let hash = slot_hashes
        .get(&skip_slot)
        .map_err(|_| VoteError::MissingSlotHashesSysvar)?;

    // Observing a valid slot hash for the slot `skip_slot` indicates that `skip_slot` was
    // not skipped on this fork. Only award credits to skip votes associated with slots that
    // were skipped.
    if hash.is_some() {
        Err(VoteError::SkipSlotPresent.into())
    } else {
        award_credits(vote_state, skip_slot, clock)
    }
}

pub(crate) fn process_notarization_vote(
    vote_account: &AccountInfo,
    vote_authority: &Pubkey,
    clock: &Clock,
    slot_hashes: &PodSlotHashes,
    vote: &NotarizationVoteInstructionData,
) -> Result<(), ProgramError> {
    let mut vote_state = vote_account.data.borrow_mut();
    let vote_state = bytemuck::from_bytes_mut::<VoteState>(&mut vote_state);

    let vote_slot = vote.slot.into();

    if vote.version != CURRENT_NOTARIZE_VOTE_VERSION {
        return Err(VoteError::VersionMismatch.into());
    }

    if vote_state.authorized_voter.voter != *vote_authority {
        return Err(ProgramError::MissingRequiredSignature);
    }

    award_notarization_credits(
        vote_state,
        vote_slot,
        vote.replayed_bank_hash,
        clock,
        slot_hashes,
    )
}

pub(crate) fn process_finalization_vote(
    vote_account: &AccountInfo,
    vote_authority: &Pubkey,
    clock: &Clock,
    slot: &PodSlot,
) -> Result<(), ProgramError> {
    let mut vote_state = vote_account.data.borrow_mut();
    let vote_state = bytemuck::from_bytes_mut::<VoteState>(&mut vote_state);

    if vote_state.authorized_voter.voter != *vote_authority {
        return Err(ProgramError::MissingRequiredSignature);
    }

    let vote_slot = Slot::from(*slot);

    award_finalization_credits(vote_state, vote_slot, clock)
}

pub(crate) fn process_skip_vote(
    vote_account: &AccountInfo,
    vote_authority: &Pubkey,
    clock: &Clock,
    slot_hashes: &PodSlotHashes,
    slot: &PodSlot,
) -> Result<(), ProgramError> {
    let mut vote_state = vote_account.data.borrow_mut();
    let vote_state = bytemuck::from_bytes_mut::<VoteState>(&mut vote_state);

    if vote_state.authorized_voter.voter != *vote_authority {
        return Err(ProgramError::MissingRequiredSignature);
    }

    let slot = Slot::from(*slot);

    award_skip_credits(vote_state, slot, clock, slot_hashes)
}

#[cfg(test)]
mod tests {
    use {
        super::award_skip_credits,
        crate::alpenglow::{
            accounting::EpochCredit,
            instruction::InitializeAccountInstructionData,
            state::VoteState,
            vote_processor::{
                award_credits, latency_to_credits, set_credits, VOTE_CREDITS_GRACE_SLOTS,
                VOTE_CREDITS_MAXIMUM_PER_SLOT,
            },
        },
        serial_test::serial,
        solana_bls_signatures::keypair::Keypair as BlsKeypair,
        solana_clock::Clock,
        solana_hash::Hash,
        solana_program::{
            entrypoint::SUCCESS,
            epoch_schedule::EpochSchedule,
            program_stubs::{set_syscall_stubs, SyscallStubs},
            slot_hashes::SlotHashes,
            sysvar::{slot_hashes::PodSlotHashes, Sysvar},
        },
        solana_pubkey::Pubkey,
        spl_pod::primitives::PodU64,
        test_case::test_case,
    };

    #[test]
    fn test_latency_to_credits_max_credits() {
        for latency in 0..=VOTE_CREDITS_GRACE_SLOTS {
            assert_eq!(VOTE_CREDITS_MAXIMUM_PER_SLOT, latency_to_credits(latency));
        }
    }

    #[test]
    fn test_latency_to_credits_ramp_down() {
        for latency in 3..=VOTE_CREDITS_MAXIMUM_PER_SLOT + 1 {
            assert_eq!(
                VOTE_CREDITS_MAXIMUM_PER_SLOT - (latency - 2),
                latency_to_credits(latency)
            );
        }
    }

    #[test]
    fn test_latency_to_credits_min() {
        for latency in [18, 20, 100, 1_000, 10_000, 100_000] {
            assert_eq!(1, latency_to_credits(latency));
        }
    }

    fn setup_vote_state(clock: &Clock) -> VoteState {
        let bls_keypair = BlsKeypair::new();
        let bls_pubkey = bls_keypair.public.into();
        let iaid = InitializeAccountInstructionData {
            node_pubkey: Pubkey::new_unique(),
            authorized_voter: Pubkey::new_unique(),
            authorized_withdrawer: Pubkey::new_unique(),
            commission: 0_u8,
            bls_pubkey,
        };

        VoteState::new(&iaid, clock)
    }

    fn epoch_to_starting_slot(epoch: u64) -> u64 {
        epoch
            .saturating_sub(14)
            .saturating_mul(432_000)
            .saturating_add(524_256)
    }

    // NOTE tests that use this mock MUST carry the #[serial] attribute
    struct MockGetSysvarSyscall {
        data: Vec<u8>,
    }

    impl SyscallStubs for MockGetSysvarSyscall {
        #[allow(clippy::arithmetic_side_effects)]
        fn sol_get_sysvar(
            &self,
            _sysvar_id_addr: *const u8,
            var_addr: *mut u8,
            offset: u64,
            length: u64,
        ) -> u64 {
            let slice = unsafe { std::slice::from_raw_parts_mut(var_addr, length as usize) };
            slice.copy_from_slice(&self.data[offset as usize..(offset + length) as usize]);
            SUCCESS
        }
    }

    pub fn mock_get_sysvar_syscall(data: &[u8]) {
        set_syscall_stubs(Box::new(MockGetSysvarSyscall {
            data: data.to_vec(),
        }));
    }

    fn mock_slot_hashes(slot_hashes: &SlotHashes) {
        // The data is always `SlotHashes::size_of()`.
        let mut data = vec![0; SlotHashes::size_of()];
        bincode::serialize_into(&mut data[..], slot_hashes).unwrap();
        mock_get_sysvar_syscall(&data);
    }

    fn mock_slot_hash_entries(slot_hash_entries: Vec<(u64, Hash)>) -> PodSlotHashes {
        mock_slot_hashes(&SlotHashes::new(&slot_hash_entries));
        PodSlotHashes::fetch().unwrap()
    }

    #[test_case(1; "one")]
    #[test_case(2; "two")]
    #[test_case(5; "five")]
    #[test_case(20; "twenty")]
    fn test_award_credits_vote_epoch_equals_epoch_init(latency: u64) {
        let clock = Clock {
            slot: epoch_to_starting_slot(256).saturating_add(latency),
            epoch: 256,
            ..Clock::default()
        };
        let mut vote_state = setup_vote_state(&clock);

        let vote_slot = epoch_to_starting_slot(256);

        assert_eq!(0, vote_state.epoch_credits.epoch());
        assert_eq!(0, vote_state.epoch_credits.prev_credits());
        assert_eq!(0, vote_state.epoch_credits.credits());

        let expected_earned_credits = latency_to_credits(latency);

        assert!(set_credits(&mut vote_state, clock.epoch, expected_earned_credits,).is_ok());
        assert_eq!(latency, clock.slot.saturating_sub(vote_slot));

        assert_eq!(256, vote_state.epoch_credits.epoch());
        assert_eq!(0, vote_state.epoch_credits.prev_credits());
        assert_eq!(expected_earned_credits, vote_state.epoch_credits.credits());
    }

    #[test_case(1; "one")]
    #[test_case(2; "two")]
    #[test_case(5; "five")]
    #[test_case(20; "twenty")]
    fn test_award_credits_vote_epoch_equals_clock_epoch_intermediate(latency: u64) {
        let clock = Clock {
            slot: epoch_to_starting_slot(256).saturating_add(latency),
            epoch: 256,
            ..Clock::default()
        };
        let epoch_schedule = EpochSchedule::default();
        let mut vote_state = setup_vote_state(&clock);
        let vote_slot = epoch_to_starting_slot(256);

        assert_eq!(256, epoch_schedule.get_epoch(clock.slot));
        assert_eq!(256, epoch_schedule.get_epoch(vote_slot));

        vote_state.epoch_credits = EpochCredit {
            epoch: PodU64::from(256),
            credits: PodU64::from(123),
            prev_credits: PodU64::from(234),
        };

        let expected_earned_credits = latency_to_credits(clock.slot.saturating_sub(vote_slot));

        assert!(set_credits(&mut vote_state, clock.epoch, expected_earned_credits,).is_ok());

        assert_eq!(latency, clock.slot.saturating_sub(vote_slot));
        assert_eq!(256, vote_state.epoch_credits.epoch());
        assert_eq!(234, vote_state.epoch_credits.prev_credits());
        assert_eq!(
            expected_earned_credits.saturating_add(123),
            vote_state.epoch_credits.credits()
        );
    }

    #[test_case(1; "one")]
    #[test_case(2; "two")]
    #[test_case(5; "five")]
    #[test_case(20; "twenty")]
    fn test_award_credits_vote_epoch_greater_than_clock_epoch(latency: u64) {
        let clock = Clock {
            slot: epoch_to_starting_slot(256),
            epoch: 256,
            ..Clock::default()
        };
        let epoch_schedule = EpochSchedule::default();
        let mut vote_state = setup_vote_state(&clock);
        let vote_slot = epoch_to_starting_slot(256).saturating_sub(latency);

        assert_eq!(255, epoch_schedule.get_epoch(vote_slot));
        assert_eq!(256, epoch_schedule.get_epoch(clock.slot));

        vote_state.epoch_credits = EpochCredit {
            epoch: PodU64::from(12),
            credits: PodU64::from(234),
            prev_credits: PodU64::from(123),
        };

        let expected_earned_credits = latency_to_credits(clock.slot.saturating_sub(vote_slot));

        assert!(set_credits(&mut vote_state, clock.epoch, expected_earned_credits,).is_ok());
        assert_eq!(latency, clock.slot.saturating_sub(vote_slot));

        assert_eq!(256, vote_state.epoch_credits.epoch());
        assert_eq!(123 + 234, vote_state.epoch_credits.prev_credits());
        assert_eq!(
            expected_earned_credits.saturating_add(123 + 234),
            vote_state.epoch_credits.credits()
        );
    }

    #[test]
    #[serial]
    fn test_process_skip_credits_vote_slot_cannot_be_after_clock_slot() {
        let clock = Clock {
            slot: epoch_to_starting_slot(256),
            epoch: 256,
            ..Clock::default()
        };

        let mut vote_state = setup_vote_state(&clock);

        assert_eq!(0, vote_state.epoch_credits().credits());
        assert_eq!(0, vote_state.epoch_credits().prev_credits());

        assert!(award_skip_credits(
            &mut vote_state,
            clock.slot - 5,
            &clock,
            &mock_slot_hash_entries(vec![]),
        )
        .is_ok());

        assert_eq!(13, vote_state.epoch_credits().credits());
        assert_eq!(0, vote_state.epoch_credits().prev_credits());
    }

    #[test_case(1; "one")]
    #[test_case(2; "two")]
    #[test_case(3; "three")]
    #[test_case(4; "four")]
    #[test_case(5; "five")]
    #[test_case(10; "ten")]
    #[test_case(12; "twelve")]
    #[test_case(14; "fourteen")]
    #[test_case(16; "sixteen")]
    #[test_case(18; "eighteen")]
    #[test_case(20; "twenty")]
    #[serial]
    fn test_process_notarization_finalization_credits_simple(latency: u64) {
        let clock = Clock {
            slot: epoch_to_starting_slot(256),
            epoch: 256,
            ..Clock::default()
        };

        let mut vote_state = setup_vote_state(&clock);
        assert_eq!(0, vote_state.epoch_credits().credits());
        assert_eq!(0, vote_state.epoch_credits().prev_credits());

        let vote_slot = clock.slot.checked_sub(latency).unwrap();

        assert!(award_credits(&mut vote_state, vote_slot, &clock).is_ok());

        let expected_awarded_credits = latency_to_credits(latency);

        assert_eq!(
            expected_awarded_credits,
            vote_state.epoch_credits().credits()
        );
        assert_eq!(0, vote_state.epoch_credits().prev_credits());
    }
}
