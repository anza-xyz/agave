use {
    crate::banking_stage::LikeClusterInfo,
    itertools::Itertools,
    solana_gossip::{
        cluster_info::ClusterInfo,
        contact_info::{ContactInfoQuery, Protocol},
    },
    solana_poh::poh_recorder::PohRecorder,
    solana_sdk::clock::{
        FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET, NUM_CONSECUTIVE_LEADER_SLOTS,
    },
    std::{net::SocketAddr, sync::RwLock},
};

/// Returns a list of tpu vote sockets for the leaders of the next N fanout
/// slots. Leaders and sockets are deduped.
pub(crate) fn upcoming_leader_tpu_vote_sockets(
    cluster_info: &ClusterInfo,
    poh_recorder: &RwLock<PohRecorder>,
    fanout_slots: u64,
    protocol: Protocol,
) -> Vec<SocketAddr> {
    let upcoming_leaders = {
        let poh_recorder = poh_recorder.read().unwrap();
        (0..fanout_slots)
            .filter_map(|n_slots| poh_recorder.leader_after_n_slots(n_slots))
            .collect_vec()
    };

    upcoming_leaders
        .into_iter()
        .dedup()
        .filter_map(|leader_pubkey| {
            cluster_info.lookup_contact_info(&leader_pubkey, |node| node.tpu_vote(protocol))?
        })
        // dedup again since leaders could potentially share the same tpu vote socket
        .dedup()
        .collect()
}

pub(crate) fn next_leaders(
    cluster_info: &impl LikeClusterInfo,
    poh_recorder: &RwLock<PohRecorder>,
    max_count: u64,
    port_selector: impl ContactInfoQuery<Option<SocketAddr>>,
) -> Vec<SocketAddr> {
    let recorder = poh_recorder.read().unwrap();
    let leader_pubkeys: Vec<_> = (0..max_count)
        .filter_map(|i| {
            recorder.leader_after_n_slots(
                FORWARD_TRANSACTIONS_TO_LEADER_AT_SLOT_OFFSET + i * NUM_CONSECUTIVE_LEADER_SLOTS,
            )
        })
        .collect();
    drop(recorder);

    leader_pubkeys
        .iter()
        .filter_map(|leader_pubkey| {
            cluster_info.lookup_contact_info(leader_pubkey, &port_selector)?
        })
        .collect()
}
