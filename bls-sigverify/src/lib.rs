#![cfg(feature = "agave-unstable-api")]
#[cfg(feature = "dev-context-only-utils")]
use qualifier_attr::qualifiers;
use {
    crate::votes_verifier::UnverifiedVote,
    agave_votor_messages::{sig_verified_messages::VoteAggregate, wire::VotePayloadToSign},
    solana_pubkey::Pubkey,
    solana_runtime::epoch_stakes::BLSPubkeyToRankMap,
    std::{collections::HashMap, sync::Arc},
};

pub mod bls_sigverifier;
mod certs_verifier;
mod errors;
pub mod generated_cert_types;
mod msg_receiver;
pub mod rewards;
pub mod stats;
mod utils;
mod vote_pool;
mod votes_processor;
pub mod votes_verifier;

type MessageToVotesVerifier =
    HashMap<VotePayloadToSign, (Vec<UnverifiedVote>, Arc<BLSPubkeyToRankMap>)>;

#[cfg_attr(feature = "dev-context-only-utils", qualifiers(pub))]
pub(crate) struct VerifiedVote {
    pub(crate) vote_aggregate: VoteAggregate,
    pub(crate) sender_vote_account_pubkeys: Vec<Pubkey>,
}

#[cfg(test)]
mod test_utils;
