use {
    crate::{crds_data::CrdsData, crds_value::CrdsValue},
    solana_pubkey::Pubkey,
    std::collections::HashMap,
};

/// Minimum number of staked nodes for enforcing stakes in gossip.
const MIN_NUM_STAKED_NODES: usize = 500;
/// Minimum stake that a node should have so that its CRDS values are
/// propagated through gossip (few types are exempted).
pub(crate) const MIN_STAKE_FOR_GOSSIP: u64 = solana_native_token::LAMPORTS_PER_SOL;

pub(crate) enum GossipFilterDirection {
    Ingress,
    Egress,
}

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
    match value.data() {
        CrdsData::ContactInfo(_) => true,
        CrdsData::LegacyContactInfo(_) => true,
        // Unstaked nodes can still serve snapshots.
        CrdsData::SnapshotHashes(_) => true,
        // Messages only allowed for staked nodes
        CrdsData::DuplicateShred(_, _)
        | CrdsData::LowestSlot(_, _)
        | CrdsData::RestartHeaviestFork(_)
        | CrdsData::RestartLastVotedForkSlots(_)
        | CrdsData::Vote(_, _) => retain_if_staked(),
        // Legacy unstaked nodes can still send EpochSlots
        CrdsData::EpochSlots(_, _) => match direction {
            // always store EpochSlots if we have received them
            // to avoid getting them again in PullResponses
            GossipFilterDirection::Ingress => true,
            // only forward EpochSlots if the origin is staked
            GossipFilterDirection::Egress => retain_if_staked(),
        },
        // Deprecated messages we still see in the mainnet
        CrdsData::Version(_) => match direction {
            GossipFilterDirection::Ingress => true,
            GossipFilterDirection::Egress => false,
        },
        CrdsData::NodeInstance(_) => match direction {
            // Store NodeInstance to avoid getting them again
            // in PullResponses
            GossipFilterDirection::Ingress => true,
            GossipFilterDirection::Egress => false,
        },
        // Fully deprecated messages
        CrdsData::LegacySnapshotHashes(_) => false,
        CrdsData::LegacyVersion(_) => false,
        CrdsData::AccountsHashes(_) => false,
    }
}
