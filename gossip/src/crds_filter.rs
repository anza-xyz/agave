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

/// Returns false if the CRDS value should be discarded.
/// `direction` controls whether we are looking at
/// incoming packet (via Push or PullResponse) or
/// we are about to make a packet.
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
        CrdsData::DuplicateShred(_, _) | CrdsData::EpochSlots(_, _) | CrdsData::Vote(_, _) => false,
        CrdsData::LowestSlot(0, _) => match direction {
            Ingress => retain_if_staked(),
            EgressPush | EgressPullResponse => false,
        },
        // Fully deprecated messages
        CrdsData::AccountsHashes(_) => false,
        CrdsData::LegacyContactInfo(_) => false,
        CrdsData::LegacySnapshotHashes(_) => false,
        CrdsData::LegacyVersion(_) => false,
        CrdsData::LowestSlot(1.., _) => false,
        CrdsData::NodeInstance(_) => false,
        CrdsData::RestartHeaviestFork(_) | CrdsData::RestartLastVotedForkSlots(_) => {
            match direction {
                Ingress => true,
                EgressPush | EgressPullResponse => false,
            }
        }
        CrdsData::Version(_) => false,
    }
}
