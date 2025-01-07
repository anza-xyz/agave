//! Runtime feature set.

use {
    crate::proto::FeatureSet as ProtoFeatureSet,
    lazy_static::lazy_static,
    solana_feature_set::{FeatureSet, FEATURE_NAMES},
    solana_keccak_hasher::Hasher,
    solana_pubkey::Pubkey,
    std::collections::HashMap,
};

// Omit "test features" (they have the same u64 ID).
static OMITTED_FEATURES: &[Pubkey] = &[
    solana_feature_set::disable_sbpf_v0_execution::id(),
    solana_feature_set::reenable_sbpf_v0_execution::id(),
];

const fn feature_u64(feature: &Pubkey) -> u64 {
    let feature_id = feature.to_bytes();
    feature_id[0] as u64
        | (feature_id[1] as u64) << 8
        | (feature_id[2] as u64) << 16
        | (feature_id[3] as u64) << 24
        | (feature_id[4] as u64) << 32
        | (feature_id[5] as u64) << 40
        | (feature_id[6] as u64) << 48
        | (feature_id[7] as u64) << 56
}

lazy_static! {
    static ref INDEXED_FEATURES: HashMap<u64, Pubkey> = {
        FEATURE_NAMES
            .iter()
            .filter_map(|(pubkey, _)| {
                (!OMITTED_FEATURES.contains(pubkey)).then_some((feature_u64(pubkey), *pubkey))
            })
            .collect()
    };
}

lazy_static! {
    static ref INDEXED_FEATURE_U64S: HashMap<Pubkey, u64> = {
        FEATURE_NAMES
            .iter()
            .filter_map(|(pubkey, _)| {
                (!OMITTED_FEATURES.contains(pubkey)).then_some((*pubkey, feature_u64(pubkey)))
            })
            .collect()
    };
}

impl From<ProtoFeatureSet> for FeatureSet {
    fn from(value: ProtoFeatureSet) -> Self {
        let mut feature_set = FeatureSet::default();
        for id in &value.features {
            if let Some(pubkey) = INDEXED_FEATURES.get(id) {
                feature_set.activate(pubkey, 0);
            }
        }
        feature_set
    }
}

impl From<FeatureSet> for ProtoFeatureSet {
    fn from(value: FeatureSet) -> Self {
        let features = value
            .active
            .keys()
            .filter_map(|id| INDEXED_FEATURE_U64S.get(id).copied())
            .collect();

        Self { features }
    }
}

pub(crate) fn hash_proto_feature_set(hasher: &mut Hasher, feature_set: &ProtoFeatureSet) {
    let mut features = feature_set.features.clone();
    features.sort();
    for f in &features {
        hasher.hash(&f.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_to_from_feature_set(feature_set: FeatureSet) {
        let proto = ProtoFeatureSet::from(feature_set.clone());
        let fs = FeatureSet::from(proto);

        // Ordering can't be preserved for `HashSet` or `HashMap`.
        assert_eq!(feature_set.active.keys().len(), fs.active.keys().len());
        assert_eq!(feature_set.inactive.len(), fs.inactive.len());
        for (feature, _) in feature_set.active.iter() {
            assert!(fs.active.contains_key(feature));
        }
        for feature in feature_set.inactive.iter() {
            assert!(fs.inactive.contains(feature));
        }
    }

    #[test]
    fn test_conversion_feature_set() {
        let feature_set = FeatureSet::default();
        test_to_from_feature_set(feature_set);

        // Have to remove the omitted features (collisions).
        let mut feature_set = FeatureSet::all_enabled();
        feature_set.deactivate(&OMITTED_FEATURES[0]);
        feature_set.deactivate(&OMITTED_FEATURES[1]);
        test_to_from_feature_set(feature_set);

        // Activate some arbitrary features.
        let mut feature_set = FeatureSet::default();
        feature_set
            .inactive
            .clone()
            .into_iter()
            .enumerate()
            .for_each(|(i, feature)| {
                if i % 3 == 0 && !OMITTED_FEATURES.contains(&feature) {
                    feature_set.activate(&feature, 0);
                }
            });
        test_to_from_feature_set(feature_set);
    }
}
