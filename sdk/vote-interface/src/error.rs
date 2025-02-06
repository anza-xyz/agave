//! Vote program errors

use {
    core::fmt,
    num_derive::{FromPrimitive, ToPrimitive},
    solana_decode_error::DecodeError,
};

/// Reasons the vote might have had an error
#[derive(Debug, Clone, PartialEq, Eq, FromPrimitive, ToPrimitive)]
pub enum VoteError {
    VoteTooOld,
    SlotsMismatch,
    SlotHashMismatch,
    EmptySlots,
    TimestampTooOld,
    TooSoonToReauthorize,
    // TODO: figure out how to migrate these new errors
    LockoutConflict,
    NewVoteStateLockoutMismatch,
    SlotsNotOrdered,
    ConfirmationsNotOrdered,
    ZeroConfirmations,
    ConfirmationTooLarge,
    RootRollBack,
    ConfirmationRollBack,
    SlotSmallerThanRoot,
    TooManyVotes,
    VotesTooOldAllFiltered,
    RootOnDifferentFork,
    ActiveVoteAccountClose,
    CommissionUpdateTooLate,
    AssertionFailed,
}

impl std::error::Error for VoteError {}

impl fmt::Display for VoteError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Self::VoteTooOld => "vote already recorded or not in slot hashes history",
            Self::SlotsMismatch => "vote slots do not match bank history",
            Self::SlotHashMismatch => "vote hash does not match bank hash",
            Self::EmptySlots => "vote has no slots, invalid",
            Self::TimestampTooOld => "vote timestamp not recent",
            Self::TooSoonToReauthorize => "authorized voter has already been changed this epoch",
            Self::LockoutConflict => {
                "Old state had vote which should not have been popped off by vote in new state"
            }
            Self::NewVoteStateLockoutMismatch => {
                "Proposed state had earlier slot which should have been popped off by later vote"
            }
            Self::SlotsNotOrdered => "Vote slots are not ordered",
            Self::ConfirmationsNotOrdered => "Confirmations are not ordered",
            Self::ZeroConfirmations => "Zero confirmations",
            Self::ConfirmationTooLarge => "Confirmation exceeds limit",
            Self::RootRollBack => "Root rolled back",
            Self::ConfirmationRollBack => {
                "Confirmations for same vote were smaller in new proposed state"
            }
            Self::SlotSmallerThanRoot => "New state contained a vote slot smaller than the root",
            Self::TooManyVotes => "New state contained too many votes",
            Self::VotesTooOldAllFiltered => {
                "every slot in the vote was older than the SlotHashes history"
            }
            Self::RootOnDifferentFork => "Proposed root is not in slot hashes",
            Self::ActiveVoteAccountClose => {
                "Cannot close vote account unless it stopped voting at least one full epoch ago"
            }
            Self::CommissionUpdateTooLate => "Cannot update commission at this point in the epoch",
            Self::AssertionFailed => "Assertion failed",
        })
    }
}

impl<E> DecodeError<E> for VoteError {
    fn type_of() -> &'static str {
        "VoteError"
    }
}

#[cfg(test)]
mod tests {
    use {super::*, solana_instruction::error::InstructionError};

    #[test]
    fn test_custom_error_decode() {
        use num_traits::FromPrimitive;
        fn pretty_err<T>(err: InstructionError) -> String
        where
            T: 'static + std::error::Error + DecodeError<T> + FromPrimitive,
        {
            if let InstructionError::Custom(code) = err {
                let specific_error: T = T::decode_custom_error_to_enum(code).unwrap();
                format!(
                    "{:?}: {}::{:?} - {}",
                    err,
                    T::type_of(),
                    specific_error,
                    specific_error,
                )
            } else {
                "".to_string()
            }
        }
        assert_eq!(
            "Custom(0): VoteError::VoteTooOld - vote already recorded or not in slot hashes history",
            pretty_err::<VoteError>(VoteError::VoteTooOld.into())
        )
    }
}
