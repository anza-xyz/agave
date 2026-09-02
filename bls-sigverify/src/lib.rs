#![cfg(feature = "agave-unstable-api")]
#![allow(dead_code)]

use {
    crate::bls_vote_sigverify::UnverifiedVotePayload,
    agave_votor_messages::wire::VotePayloadToSign,
    solana_runtime::epoch_stakes::BLSPubkeyToRankMap,
    std::{collections::HashMap, sync::Arc},
};

pub mod bls_cert_sigverify;
pub mod bls_sigverifier;
pub mod bls_vote_sigverify;
mod certs_verifier;
mod errors;
pub mod generated_cert_types;
mod msg_receiver;
pub mod rewards;
pub mod stats;
mod utils;
mod vote_pool;
mod votes_processor;
mod votes_verifier;

type UnverifiedVotesMessage =
    HashMap<VotePayloadToSign, (Vec<UnverifiedVotePayload>, Arc<BLSPubkeyToRankMap>)>;
