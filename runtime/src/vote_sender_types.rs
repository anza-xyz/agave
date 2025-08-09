use {
    crossbeam_channel::{Receiver, Sender},
    solana_vote::vote_parser::ParsedVote,
    solana_votor_messages::bls_message::BLSMessage,
};

pub type ReplayVoteSender = Sender<ParsedVote>;
pub type ReplayVoteReceiver = Receiver<ParsedVote>;

pub type BLSVerifiedMessageSender = Sender<BLSMessage>;
pub type BLSVerifiedMessageReceiver = Receiver<BLSMessage>;
