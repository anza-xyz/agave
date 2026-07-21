use {
    crossbeam_channel::{Receiver, Sender},
    solana_clock::{BankId, Slot},
    solana_hash::Hash,
    solana_vote::vote_parser::ParsedVote,
};

/// Message sent by banking and replay to the solCiProcVotes thread to update its state machine.
///
/// Banking sends VerifiedExecuted(vote) as it builds a block, and solCiProcVotes processes those
/// votes immediately.
///
/// Replay runs sigverify and execution in parallel, and sends Verified and Executed respectively as
/// those stages complete. solCiProcVotes waits until it has received both messages for the same
/// vote transaction before processing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayVoteMessage {
    /// The vote was sigverified and executed
    VerifiedExecuted(ParsedVote),
    /// The vote was executed, but sigverify might not have finished yet
    Executed {
        replay_bank_id: BankId,
        replay_slot: Slot,
        message_hash: Hash,
        parsed_vote: ParsedVote,
    },
    /// The votes were sigverified.
    Verified {
        replay_bank_id: BankId,
        replay_slot: Slot,
        message_hashes: Vec<Hash>,
    },
    /// The bank is invalid no more votes should be processed.
    ///
    /// This is informative and used to release memory early. If not sent (like
    /// in some replay error paths), memory will be released as slots are rooted.
    InvalidBank {
        replay_bank_id: BankId,
        replay_slot: Slot,
    },
    /// The bank is complete.
    ///
    /// Like InvalidBank this is informative and used to release memory early.
    BankComplete {
        replay_bank_id: BankId,
        replay_slot: Slot,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayVoteSendType {
    VerifiedExecuted,
    Executed {
        replay_bank_id: BankId,
        replay_slot: Slot,
    },
}

pub type ReplayVoteSender = Sender<ReplayVoteMessage>;
pub type ReplayVoteReceiver = Receiver<ReplayVoteMessage>;
