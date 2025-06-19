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
pub(crate) const MIN_STAKE_FOR_GOSSIP: u64 = solana_native_token::LAMPORTS_PER_SOL;
/// Minimum stake required for a node to bypass the initial ping check when joining gossip.
pub(crate) const MIN_STAKE_TO_SKIP_PING: u64 = 1000 * solana_native_token::LAMPORTS_PER_SOL;

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
        // All nodes can send ContactInfo
        CrdsData::ContactInfo(_) => true,
        // Unstaked nodes can still serve snapshots.
        CrdsData::SnapshotHashes(_) => true,
        // Consensus related messages only allowed for staked nodes
        CrdsData::DuplicateShred(_, _)
        | CrdsData::LowestSlot(0, _)
        | CrdsData::RestartHeaviestFork(_)
        | CrdsData::RestartLastVotedForkSlots(_) => retain_if_staked(),
        // Unstaked nodes can technically send EpochSlots, but we do not want them
        // eating gossip bandwidth
        CrdsData::EpochSlots(_, _) => match direction {
            // always store if we have received them
            // to avoid getting them again in PullResponses
            Ingress => true,
            // only forward if the origin is staked
            EgressPush | EgressPullResponse => retain_if_staked(),
        },
        CrdsData::Vote(_, _) => match direction {
            Ingress | EgressPush => true,
            EgressPullResponse => retain_if_staked(),
        },
        // Fully deprecated messages
        CrdsData::AccountsHashes(_) => false,
        CrdsData::LegacyContactInfo(_) => false,
        CrdsData::LegacySnapshotHashes(_) => false,
        CrdsData::LegacyVersion(_) => false,
        CrdsData::LowestSlot(1.., _) => false,
        CrdsData::NodeInstance(_) => false,
        CrdsData::Version(_) => false,
    }
}
