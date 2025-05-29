use {
    crate::{crds_data::CrdsData, crds_value::CrdsValue},
    solana_pubkey::Pubkey,
    std::collections::HashMap,
};

pub(crate) enum GossipFilterDirection {
    Ingress,
    EgressPush,
    EgressPullResponse,
}

/// Minimum number of staked nodes for enforcing stakes in gossip.
const MIN_NUM_STAKED_NODES: usize = 500;

/// Minimum stake that a node should have so that all its CRDS values are
/// propagated through gossip (below this only subset of CRDS is propagated).
const MIN_STAKE_FOR_GOSSIP: u64 = solana_native_token::LAMPORTS_PER_SOL;

/// Minimum stake that a node should have so that we skip pinging it when
/// joining the cluster
pub(crate) const MIN_STAKE_TO_SKIP_PING: u64 = 100 * MIN_STAKE_FOR_GOSSIP;

/// Returns false if the CRDS value should be discarded.
/// `direction` controls whether we are looking at
/// incoming packet (via Push or PullResponse) or
/// we are about to make a packet
#[inline]
#[must_use]
pub(crate) fn should_retain_crds_value(
    value: &CrdsValue,
    stakes: &HashMap<Pubkey, u64>,
    direction: GossipFilterDirection,
) -> bool {
    let retain_if_staked = || {
        stakes.len() < MIN_NUM_STAKED_NODES || {
            let stake = stakes.get(&value.pubkey()).copied();
            stake.unwrap_or_default() >= MIN_STAKE_FOR_GOSSIP
        }
    };

    use GossipFilterDirection::*;
    match value.data() {
        CrdsData::ContactInfo(_) => true,
        // Unstaked nodes can still serve snapshots.
        CrdsData::SnapshotHashes(_) => true,
        // Consensus related messages only allowed for staked nodes
        CrdsData::DuplicateShred(_, _)
        | CrdsData::LowestSlot(_, _)
        | CrdsData::RestartHeaviestFork(_)
        | CrdsData::RestartLastVotedForkSlots(_) => retain_if_staked(),
        // Legacy unstaked nodes can still send EpochSlots
        CrdsData::EpochSlots(_, _) | CrdsData::Vote(_, _) => match direction {
            // always store if we have received them
            // to avoid getting them again in PullResponses
            Ingress => true,
            // only forward if the origin is staked
            EgressPush | EgressPullResponse => retain_if_staked(),
        },
        // Deprecated messages we still see in the mainnet.
        // We want to store them to avoid getting them again
        // in PullResponses.
CrdsData::NodeInstance(_) | CrdsData::LegacyContactInfo(_) | CrdsData::Version(_) => false,
        // Fully deprecated messages
        CrdsData::LegacySnapshotHashes(_) => false,
        CrdsData::LegacyVersion(_) => false,
        CrdsData::AccountsHashes(_) => false,
    }
}
