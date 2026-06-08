#![cfg(feature = "agave-unstable-api")]
#![cfg_attr(feature = "frozen-abi", feature(min_specialization))]

#[macro_use]
extern crate log;

pub mod commitment;
pub mod common;
pub mod consensus_metrics;
pub mod consensus_pool;
mod consensus_pool_service;
pub mod event;
mod event_handler;
pub mod generated_cert_types;
pub mod root_utils;
mod staked_validators_cache;
mod timer_manager;
pub mod vote_history;
pub mod vote_history_storage;
pub mod voting_service;
pub mod voting_utils;
pub mod votor;

#[cfg_attr(feature = "frozen-abi", macro_use)]
#[cfg(feature = "frozen-abi")]
extern crate solana_frozen_abi_macro;

#[cfg(test)]
mod tests {
    use {
        solana_gossip::{cluster_info::ClusterInfo, contact_info::ContactInfo},
        solana_keypair::Keypair,
        solana_net_utils::SocketAddrSpace,
        solana_signer::Signer,
        std::sync::Arc,
    };

    pub(crate) fn get_cluster_info(keypair: Keypair) -> Arc<ClusterInfo> {
        Arc::new(ClusterInfo::new(
            ContactInfo::new_localhost(&keypair.pubkey(), 0),
            Arc::new(keypair),
            SocketAddrSpace::Unspecified,
        ))
    }
}
