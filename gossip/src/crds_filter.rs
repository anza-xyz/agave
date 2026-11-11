use crate::{crds_data::CrdsData, crds_value::CrdsValue};

/// Returns false if the CRDS value should be discarded.
#[inline]
#[must_use]
pub(crate) fn should_retain_crds_value(value: &CrdsValue) -> bool {
    match value.data() {
        // All nodes can send ContactInfo
        CrdsData::ContactInfo(_) => true,
        // Unstaked nodes can still serve snapshots.
        CrdsData::SnapshotHashes(_) => true,
        CrdsData::DuplicateShred(_, _) | CrdsData::EpochSlots(_) => false,
        // Fully deprecated messages
        CrdsData::AccountsHashes(_) | CrdsData::Vote(_) => false,
        CrdsData::LegacyContactInfo(_) => false,
        CrdsData::LegacySnapshotHashes(_) => false,
        CrdsData::LegacyVersion(_) => false,
        CrdsData::LowestSlot(_) => false,
        CrdsData::NodeInstance(_) => false,
        CrdsData::RestartHeaviestFork(_) | CrdsData::RestartLastVotedForkSlots(_) => false,
        CrdsData::Version(_) => false,
    }
}
